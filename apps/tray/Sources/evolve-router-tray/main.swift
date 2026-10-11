// EvolveRouter 菜单栏伴侣（SwiftUI MenuBarExtra 原生实现，.menu 样式）
// 点菜单栏图标弹原生菜单：状态行、启停控制、开机自启、开源地址、退出。
// 数据源：本地网关（App 级 3s 轮询常驻，EVOLVE_URL env 可覆盖）。

import SwiftUI

enum Config {
    static let baseURL = ProcessInfo.processInfo.environment["EVOLVE_URL"] ?? "http://127.0.0.1:8787"
    static let githubURL = URL(string: "https://github.com/holny/evolve-router")!
    static let gatewayLabel = "ai.evolve-router.gateway"
    static let trayLabel = "ai.evolve-router.tray"
}

// MARK: - 状态 + 轮询

@MainActor
final class AppState: ObservableObject {
    @Published var healthy = true
    @Published var requests = 0
    @Published var successRate = 0
    @Published var tokensK = "-"
    @Published var plans: [(String, Int)] = []
    @Published var recent: [(String, Int, Int)] = []   // (model, status, total_ms)

    func poll() async {
        if let resp = try? await get("/healthz") {
            healthy = resp.0 == 200
        } else {
            healthy = false
        }
        if case (200, let data)? = try? await get("/api/stats"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            var reqs = 0, ok = 0, toks = 0
            for (_, s) in obj {
                guard let s = s as? [String: Any] else { continue }
                reqs += (s["requests"] as? Int) ?? Int((s["requests"] as? Double) ?? 0)
                ok += (s["success"] as? Int) ?? Int((s["success"] as? Double) ?? 0)
                toks += (s["prompt_tokens"] as? Int) ?? Int((s["prompt_tokens"] as? Double) ?? 0)
                toks += (s["completion_tokens"] as? Int) ?? Int((s["completion_tokens"] as? Double) ?? 0)
            }
            requests = reqs
            successRate = reqs > 0 ? Int(round(100.0 * Double(ok) / Double(reqs))) : 0
            tokensK = toks >= 1_000_000 ? String(format: "%.1fM", Double(toks)/1e6)
                    : toks >= 1_000 ? String(format: "%.0fk", Double(toks)/1e3) : "\(toks)"
        }
        if case (200, let data)? = try? await get("/api/providers"),
           let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
            var byPlan: [String: Int] = [:]
            for p in arr {
                let kind = (p["plan_kind"] as? String) ?? "-"
                let short = ["coding": "CP", "go": "GO", "agent": "AP", "token": "TOK"][kind]
                guard let short else { continue }
                guard let windows = p["windows"] as? [String: Any],
                      let allow = (windows["5h"] as? Double) ?? (windows["5h"] as? Int).map(Double.init),
                      allow > 0 else { continue }
                let used = (p["used_5h"] as? Double) ?? (p["used_5h"] as? Int).map(Double.init) ?? 0
                byPlan[short] = max(byPlan[short] ?? 0, Int(round(100 * used / allow)))
            }
            plans = byPlan.sorted { $0.value > $1.value }
        }
        if case (200, let data)? = try? await get("/api/events?limit=8"),
           let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
            var rows: [(String, Int, Int)] = []
            for ev in arr where (ev["kind"] as? String) == "chat" {
                let model = (ev["chosen"] as? String) ?? "-"
                let status = (ev["status"] as? Int) ?? Int((ev["status"] as? Double) ?? 0)
                let ms = (ev["total_ms"] as? Int) ?? Int((ev["total_ms"] as? Double) ?? 0)
                rows.append((model, status, ms))
                if rows.count >= 5 { break }
            }
            recent = rows
        }
    }

    private func get(_ path: String) async throws -> (Int, Data) {
        let (data, resp) = try await URLSession.shared.data(from: URL(string: Config.baseURL + path)!)
        return ((resp as? HTTPURLResponse)?.statusCode ?? 0, data)
    }
}

// MARK: - App（.menu 原生菜单）

@main
struct EvolveRouterTrayApp: App {
    @StateObject private var state = AppState()
    // 开机自启：launchctl enable/disable（持久）；默认开
    @State private var autostart: Bool

    init() {
        let saved = UserDefaults.standard.object(forKey: "ev-autostart")
        _autostart = State(initialValue: saved == nil ? true : UserDefaults.standard.bool(forKey: "ev-autostart"))
        applyAutostart(_autostart.wrappedValue)
        // 轮询挂 App 级常驻 Timer——不随菜单开闭暂停
        let s = _state.wrappedValue
        let t = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in
            Task { @MainActor in await s.poll() }
        }
        RunLoop.main.add(t, forMode: .common)
    }

    var body: some Scene {
        MenuBarExtra {
            // ── 状态区（灰字） ──
            Text(state.healthy ? "● 网关运行中 · 8787" : "● 网关已停止")
            Text("请求 \(state.requests) · 成功率 \(state.successRate)% · \(state.tokensK) tok")
            if !state.plans.isEmpty {
                Text("方案 " + state.plans.map { "\($0.0) \($0.1)%" }.joined(separator: " · "))
            }
            ForEach(Array(state.recent.enumerated()), id: \.offset) { _, row in
                Text("\(row.1 == 200 ? "✓" : "✗") \(row.0) · \(row.2)ms")
            }
            Divider()
            // ── 控制 ──
            Button(state.healthy ? "停止 EvolveRouter" : "启动 EvolveRouter") {
                Task { await toggleGateway() }
            }
            Toggle("开机自动启动", isOn: Binding(
                get: { autostart },
                set: { on in
                    autostart = on
                    applyAutostart(on)
                }
            ))
            Divider()
            Button("打开完整面板") { NSWorkspace.shared.open(URL(string: Config.baseURL)!) }
            Button("开源地址") { NSWorkspace.shared.open(Config.githubURL) }
            Divider()
            Button("彻底退出…") { confirmExit() }
        } label: {
            HStack(spacing: 3) {
                Image(systemName: state.healthy
                      ? "arrow.triangle.branch"
                      : "exclamationmark.triangle.fill")
                Text("Router")
            }
        }
        .menuBarExtraStyle(.menu)
    }

    // ── 网关控制（launchctl，状态由 3s 轮询自动反馈） ──

    private func toggleGateway() async {
        await state.poll() // 先取当前真实状态
        let uid = getuid()
        if state.healthy {
            shell("launchctl bootout gui/\(uid)/\(Config.gatewayLabel)")
        } else {
            shell("launchctl bootstrap gui/\(uid)/\(Config.gatewayLabel)")
        }
        await state.poll()
    }

    private func applyAutostart(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: "ev-autostart")
        let uid = getuid()
        shell(on ? "launchctl enable gui/\(uid)/\(Config.trayLabel)"
                 : "launchctl disable gui/\(uid)/\(Config.trayLabel)")
        shell(on ? "launchctl enable gui/\(uid)/\(Config.gatewayLabel)"
                 : "launchctl disable gui/\(uid)/\(Config.gatewayLabel)")
    }

    private func shell(_ cmd: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/zsh")
        p.arguments = ["-c", cmd]
        try? p.run()
    }

    // 彻底退出：引擎 + 托盘全停；同步 NSAlert 告知重开方式（menu 上下文
    // 不支持 SwiftUI confirmationDialog）
    private func confirmExit() {
        let a = NSAlert()
        a.messageText = "确定彻底退出 EvolveRouter 吗？"
        a.informativeText = "网关引擎与菜单栏将全部关闭。\n重新打开：双击 应用程序 → EvolveRouter，或终端执行 open -a EvolveRouter"
        a.addButton(withTitle: "退出")
        a.addButton(withTitle: "取消")
        if a.runModal() == .alertFirstButtonReturn {
            let uid = getuid()
            shell("launchctl bootout gui/\(uid)/\(Config.gatewayLabel)")
            // 分离延迟收掉托盘自身（app 退出后执行）
            shell("(sleep 1; launchctl bootout gui/\(uid)/\(Config.trayLabel)) >/dev/null 2>&1 &")
            NSApp.terminate(nil)
        }
    }
}
