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
        // 版本随仓库 Cargo.toml 走（发版前同步），截图别再挂着两年前的旧号
        state.version = "0.3.3"
        // EV_PREVIEW_DIM=model → 直接看 Model 维度（标题/复数 tab/报错状态）
        if ProcessInfo.processInfo.environment["EV_PREVIEW_DIM"] == "model" {
            state.modelDim = true
            state.modelRows = [
                ModelRow(id: "opencode-go-github/glm-5.3", tok: 48_200, hit: 62, rate: 92,
                         ttft: 1_240, requests: 41, capability: "powerful", priceTier: "medium"),
                ModelRow(id: "deepseek/deepseek-v4-pro", tok: 21_400, hit: 45, rate: 71,
                         ttft: 3_180, requests: 8, capability: "powerful", priceTier: "cheap",
                         failPct: 25),
                ModelRow(id: "opencode-go/beam-free", tok: 1_200, hit: nil, rate: 40,
                         ttft: 800, requests: 3, capability: "basic", priceTier: "cheap",
                         failPct: 67, unusableReason: "no-key"),
                // 非账户级失败原因（应显示在行①右侧）
                ModelRow(id: "zhipuai-coding-plan/glm-5.3", tok: 900, hit: nil, rate: 30,
                         ttft: 2_000, requests: 2, capability: "powerful", priceTier: "medium",
                         coolingMs: 240_000, coolMsg: "unsupported model"),
                // 账户级（quota）冷却 → 按裁决不在 model 行显示原因
                ModelRow(id: "MiniMax/MiniMax-M3", tok: 700, hit: nil, rate: 20,
                         ttft: 3_000, requests: 1, capability: "powerful", priceTier: "cheap",
                         coolingMs: 300_000, coolMsg: nil),
            ]
        }
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
        // 三行制（名称+状态 / 数据 / 标签）——种子里也带齐数据与标签，
        // 否则截图只能看到第一行（README/回归看图都看不出结构）
        state.providers = [
            ProviderRow(key: "opencode-go", enabled: true, available: true, cooldownUntil: nil, cooldownCause: nil, pct: 4, planKind: "go",
                        tokensWin: 412_800, avgTtftMs: 1_240, avgRate: 92, cachePctWin: 62,
                        modelCount: 38, freeModels: 2, tierEffective: "go"),
            ProviderRow(key: "deepseek", enabled: true, available: true, cooldownUntil: nil, cooldownCause: nil, pct: 26, planKind: "api",
                        tokensWin: 168_400, avgTtftMs: 3_180, avgRate: 71, cachePctWin: 45,
                        paygBalance: PaygBalance(currency: "CNY", total: 59.31, available: true),
                        modelCount: 4),
            ProviderRow(key: "MiniMax", enabled: true, available: false, cooldownUntil: Date().addingTimeInterval(6_720), cooldownCause: "quota window exhausted (5h)", pct: 100, planKind: "coding",
                        tokensWin: 22_100, modelCount: 10, tierEffective: "pro"),
            ProviderRow(key: "zhipuai-coding-plan", enabled: true, available: false, cooldownUntil: Date().addingTimeInterval(8_520), cooldownCause: "transient error (cooling down)", pct: 71, planKind: "coding",
                        tokensWin: 96_300, avgTtftMs: 900, avgRate: 110, cachePctWin: 38,
                        modelCount: 12, tierEffective: "pro"),
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
        // 崩溃复现 harness（EV_STRESS=1）：0.5s 高频 churn（inflight 抖动 /
        // recent 60 行整组重建 / 时钟走秒），模拟 AttributeGraph 表爆炸的最坏
        // 触发面——目的是离线复现 AG grow_region SIGABRT 并定罪具体视图
        if ProcessInfo.processInfo.environment["EV_STRESS"] == "1" {
            let churnRow = { (i: Int, round: Int) -> RecentDecision in
                let hh = String(format: "%02d", (i * 7 + round / 2) % 24)
                let mm = String(format: "%02d", (i * 3) % 60)
                let ss = String(format: "%02d", round % 60)
                let flags: [String] = i % 3 == 0 ? ["tools"] : []
                let fit = i % 5 == 0 ? "under" : "match"
                let status = 200 + (i % 4 == 0 ? 399 : 0)
                let tags = DecisionTags(
                    difficulty: "medium", domain: "code",
                    flags: flags, mode: "model+rules", fit: fit)
                return RecentDecision(
                    model: "opencode-go-github/mimo-v2.6-flash",
                    status: status, totalMs: 10_000 + i, ttftMs: 4_000,
                    tokS: 90.0, agent: "opencode",
                    time: hh + ":" + mm + ":" + ss,
                    preprocessMs: 20, decisionMs: 10 + round,
                    totalTokens: 48_000 + i, cachePct: 60,
                    capability: "basic", tags: tags)
            }
            var round = 0
            Task { @MainActor [weak state] in
                guard let state else { return }
                while !Task.isCancelled {
                    round += 1
                    state.inflight = 1 + (round % 6)
                    state.requests += round
                    state.lastSync = Date()
                    state.recent = (0..<60).map { churnRow($0, round) }
                    try? await Task.sleep(nanoseconds: 500_000_000)
                }
            }
            return
        }
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
