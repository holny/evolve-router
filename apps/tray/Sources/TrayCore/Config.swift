import Foundation

public enum Config {
    public static let baseURL = ProcessInfo.processInfo.environment["EVO_ROUTER_URL"]
        ?? ProcessInfo.processInfo.environment["EVOLVE_URL"]   // 旧名兜底
        ?? "http://127.0.0.1:8787"
    public static let githubURL = URL(string: "https://github.com/holny/evolve-router")!
    public static let trayLabel = "ai.evolve-router.tray"

    /// 同 bundle 的 Rust 网关 helper：Contents/MacOS/evo-router
    static let siblingGatewayURL: URL = (
        Bundle.main.executableURL?.deletingLastPathComponent()
            .appendingPathComponent("evo-router")
    ) ?? URL(fileURLWithPath: "/Applications/EvolveRouter.app/Contents/MacOS/evo-router")

    /// 网关子进程日志（追加）：归类到 ~/.evo-router/logs/
    static let gatewayLogURL: URL = {
        let logs = dataDirURL.appendingPathComponent("logs", isDirectory: true)
        // 必须把 logs/ 一起建出来：首次安装时 CreateFile 因父目录不存在会失败，
        // 子进程 stdout/stderr 不落盘，而 dm_degraded_hint 正指向这个文件
        try? FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        return logs.appendingPathComponent("gateway-child.log")
    }()

    /// 数据目录（~/.evo-router）——同时作为网关子进程的 cwd（非 TCC 保护目录）
    static let dataDirURL: URL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".evo-router", isDirectory: true)

    static var gatewayURL: URL {
        URL(fileURLWithPath: siblingGatewayURL.path)
    }
}
