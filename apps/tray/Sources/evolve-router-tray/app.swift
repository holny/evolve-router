// EvolveRouter 菜单栏伴侣（SwiftUI MenuBarExtra 原生实现，.window 面板样式）
// 图标四态实时反映网关：停止（暗淡）/ 空闲（常态）/ 忙碌（呼吸，决策+请求合并）/
// 失败（叹号 60s，直到下一次成功）。点击弹出迷你面板：状态（含进行中计数）、
// 方案 5h 用量、最近决策、Provider 管理（状态点/冷却/24h tokens/配额进度）、
// 智能启停控制、开机自启、开源地址与完整面板入口。
// 数据源：本地网关。双频轮询常驻——live 1s（/api/live + /healthz）驱动图标与
// 进行中计数；data 3s（stats/providers/status/events）驱动面板明细。
// 配额百分比取 /api/providers/status 的 pressure（服务端已做套餐单位换算，
// 与完整面板同源——曾因直接用 /api/providers 原始 used/allowance 出现 172222% 错值）。

import SwiftUI

enum Config {
    static let baseURL = ProcessInfo.processInfo.environment["EVOLVE_URL"] ?? "http://127.0.0.1:8787"
    static let githubURL = URL(string: "https://github.com/holny/evolve-router")!
    static let gatewayLabel = "ai.evolve-router.gateway"
    static let trayLabel = "ai.evolve-router.tray"
}

// MARK: - 状态 + 轮询

struct RecentDecision {
    let model: String
    let status: Int
    let totalMs: Int
    let ttftMs: Int?
    let tokS: Double?
    let agent: String
    let time: String
}

struct ProviderRow: Identifiable {
    let key: String
    let enabled: Bool
    let available: Bool      // 无熔断冷却
    let cooldownMs: Int?
    let pct: Int?            // 配额进度：observed 实测优先 → windows 兜底（null=按量/无配额）
    let tokens24h: Int
    let planKind: String
    var id: String { key }
}

enum IconState { case idle, busy, fail, stopped }

@MainActor
final class AppState: ObservableObject {
    @Published var healthy = true
    @Published var inflight = 0
    @Published var pulse = false
    @Published var requests = 0
    @Published var successRate = 0
    @Published var tokensK = "-"
    @Published var plans: [(String, Int)] = []
    @Published var recent: [RecentDecision] = []
    @Published var providers: [ProviderRow] = []
    @Published var lastSync: Date?
    @Published var lastFailAt: Date?

    var iconState: IconState {
        if !healthy { return .stopped }
        if let f = lastFailAt, Date().timeIntervalSince(f) < 60 { return .fail }
        if inflight > 0 { return .busy }
        return .idle
    }

    /// 高频轻量轮询（1s）：in-flight 计数 + 健康，驱动图标与"进行中"
    func pollLive() async {
        if case (200, let data)? = try? await get("/api/live"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            inflight = (obj["inflight"] as? Int) ?? Int((obj["inflight"] as? Double) ?? 0)
        } else {
            inflight = 0
        }
        if let resp = try? await get("/healthz") {
            healthy = resp.0 == 200
        } else {
            healthy = false
        }
    }

    /// 低频明细轮询（3s）：统计/Provider/事件
    func pollData() async {
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
        // /api/providers：Provider 唯一主链路（服务端已按 plan_key 归一同套餐、
        // 单位换算与完整面板同源）；/api/providers/status 仅作冷却/可用 overlay
        var rows: [ProviderRow] = []
        var byPlan: [String: Int] = [:]
        var coolByKey: [String: (Bool, Int?)] = [:]
        if case (200, let sdata)? = try? await get("/api/providers/status"),
           let sw = try? JSONSerialization.jsonObject(with: sdata) as? [String: Any],
           let sarr = sw["providers"] as? [[String: Any]] {
            for s in sarr {
                let k = (s["key"] as? String) ?? ""
                let cd = (s["cooldown_remaining_ms"] as? Int)
                    ?? (s["cooldown_remaining_ms"] as? Double).map(Int.init)
                coolByKey[k] = (((s["available"] as? Bool) ?? true), cd)
            }
        }
        if case (200, let data)? = try? await get("/api/providers"),
           let wrapper = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let arr = wrapper["providers"] as? [[String: Any]] {
            for p in arr {
                let key = (p["key"] as? String) ?? "-"
                let mergedKeys = (p["merged_from"] as? [Any])?.compactMap { $0 as? String } ?? [key]
                let kind = (p["plan_kind"] as? String) ?? "-"
                let windows = p["windows"] as? [String: Any]
                let w5 = (windows?["5h"] as? Double) ?? (windows?["5h"] as? Int).map(Double.init)
                let used5 = (p["used_5h"] as? Double) ?? 0
                // 配额进度（dashboard Provider 卡同款）：observed 实测优先 → windows 兜底
                var pct: Int? = nil
                if let obs = p["observed"] as? [String: Any],
                   let o5 = obs["5h"] as? [String: Any],
                   let limit = (o5["limit"] as? Double) ?? (o5["limit"] as? Int).map(Double.init),
                   let remaining = (o5["remaining"] as? Double) ?? (o5["remaining"] as? Int).map(Double.init),
                   limit > 0 {
                    pct = Int(((limit - remaining) / limit * 100).rounded())
                }
                if pct == nil, let w5, w5 > 0 {
                    pct = Int((used5 / w5 * 100).rounded())
                }
                // 方案聚合（同 kind 取最紧窗口）
                if let pct {
                    let short = ["coding": "CP", "go": "GO", "agent": "AP", "token": "TOK"][kind]
                        ?? String(kind.prefix(4)).uppercased()
                    byPlan[short] = max(byPlan[short] ?? 0, pct)
                }
                // 冷却/可用：合并行内任一账号冷却即显示最小正冷却
                var available = true
                var cooldownMs: Int? = nil
                for k in mergedKeys {
                    if let (av, cd) = coolByKey[k] {
                        available = available && av
                        if let cd, cd > 0 { cooldownMs = min(cooldownMs ?? Int.max, cd) }
                    }
                }
                let disabled = (p["disabled"] as? Bool) ?? false
                let tok24 = (p["tokens_24h"] as? Int) ?? Int((p["tokens_24h"] as? Double) ?? 0)
                rows.append(ProviderRow(key: key, enabled: !disabled, available: available,
                                        cooldownMs: cooldownMs, pct: pct,
                                        tokens24h: tok24, planKind: kind))
            }
        }
        rows.sort {
            if $0.enabled != $1.enabled { return $0.enabled }
            return $0.tokens24h > $1.tokens24h
        }
        providers = Array(rows.prefix(8))
        plans = byPlan.sorted { $0.value > $1.value }
        if case (200, let data)? = try? await get("/api/events?limit=30"),
           let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
            // 事件流 oldest→newest（服务端已 reverse）
            var decRows: [RecentDecision] = []
            for ev in arr.reversed() where (ev["kind"] as? String) == "chat" {
                let model = (ev["chosen"] as? String) ?? "-"
                let status = (ev["status"] as? Int) ?? Int((ev["status"] as? Double) ?? 0)
                let total = (ev["total_ms"] as? Int) ?? Int((ev["total_ms"] as? Double) ?? 0)
                let ttft = (ev["ttft_ms"] as? Int) ?? Int((ev["ttft_ms"] as? Double) ?? -1)
                let out = (ev["completion_tokens"] as? Int) ?? Int((ev["completion_tokens"] as? Double) ?? 0)
                let gen = max(1, total - max(0, ttft))
                let tokS = out > 0 ? Double(out) / (Double(gen) / 1000.0) : nil
                let agent = ((ev["agent"] as? String) ?? "-").split(separator: "/").first.map(String.init) ?? "-"
                let time = Date(timeIntervalSince1970: ((ev["ts"] as? Double) ?? 0)/1000).formatted(date: .omitted, time: .shortened)
                decRows.append(RecentDecision(model: model, status: status, totalMs: total,
                                              ttftMs: ttft > 0 ? ttft : nil, tokS: tokS,
                                              agent: agent, time: time))
                if decRows.count >= 5 { break }
            }
            recent = decRows
            // 失败态检测：最新一条 chat 事件——失败后图标亮叹号 60s，成功即清除
            if let newest = arr.last(where: { ($0["kind"] as? String) == "chat" }) {
                let status = (newest["status"] as? Int) ?? Int((newest["status"] as? Double) ?? 0)
                if status != 200 {
                    let ts = (newest["ts"] as? Double) ?? 0
                    lastFailAt = Date(timeIntervalSince1970: ts / 1000)
                } else {
                    lastFailAt = nil
                }
            }
        }
        lastSync = Date()
    }

    private func get(_ path: String) async throws -> (Int, Data) {
        let (data, resp) = try await URLSession.shared.data(from: URL(string: Config.baseURL + path)!)
        return ((resp as? HTTPURLResponse)?.statusCode ?? 0, data)
    }
}

// MARK: - 菜单栏图标（四态）

enum TrayIcon {
    static let base: NSImage = {
        let url = Bundle.module.url(forResource: "tray-icon@2x", withExtension: "png")
        let img = url.flatMap { NSImage(contentsOf: $0) } ?? NSImage()
        img.isTemplate = true
        img.size = NSSize(width: 22, height: 22)
        return img
    }()

    // 按 alpha 重绘（template 只取 alpha 通道：alpha 缩放 = 菜单栏内变淡）
    static func alpha(_ a: CGFloat) -> NSImage {
        let out = NSImage(size: NSSize(width: 22, height: 22))
        out.isTemplate = true
        out.lockFocus()
        if let ctx = NSGraphicsContext.current?.cgContext,
           let cg = base.cgImage(forProposedRect: nil, context: nil, hints: nil) {
            ctx.interpolationQuality = .high
            ctx.setAlpha(a)
            ctx.draw(cg, in: NSRect(x: 0, y: 0, width: 22, height: 22))
        }
        out.unlockFocus()
        return out
    }

    static let fail: NSImage = {
        let cfg = NSImage.SymbolConfiguration(pointSize: 16, weight: .medium)
        let img = (NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: "gateway error")?
            .withSymbolConfiguration(cfg)) ?? NSImage()
        img.isTemplate = true
        return img
    }()
}

struct TrayIconView: View {
    @ObservedObject var state: AppState

    var body: some View {
        let ns: NSImage = {
            switch state.iconState {
            case .stopped: return TrayIcon.alpha(0.35)
            case .fail:    return TrayIcon.fail
            case .busy:    return state.pulse ? TrayIcon.alpha(0.45) : TrayIcon.alpha(1.0)
            case .idle:    return TrayIcon.alpha(1.0)
            }
        }()
        Image(nsImage: ns)
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
            HStack(spacing: 6) {
                Circle().fill(state.healthy ? Color.green : Color.red).frame(width: 8, height: 8)
                Text(state.healthy ? "网关运行中 · 8787" : "网关已停止")
                    .font(.system(size: 12, weight: .semibold))
                // 进行中计数（1s 实时）
                if state.inflight > 0 {
                    HStack(spacing: 3) {
                        Circle().fill(.blue).frame(width: 5, height: 5)
                        Text("\(state.inflight) 进行中")
                            .font(.system(size: 10, weight: .medium))
                            .foregroundStyle(.blue)
                    }
                }
                // 复制接入配置（baseURL/model/provider——agent 接入三要素）
                Button {
                    let cfg = """
                    baseURL  = \(Config.baseURL)/v1
                    model    = auto
                    provider = evo-router
                    """
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(cfg, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc").font(.system(size: 10))
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .help("复制接入配置（baseURL / model / provider）")
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
                            Text(name)
                                .font(.system(size: 11, weight: .medium))
                                .lineLimit(1).minimumScaleFactor(0.7)
                                .frame(width: 44, alignment: .leading)
                            ProgressBar(pct: pct)
                            Text(pct > 999 ? ">999%" : "\(pct)%")
                                .font(.system(size: 11, design: .monospaced))
                                .lineLimit(1).minimumScaleFactor(0.7)
                                .frame(width: 40, alignment: .trailing)
                        }
                    }
                }
            }
            if !state.recent.isEmpty {
                section("最近决策") {
                    ForEach(Array(state.recent.enumerated()), id: \.offset) { _, r in
                        HStack(spacing: 6) {
                            Circle().fill(r.status == 200 ? Color.green : Color.red).frame(width: 5, height: 5)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(r.model).font(.system(size: 10.5)).lineLimit(1)
                                HStack(spacing: 5) {
                                    Text(r.time).font(.system(size: 9)).foregroundStyle(.secondary)
                                    if let tt = r.ttftMs { Text("TTFT \(tt)ms").font(.system(size: 9)).foregroundStyle(.secondary) }
                                    if let s = r.tokS, s > 0 { Text(String(format: "%.0f tok/s", s)).font(.system(size: 9)).foregroundStyle(.secondary) }
                                    Text(r.agent).font(.system(size: 9, weight: .medium)).foregroundStyle(.tertiary)
                                }
                            }
                            Spacer()
                            Text(r.status == 200 ? "\(r.totalMs)ms" : "fail")
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundStyle(r.status == 200 ? .secondary : Color.red)
                        }
                    }
                }
            }
            if !state.providers.isEmpty {
                section("Provider 管理") {
                    ForEach(state.providers) { p in
                        HStack(spacing: 6) {
                            // 状态点：绿=可用 / 橙=熔断冷却中 / 灰=已禁用
                            Circle()
                                .fill(p.enabled ? (p.available ? Color.green : Color.orange) : Color.gray)
                                .frame(width: 5, height: 5)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(p.key).font(.system(size: 10.5)).lineLimit(1)
                                HStack(spacing: 4) {
                                    if let cd = p.cooldownMs, cd > 0 {
                                        Text("冷却 \(cd / 60_000)m")
                                            .font(.system(size: 9)).foregroundStyle(.orange)
                                    }
                                    if !p.enabled {
                                        Text("已禁用").font(.system(size: 9)).foregroundStyle(.tertiary)
                                    }
                                    if p.tokens24h > 0 {
                                        Text("\(Self.fmtTok(p.tokens24h)) · 24h")
                                            .font(.system(size: 9)).foregroundStyle(.tertiary)
                                    }
                                }
                            }
                            Spacer()
                            if let pct = p.pct {
                                ProgressBar(pct: pct).frame(width: 46)
                                Text(pct > 999 ? ">999%" : "\(pct)%")
                                    .font(.system(size: 10, design: .monospaced))
                                    .lineLimit(1).minimumScaleFactor(0.7)
                                    .frame(width: 38, alignment: .trailing)
                            } else {
                                Text(p.planKind == "api" ? "按量" : "—")
                                    .font(.system(size: 9)).foregroundStyle(.tertiary)
                            }
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
        .frame(width: 320)
        .task { await state.pollData() }
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

    static func fmtTok(_ n: Int) -> String {
        n >= 1_000_000 ? String(format: "%.1fM", Double(n)/1e6)
        : n >= 1_000 ? String(format: "%.0fk", Double(n)/1e3)
        : "\(n)"
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
        await state.pollLive() // 先取当前真实状态
        let uid = getuid()
        if state.healthy {
            shell("launchctl bootout gui/\(uid)/\(Config.gatewayLabel)")
        } else {
            shell("launchctl bootstrap gui/\(uid)/\(Config.gatewayLabel)")
        }
        // bootout/bootstrap 异步生效——等一拍再反馈
        try? await Task.sleep(nanoseconds: 800_000_000)
        await state.pollLive()
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

// MARK: - App（双频轮询 + 忙碌呼吸帧，均挂 App 级常驻）

@main
struct EvolveRouterTrayApp: App {
    @StateObject private var state = AppState()

    init() {
        // 轮询必须挂在 App 级（常驻）——挂在面板内容的 .task 上会在
        // 面板关闭时被暂停，状态冻结导致按钮卡死（实测根因）
        let s = _state.wrappedValue
        // live 1s：in-flight + 健康 → 图标状态与"进行中"计数
        let fast = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            Task { @MainActor in await s.pollLive() }
        }
        RunLoop.main.add(fast, forMode: .common)
        // data 3s：统计/Provider/事件明细
        let slow = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in
            Task { @MainActor in await s.pollData() }
        }
        RunLoop.main.add(slow, forMode: .common)
        // 忙碌呼吸帧 0.5s（仅忙碌时翻转）
        let pulse = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { _ in
            Task { @MainActor in
                if s.healthy && s.inflight > 0 {
                    s.pulse.toggle()
                } else if s.pulse {
                    s.pulse = false
                }
            }
        }
        RunLoop.main.add(pulse, forMode: .common)
    }

    var body: some Scene {
        MenuBarExtra {
            PanelView(state: state)
        } label: {
            // 独立 View 持 @ObservedObject——MenuBarExtra label 不随 App 级
            // ObservableObject 自动重渲染，包一层观察视图才能四态切换
            TrayIconView(state: state)
        }
        .menuBarExtraStyle(.window)
    }
}
