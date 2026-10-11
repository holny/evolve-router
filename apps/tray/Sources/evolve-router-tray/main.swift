// EvolveRouter 菜单栏伴侣（SwiftUI MenuBarExtra 原生实现，.window 面板样式）
// 图标实时反映网关健康；点击弹出迷你面板：状态、方案 5h 用量、最近决策、
// 智能启停控制、开机自启、开源地址与完整面板入口。
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
    @Published var lastSync: Date?

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

// MARK: - 迷你面板

struct PanelView: View {
    @ObservedObject var state: AppState
    @State private var autostart: Bool = UserDefaults.standard.object(forKey: "ev-autostart") == nil
        ? true
        : UserDefaults.standard.bool(forKey: "ev-autostart")
    @State private var showExitConfirm = false

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack {
                Circle().fill(state.healthy ? Color.green : Color.red).frame(width: 8, height: 8)
                Text(state.healthy ? "网关运行中 · 8787" : "网关已停止")
                    .font(.system(size: 12, weight: .semibold))
                Spacer()
                if let t = state.lastSync {
                    Text(t, style: .time).font(.system(size: 10)).foregroundStyle(.secondary)
                }
            }
            HStack(spacing: 14) {
                stat("请求", "\(state.requests)")
                stat("成功率", "\(state.successRate)%")
                stat("Tokens", state.tokensK)
            }
            if !state.plans.isEmpty {
                section("方案 5h 用量") {
                    ForEach(state.plans, id: \.0) { name, pct in
                        HStack {
                            Text(name).font(.system(size: 11, weight: .medium)).frame(width: 36, alignment: .leading)
                            ProgressBar(pct: pct)
                            Text("\(pct)%").font(.system(size: 11, design: .monospaced)).frame(width: 34, alignment: .trailing)
                        }
                    }
                }
            }
            if !state.recent.isEmpty {
                section("最近决策") {
                    ForEach(Array(state.recent.enumerated()), id: \.offset) { _, row in
                        HStack(spacing: 6) {
                            Circle().fill(row.1 == 200 ? Color.green : Color.red).frame(width: 5, height: 5)
                            Text(row.0).font(.system(size: 10.5)).lineLimit(1)
                            Spacer()
                            Text(row.1 == 200 ? "\(row.2)ms" : "fail")
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundStyle(row.1 == 200 ? .secondary : Color.red)
                        }
                    }
                }
            }
            Divider()
            // 控制区（小尺寸，不喧宾夺主）
            Toggle(isOn: $autostart) {
                Text("开机自动启动").font(.system(size: 11))
            }
            .controlSize(.small)
            .onChange(of: autostart) { on in
                UserDefaults.standard.set(on, forKey: "ev-autostart")
                applyAutostart(on)
            }
            Button {
                Task { await toggleGateway() }
            } label: {
                Label(state.healthy ? "停止网关" : "启动网关",
                      systemImage: state.healthy ? "stop.circle" : "play.circle")
                    .frame(maxWidth: .infinity)
            }
            .controlSize(.small)
            HStack(spacing: 8) {
                Link(destination: Config.githubURL) {
                    Label("开源地址", systemImage: "link")
                        .font(.system(size: 11))
                }
                Spacer()
                Button {
                    NSWorkspace.shared.open(URL(string: Config.baseURL)!)
                } label: {
                    Label("完整面板", systemImage: "gauge.with.needle")
                        .font(.system(size: 11))
                }
                Button(role: .destructive) {
                    showExitConfirm = true
                } label: {
                    Label("退出", systemImage: "power")
                        .font(.system(size: 11))
                }
            }
        }
        .padding(12)
        .frame(width: 300)
        .task { await state.poll() }
        .confirmationDialog("确定彻底退出 EvolveRouter 吗？", isPresented: $showExitConfirm, titleVisibility: .visible) {
            Button("退出（网关与菜单栏将全部关闭）", role: .destructive) {
                shell("launchctl bootout gui/\(getuid())/\(Config.gatewayLabel)")
                // 分离延迟收掉托盘自身（app 退出后执行）
                shell("(sleep 1; launchctl bootout gui/\(getuid())/\(Config.trayLabel)) >/dev/null 2>&1 &")
                NSApp.terminate(nil)
            }
            Button("取消", role: .cancel) {}
        } message: {
            Text("重新打开方式：双击 应用程序 → EvolveRouter，或在终端执行 open -a EvolveRouter")
        }
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 14, weight: .semibold, design: .monospaced))
        }
    }

    private func section<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            content()
        }
    }

    // ── 控制（launchctl） ──

    private func toggleGateway() async {
        await state.poll() // 先取当前真实状态
        let uid = getuid()
        if state.healthy {
            shell("launchctl bootout gui/\(uid)/\(Config.gatewayLabel)")
        } else {
            shell("launchctl bootstrap gui/\(uid)/\(Config.gatewayLabel)")
        }
        await state.poll() // 立即反馈
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
        Task.detached {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/zsh")
            p.arguments = ["-c", cmd]
            try? p.run()
            p.waitUntilExit()
        }
    }
}

struct ProgressBar: View {
    let pct: Int
    var body: some View {
        GeometryReader { geo in
            let w = geo.size.width
            ZStack(alignment: .leading) {
                Capsule().fill(.quaternary)
                Capsule().fill(pct >= 85 ? AnyShapeStyle(Color.orange) : AnyShapeStyle(Color.accentColor))
                    .frame(width: max(3, w * CGFloat(min(100, max(0, pct))) / 100))
            }
        }.frame(height: 6)
    }
}

// MARK: - App（App 级轮询常驻）

@main
struct EvolveRouterTrayApp: App {
    @StateObject private var state = AppState()

    init() {
        // 轮询必须挂在 App 级（常驻）——挂在面板内容的 .task 上会在
        // 面板关闭时被暂停，状态冻结导致按钮卡死（实测根因）
        let s = _state.wrappedValue
        let t = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in
            Task { @MainActor in await s.poll() }
        }
        RunLoop.main.add(t, forMode: .common)
    }

    var body: some Scene {
        MenuBarExtra {
            PanelView(state: state)
        } label: {
            HStack(spacing: 3) {
                Image(systemName: state.healthy
                      ? "arrow.triangle.branch"
                      : "exclamationmark.triangle.fill")
                Text("Router")
            }
        }
        .menuBarExtraStyle(.window)
    }
}
