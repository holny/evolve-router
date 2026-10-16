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
        let dir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".evo-router", isDirectory: true)
            .appendingPathComponent("logs", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("gateway-child.log")
    }()

    static var gatewayURL: URL {
        URL(fileURLWithPath: siblingGatewayURL.path)
    }
}
