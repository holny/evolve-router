import Foundation

/// 网关子进程管理：spawn Contents/MacOS/evo-router serve，SIGTERM 优雅停机，
/// 3s 后升级 SIGKILL。外部实例探测：已 healthy 时不 spawn，避免双网关竞态。
@MainActor
public final class GatewayProcess: ObservableObject {
    public init() {}
    public static let shared = GatewayProcess()

    /// true = 由本 app 启动并管理；false = 探测到外部实例（按钮只读）
    @Published public private(set) var owned = false
    /// 用户显式 Stop 过（面板按钮）→ 周期自愈必须让路，否则 Stop 一秒内
    /// 就被重新拉起（实测 bug 2026-10-17）。Start/成功 spawn 时清除
    @Published public private(set) var userStopped = false
    @Published public private(set) var starting = false

    private var child: Process?
    private var spawnInFlight = false
    private var externalUntil: Date?

    /// Start 按钮清除用户 Stop 意图（自愈恢复）
    public func clearUserStopped() {
        userStopped = false
    }

    public var isChildAlive: Bool {
        guard let child, child.isRunning else { return false }
        return true
    }

    /// 幂等启动：外部健康实例（1s×3 连续健康）→ 30s 内判为外部只读；
    /// 一旦出现非健康 → spawnInFlight 抢占式接管 spawn。pollLive 每秒驱动、
    /// 无节流死锁（旧 lastAttempt 被每秒调用刷新，spawn 永不触发——实证修复）。
    public func ensureRunning() async {
        if userStopped { return }
        if owned, child?.isRunning == true { return }
        if spawnInFlight { return }
        if let eu = externalUntil, Date() < eu { return }
        var healthyStreak = 0
        for _ in 0..<3 {
            if await networkHealthy() { healthyStreak += 1 } else { healthyStreak = 0; break }
            try? await Task.sleep(nanoseconds: 1_000_000_000)
        }
        if healthyStreak >= 3 {
            owned = false
            externalUntil = Date().addingTimeInterval(30)
            return
        }
        spawnInFlight = true
        defer { spawnInFlight = false }
        await spawn()
    }

    private func spawn() async -> Bool {
        starting = true
        defer { starting = false }
        guard FileManager.default.isExecutableFile(atPath: Config.gatewayURL.path) else { return false }
        let p = Process()
        p.executableURL = Config.gatewayURL
        p.arguments = ["serve"]
        // 子进程 cwd 固定为数据目录（~/.evo-router，非 TCC 保护目录）：
        // 若继承到 ~/Documents / ~/Desktop 之类的工作目录，网关探测"相对 cwd 的
        // 候选配置名"就会让 macOS 反复弹"想访问文稿文件夹中的文件"（实测）
        p.currentDirectoryURL = Config.dataDirURL
        let log = FileHandle(forWritingAtPath: Config.gatewayLogURL.path) ?? {
            FileManager.default.createFile(atPath: Config.gatewayLogURL.path, contents: nil)
            return FileHandle(forWritingAtPath: Config.gatewayLogURL.path)
        }()
        if let log {
            _ = try? FileManager.default
                .attributesOfItem(atPath: Config.gatewayLogURL.path)
            log.seekToEndOfFile()
            p.standardOutput = log
            p.standardError = log
        }
        do {
            try p.run()
        } catch {
            tracingFallback("spawn failed: \(error)")
            return false
        }
        child = p
        owned = true
        userStopped = false
        // 等网关就绪（最长 ~6s：绑定 + 目录初始化）
        for _ in 0..<12 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if !p.isRunning { break }
            if await networkHealthy() { return true }
        }
        return await networkHealthy()
    }

    private func tracingFallback(_ msg: String) {
        // 诊断信息进 gateway-child.log（与子进程日志同文件，避免散落探针文件）
        if let fh = FileHandle(forWritingAtPath: Config.gatewayLogURL.path) {
            fh.seekToEndOfFile()
            fh.write(Data("[shell] \(msg)\n".utf8))
            try? fh.close()
        }
    }

    /// 优雅停止：SIGTERM → 等 3s → SIGKILL；返回是否由本 app 管理并已停。
    public func stop() async {
        userStopped = true
        guard owned, child?.isRunning == true, let proc = child else { return }
        proc.terminate()
        for _ in 0..<30 {
            try? await Task.sleep(nanoseconds: 100_000_000)
            if !proc.isRunning { break }
        }
        if proc.isRunning {
            kill(proc.processIdentifier, SIGKILL)
            proc.waitUntilExit()
        }
        child = nil
        owned = false
        userStopped = true
    }

    /// Quit 时的兜底：本 app 实例可能不是网关的父进程（launchd RunAtLoad 直启
    /// 面板、外部脚本起网关），`owned=false` 时 stop() 是 no-op。按进程名找到
    /// `evo-router serve` 发 SIGTERM（等 8s 排空，与网关自身的 drain 一致），
    /// 仍存活才 SIGKILL。找不到/已退出都是 no-op。
    public func stopGatewayWithSigterm() {
        userStopped = true
        let out = Process()
        out.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        out.arguments = ["-f", "evo-router serve"]
        let pipe = Pipe()
        out.standardOutput = pipe
        out.standardError = Pipe()
        do {
            try out.run()
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            out.waitUntilExit()
            let pids = String(decoding: data, as: UTF8.self)
                .split(separator: "\n")
                .compactMap { Int32($0.trimmingCharacters(in: .whitespaces)) }
            for pid in pids where pid != ProcessInfo.processInfo.processIdentifier {
                kill(pid, SIGTERM)
            }
            // 给排空留时间（60s 上限之内先等 8s，拖太久反而像"退不出"）
            for _ in 0..<80 {
                usleep(100_000)
                var alive = false
                for pid in pids where pid != ProcessInfo.processInfo.processIdentifier {
                    if kill(pid, 0) == 0 { alive = true }
                }
                if !alive { return }
            }
            for pid in pids where pid != ProcessInfo.processInfo.processIdentifier {
                kill(pid, SIGKILL)
            }
        } catch {
            // pgrep 不可用等极端情况：忽略（NSApp.terminate 照常走）
        }
    }

    public func networkHealthy() async -> Bool {
        var req = URLRequest(url: URL(string: Config.baseURL + "/healthz")!)
        req.timeoutInterval = 2
        if let (_, resp) = try? await URLSession.shared.data(for: req) {
            return (resp as? HTTPURLResponse)?.statusCode == 200
        }
        return false
    }
}
