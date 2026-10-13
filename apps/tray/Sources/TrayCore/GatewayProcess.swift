import Foundation

/// 网关子进程管理：spawn Contents/MacOS/evo-router serve，SIGTERM 优雅停机，
/// 3s 后升级 SIGKILL。外部实例探测：已 healthy 时不 spawn，避免双网关竞态。
@MainActor
public final class GatewayProcess: ObservableObject {
    public init() {}
    public static let shared = GatewayProcess()

    /// true = 由本 app 启动并管理；false = 探测到外部实例（按钮只读）
    @Published public private(set) var owned = false
    @Published public private(set) var starting = false

    private var child: Process?

    public var isChildAlive: Bool {
        guard let child, child.isRunning else { return false }
        return true
    }

    /// 幂等启动：外部健康实例（1s×5 连续健康）→ 判为外部只读；一旦出现非健康 → 可接管并 spawn。
    public func ensureRunning() async {
        var persistent = true
        for _ in 0..<5 {
            guard await networkHealthy() else { persistent = false; break }
            try? await Task.sleep(nanoseconds: 1_000_000_000)
        }
        if persistent { owned = false; return }
        if child?.isRunning != true {
            await spawn()
        }
    }

    private func spawn() async -> Bool {
        starting = true
        defer { starting = false }
        guard FileManager.default.isExecutableFile(atPath: Config.gatewayURL.path) else { return false }
        let p = Process()
        p.executableURL = Config.gatewayURL
        p.arguments = ["serve"]
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
