import AppKit
import SwiftUI
import TrayCore

@MainActor
enum PanelPreview {
    static func run(_ scheme: ColorScheme) {
        let state = AppState()
        state.healthy = true
        state.inflight = 2
        state.requests = 1691
        state.successRate = 99
        state.tokensK = "780.4k"
        state.version = "0.1.7"
        state.lastSync = Date()
        // 文档截图用的稳定种子（EV_PREVIEW_SEED=1）：覆盖五类标签的各种组合，
        // 不依赖本机实时流量（否则 README 图会随环境漂移）
        state.recent = [
            RecentDecision(model: "opencode-go-github/glm-5.3", status: 200, totalMs: 13_540, ttftMs: 5_012, tokS: 92.4, agent: "opencode", time: "22:58",
                           totalTokens: 48_210, cachePct: 62, capability: "powerful",
                           tags: DecisionTags(difficulty: "hard", domain: "code", flags: ["tools", "stakes"], mode: "model+rules", fit: "match")),
            RecentDecision(model: "deepseek/deepseek-v4-pro", status: 200, totalMs: 9_180, ttftMs: 3_240, tokS: 71.2, agent: "claude-code", time: "22:51",
                           totalTokens: 21_450, cachePct: 45, capability: "powerful",
                           tags: DecisionTags(difficulty: "medium", domain: "agent", flags: ["tools"], mode: "model", fit: "over")),
            RecentDecision(model: "minimax-coding/minimax-m3.1", status: 200, totalMs: 3_905, ttftMs: 1_180, tokS: 128.6, agent: "opencode", time: "22:44",
                           totalTokens: 4_120, cachePct: 18, capability: "basic",
                           tags: DecisionTags(difficulty: "easy", domain: "chat", flags: [], mode: "reused", fit: "match")),
            RecentDecision(model: "zhipuai-coding-plan/glm-5.3", status: 500, totalMs: 8_214, ttftMs: 4_410, tokS: nil, agent: "pi", time: "22:41",
                           failReason: "quota exhausted (api-backed)", capability: "powerful",
                           tags: DecisionTags(difficulty: "medium", domain: "lookup", flags: ["trivial"], mode: "rules", fit: "under")),
        ]
        state.providers = [
            ProviderRow(key: "opencode-go", enabled: true, available: true, cooldownUntil: nil, cooldownCause: nil, pct: 4, tokens24h: 92_000, planKind: "go"),
            ProviderRow(key: "deepseek", enabled: true, available: true, cooldownUntil: nil, cooldownCause: nil, pct: 26, tokens24h: 51_000, planKind: "api"),
            ProviderRow(key: "MiniMax", enabled: true, available: false, cooldownUntil: Date().addingTimeInterval(6_720), cooldownCause: "quota window exhausted (5h)", pct: 100, tokens24h: 12_400, planKind: "coding"),
            ProviderRow(key: "zhipuai-coding-plan", enabled: true, available: false, cooldownUntil: Date().addingTimeInterval(8_520), cooldownCause: "transient error (cooling down)", pct: 71, tokens24h: 51_000, planKind: "coding"),
        ]
        state.lastSync = Date()
        // 预览/截图：展示“配置改动待重启”提示行与 Restart 按钮（README 图需要）
        if ProcessInfo.processInfo.environment["EV_PREVIEW_SEED"] != nil {
            // EV_PREVIEW_JUDGE=model|degraded|missing|rules → 切换"配置态 + 横幅"，便于核对
            // 头行 = 配置情况（backend + slots）；行内标签 = 每条 request 的实际判定来源
            switch ProcessInfo.processInfo.environment["EV_PREVIEW_JUDGE"] ?? "model" {
            case "degraded":
                state.decisionStatus = "degraded"
                state.activeSlots = ["jev"]
                state.decisionBackend = "decision_model"
            case "missing":
                state.decisionStatus = "missing"
                state.activeSlots = []
                state.decisionBackend = "auto"
            case "rules":
                state.decisionStatus = "ok"
                state.activeSlots = []
                state.decisionBackend = "heuristic"
            default:
                state.decisionStatus = "ok"
                state.activeSlots = ["jev"]
                state.decisionBackend = "decision_model"
            }
            state.configRestartRequired = ProcessInfo.processInfo.environment["EV_PREVIEW_DIRTY"] != nil
        }

        let win = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 336, height: 860),
            styleMask: [.titled, .resizable],
            backing: .buffered, defer: false
        )
        win.title = "Panel — \(scheme == .dark ? "dark" : "light")"
        win.isReleasedWhenClosed = false
        win.backgroundColor = scheme == .dark
            ? NSColor(deviceWhite: 0.08, alpha: 0.96)
            : NSColor(deviceWhite: 0.94, alpha: 0.96)
        let seeded = ProcessInfo.processInfo.environment["EV_PREVIEW_SEED"] != nil
        let host = NSHostingView(
            rootView: PanelView(state: state, gateway: .shared, previewSeed: seeded)
                .environment(\.colorScheme, scheme)
        )
        win.contentView = host
        win.center()
        win.makeKeyAndOrderFront(nil)
        // 截图模式（EV_SCREENSHOT=<path>）：8s 后对**本窗口**截图并退出——
        // 用 windowNumber 定位，不受其他窗口遮挡/层级影响
        if let shot = ProcessInfo.processInfo.environment["EV_SCREENSHOT"] {
            let n = win.windowNumber
            DispatchQueue.main.asyncAfter(deadline: .now() + 8) {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
                p.arguments = ["-x", "-o", "-l", "\(n)", shot]
                try? p.run()
                p.waitUntilExit()
                exit(0)
            }
        }
        // 预览 harness 轮询循环：等真实网关就绪即覆盖种子数据（app 的循环在
        // AppDelegate 里，harness 只跑一次会错过就绪窗口）。
        // EV_PREVIEW_SEED=1 = 只渲染种子（README 截图用，保证图稳定可复现）
        if ProcessInfo.processInfo.environment["EV_PREVIEW_SEED"] != nil { return }
        Task { [weak state] in
            guard let state else { return }
            while !Task.isCancelled {
                await state.pollLive()
                if state.healthy { await state.pollData() }
                try? await Task.sleep(nanoseconds: 2_000_000_000)
            }
        }
    }

    static let darkMode = UserDefaults.standard.string(forKey: "AppleInterfaceStyle")?
        .lowercased().contains("dark") == true

    static func go() {
        let forced = ProcessInfo.processInfo.environment["EV_PREVIEW_SCHEME"]
        NSApplication.shared.setActivationPolicy(.accessory)
        run(forced == "light" ? .light : forced == "dark" ? .dark : (darkMode ? .dark : .light))
        NSApp.activate(ignoringOtherApps: true)
        NSApp.run()
    }
}

MainActor.assumeIsolated { PanelPreview.go() }
