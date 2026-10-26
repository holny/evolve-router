import Foundation

/// 子进程日志写入器（**非隔离**，内部串行队列自持句柄）——GatewayProcess 是
/// @MainActor，日志操作若挂在其方法上、从队列 sync 回 MainActor 会死锁
/// （2026-10-26 实证：spawn 卡死在 logQueue.sync，网关永不拉起）。
/// 所有调用方（MainActor / readabilityHandler 后台线程）都安全。
final class ChildLogWriter {
    private let queue = DispatchQueue(label: "ai.evolve-router.gateway-log")
    private var handle: FileHandle?
    private let url: URL
    private let rotateBytes: UInt64

    init(url: URL, rotateBytes: UInt64 = 32 * 1024 * 1024) {
        self.url = url
        self.rotateBytes = rotateBytes
    }

    /// 打开（追加模式）；文件不存在则创建
    func open() {
        queue.sync {
            if !FileManager.default.fileExists(atPath: url.path) {
                FileManager.default.createFile(atPath: url.path, contents: nil)
            }
            handle = FileHandle(forWritingAtPath: url.path)
            handle?.seekToEndOfFile()
        }
    }

    /// 超过 rotateBytes → 关句柄 → rename 为 .1（覆盖旧 .1，只保留最近一份）
    /// → 重开新文件继续写。子进程无需感知（pipe 由调用方消费）
    func rotateIfNeeded() {
        queue.sync {
            let size = (try? FileManager.default.attributesOfItem(atPath: url.path)[.size])
                as? UInt64 ?? 0
            guard size >= rotateBytes else { return }
            handle?.closeFile()
            handle = nil
            let rotated = url.path + ".1"
            if FileManager.default.fileExists(atPath: rotated) {
                try? FileManager.default.removeItem(atPath: rotated)
            }
            try? FileManager.default.moveItem(atPath: url.path, toPath: rotated)
            if !FileManager.default.fileExists(atPath: url.path) {
                FileManager.default.createFile(atPath: url.path, contents: nil)
            }
            handle = FileHandle(forWritingAtPath: url.path)
            handle?.seekToEndOfFile()
        }
    }

    func write(_ data: Data) {
        queue.async { [weak self] in
            self?.handle?.write(data)
        }
    }

    func close() {
        queue.sync {
            handle?.closeFile()
            handle = nil
        }
    }
}

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
    // 子进程日志：pipe → ChildLogWriter（非隔离、内部串行队列自持有句柄），
    // 轮转 = 关句柄 → rename 为 .1 → 重开——rename 式轮转只有持句柄者能做对
    // （外部 launchd/tray 持句柄时 rename 会把新写入引进 .1）。实测事故：
    // 无轮转的 gateway-child.log 涨到 807MB（2026-10-26）。
    // ⚠ writer 必须非隔离：MainActor 上 sync 进它的队列会死锁（互相等）
    private var logWriter: ChildLogWriter?
    private var logPipe: Pipe?
    private var logRotateTimer: Timer?

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
    /// ⚠ spawnInFlight 必须在健康探测**之前**置位（2026-10-19 实证：两个并发
    /// ensureRunning 都穿过旧检查 → 双 spawn → 绑定失败的那个把 child 覆盖成
    /// 死进程 → 自愈误判"外部实例" → Restart/Stop 从此静默失效）。
    public func ensureRunning() async {
        if userStopped { return }
        if owned, child?.isRunning == true { return }
        if spawnInFlight { return }
        spawnInFlight = true
        defer { spawnInFlight = false }
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
        // 日志：启动先轮转一次（上一进程留下的超大日志），pipe 接收子进程
        // stderr/stdout，由 ChildLogWriter 串行写入并持有句柄（轮转的唯一
        // 正确位置）
        let writer = ChildLogWriter(url: Config.gatewayLogURL)
        writer.rotateIfNeeded()
        writer.open()
        logWriter = writer
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        logPipe = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak writer] fh in
            let data = fh.availableData
            guard !data.isEmpty else {
                fh.readabilityHandler = nil
                return
            }
            writer?.write(data)
        }
        startLogRotateTimer(writer)
        do {
            try p.run()
        } catch {
            tracingFallback("spawn failed: \(error)")
            stopLogPipeline()
            return false
        }
        // 防双开兜底：已有存活子进程时立刻终止新 spawn 的副本（并发窗口残留）
        if let old = child, old.isRunning {
            tracingFallback("duplicate spawn suppressed (existing child alive)")
            p.terminate()
            return true
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

    // MARK: - 子进程日志轮转

    private func startLogRotateTimer(_ writer: ChildLogWriter) {
        logRotateTimer?.invalidate()
        // 主 runloop 触发；rotateIfNeeded 内部 sync 进 writer 队列（writer
        // 非隔离、不回主线程 → 无死锁）
        logRotateTimer = Timer.scheduledTimer(withTimeInterval: 600, repeats: true) { _ in
            writer.rotateIfNeeded()
        }
    }

    private func stopLogPipeline() {
        logPipe?.fileHandleForReading.readabilityHandler = nil
        logPipe = nil
        logRotateTimer?.invalidate()
        logRotateTimer = nil
    }

    /// 优雅停止：SIGTERM → 等 3s → SIGKILL；返回是否由本 app 管理并已停。
    public func stop() async {
        userStopped = true
        stopLogPipeline()
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
    /// `evo-router serve` 发 SIGTERM（等 2s 排空，与网关自身的 drain 一致），
    /// 仍存活才 SIGKILL。找不到/已退出都是 no-op。
    /// ⚠ pgrep pattern 必须是完整可辨识串：`-f "evo-router serve"` 是子串匹配，
    /// 会误杀 `tail -f .../evo-router serve.log` 之类无关进程（review 2026-10-23；
    /// 与进程清理纪律一致——先 pgrep 只看目标，再动手）
    /// ⚠ 必须 async：此前是同步 usleep 循环，@MainActor 下最多阻塞主线程 8s
    /// （2026-10-19 实证：点 Quit 全程冻结）。
    public func stopGatewayWithSigterm() async {
        userStopped = true
        let out = Process()
        out.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        out.arguments = ["-f", "EvolveRouter\\.app/Contents/MacOS/evo-router serve|(^|/)evo-router serve$"]
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
            let targets = pids.filter { $0 != ProcessInfo.processInfo.processIdentifier }
            for pid in targets {
                kill(pid, SIGTERM)
            }
            guard !targets.isEmpty else { return }
            // 给排空留时间（2s：拖太久反而像"退不出"；超时 SIGKILL 兜底）
            for _ in 0..<20 {
                try? await Task.sleep(nanoseconds: 100_000_000)
                var alive = false
                for pid in targets {
                    if kill(pid, 0) == 0 { alive = true }
                }
                if !alive { return }
            }
            for pid in targets {
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
