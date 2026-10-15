import SwiftUI
import AppKit

/// 面板 v2：原生分组式层级（系统字号 / SF Symbols 区段标头 / 单层容器）
public struct PanelView: View {
    @ObservedObject public var state: AppState
    @ObservedObject public var gateway: GatewayProcess
    @State private var autostart = UserDefaults.standard.object(forKey: "ev-autostart") == nil
        ? true
        : UserDefaults.standard.bool(forKey: "ev-autostart")
    @State private var showExitConfirm = false

    public init(state: AppState, gateway: GatewayProcess) {
        _state = ObservedObject(wrappedValue: state)
        _gateway = ObservedObject(wrappedValue: gateway)
        _autostart = State(initialValue: UserDefaults.standard.object(forKey: "ev-autostart") == nil
            ? true
            : UserDefaults.standard.bool(forKey: "ev-autostart"))
    }

    public var body: some View {
        VStack(spacing: 10) {
            header
            decisionBanner
            metricsCard
            // 中段（Providers + Recent）可滚动：打标行增加了内容高度，必须保证
            // 底部控制按钮（Stop/Quit）始终在框内（用户裁决）
            ScrollView(.vertical, showsIndicators: false) {
                VStack(spacing: 10) {
                    if !state.providers.isEmpty { providersCard }
                    if !state.recent.isEmpty { decisionsCard }
                }
            }
            .frame(maxHeight: .infinity)
            controls
        }
        .padding(14)
        .frame(width: 336, height: 620)
        .task {
            await state.pollLive()
            await state.pollData()
        }
        .confirmationDialog(t("quit_confirm"), isPresented: $showExitConfirm, titleVisibility: .visible) {
            Button(t("quit_action"), role: .destructive) { quitAll() }
            Button(t("cancel"), role: .cancel) {}
        } message: {
            Text(t("quit_hint"))
        }
    }

    // MARK: - 头部（品牌位 + 状态行）

    private var header: some View {
        HStack(spacing: 10) {
            // 品牌徽章 = 官方 v5 彩盘（与菜单栏/桌面图标同源）
            brandBadge
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Circle()
                        .fill(statusColor)
                        .frame(width: 7, height: 7)
                    Text(statusText)
                        .font(.system(size: 13, weight: .semibold))
                        .lineLimit(1)
                }
                if state.lastSync != nil || !state.version.isEmpty {
                    HStack(spacing: 6) {
                        if let ts = state.lastSync {
                            Text("\(t("last_sync")) \(ts.formatted(date: .omitted, time: .shortened))")
                        }
                        if !state.version.isEmpty {
                            Text("v\(state.version)")
                        }
                    }
                    .font(.system(size: 10))
                    .foregroundStyle(.tertiary)
                }
            }
            Spacer()
            Button {
                let cfg = """
                baseURL  = \(Config.baseURL)/v1
                model    = Auto (smart routing)
                provider = evo-router
                """
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(cfg, forType: .string)
            } label: {
                Image(systemName: "doc.on.doc")
                    .font(.system(size: 12))
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help(t("copy_cfg"))
        }
    }

    // MARK: - 决策模型横幅（missing/stale 提示；点按打开 Dashboard）

    @ViewBuilder
    private var decisionBanner: some View {
        if state.decisionStatus == "missing" || state.decisionStatus == "stale" {
            let stale = state.decisionStatus == "stale"
            Button {
                openDashboard()
            } label: {
                HStack(spacing: 7) {
                    Image(systemName: stale ? "checkmark.seal" : "exclamationmark.triangle")
                        .font(.system(size: 10, weight: .semibold))
                    VStack(alignment: .leading, spacing: 1) {
                        Text(stale ? t("dm_stale") : t("dm_missing"))
                            .font(.system(size: 10.5, weight: .semibold))
                            .lineLimit(1)
                        if !stale {
                            Text(t("dm_missing_hint"))
                                .font(.system(size: 9))
                                .foregroundStyle(.orange.opacity(0.85))
                                .lineLimit(1)
                        }
                    }
                    Spacer(minLength: 0)
                    Image(systemName: "arrow.up.right")
                        .font(.system(size: 9))
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(Color.orange.opacity(0.10), in: RoundedRectangle(cornerRadius: 8))
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.orange)
            .help(stale ? "Decision model configured — restart the gateway to apply" : "Decision model not configured — set DECISION_JEV_* / LAYA_* / OPENAI_* env (see Dashboard banner)")
        }
    }

    private func openDashboard() {
        if let url = URL(string: Config.baseURL) { NSWorkspace.shared.open(url) }
    }

    private var statusColor: Color {
        if !state.healthy { return .red }
        if state.starting { return .orange }
        return .green
    }

    private var statusText: String {
        if !state.healthy { return t("stopped") }
        if state.starting { return t("starting") }
        if state.external { return t("external") }
        return t("healthy")
    }

    // MARK: - 指标卡

    private var metricsCard: some View {
        HStack(spacing: 14) {
            metric("gauge.with.needle", label: t("reqs"), value: "\(state.requests)")
            metric("arrow.up.right", label: t("success_rate"), value: "\(state.successRate)%")
            metric("number", label: t("tokens"), value: state.tokensK)
            Spacer(minLength: 0)
            // 窗口标签即切换钮（用户裁决）：今天 ⇄ 昨天——同时作用于顶部指标与 Providers 行
            Button {
                state.statWindowToday.toggle()
                Task { await state.pollData() }
            } label: {
                Text(state.statWindowToday ? "today" : "yesterday")
                    .font(.system(size: 9, weight: .medium))
                    .lineLimit(1)
                    .fixedSize()
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Capsule().fill(Color.primary.opacity(0.07)))
            }
            .buttonStyle(.plain)
            .help("Metrics window — click to switch between today and yesterday")
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    private func metric(_ symbol: String, label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 4) {
                Image(systemName: symbol)
                    .font(.system(size: 9, weight: .medium))
                    .foregroundStyle(.tertiary)
                Text(label)
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
            }
            Text(value)
                .font(.system(size: 16, weight: .semibold, design: .rounded))
                .monospacedDigit()
                .contentTransition(.numericText())
        }
    }

    // MARK: - 最近决策

    private var decisionsCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            sectionLabel("sparkle.middle", t("recent"))
            let rows = state.recent
            ForEach(rows, id: \.id) { r in
                HStack(spacing: 7) {
                    Circle()
                        .fill(r.status == 200 ? Color.green : Color.red)
                        .frame(width: 5, height: 5)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(r.model)
                            .font(.system(size: 11, weight: .medium))
                            .lineLimit(1)
                            .minimumScaleFactor(0.8)
                        VStack(alignment: .leading, spacing: 1) {
                            HStack(spacing: 5) {
                                let routing = r.preprocessMs + r.decisionMs
                                Text("ER \(Self.fmtMs(routing))")
                                    .help("EvolveRouter routing decision time")
                                if let tt = r.ttftMs {
                                    Text("TTFT \(Self.fmtMs(tt))")
                                        .help("upstream time-to-first-token")
                                }
                                if let s = r.tokS, s > 0 {
                                    Text(AppState.fmtRate(s) + " t/s")
                                        .help("generation speed")
                                }
                                if r.totalTokens > 0 {
                                    Text("\(AppState.fmtTok(r.totalTokens))(\(r.cachePct ?? 0)%)")
                                        .help("total tokens (cache hit rate in parens)")
                                }
                            }
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .minimumScaleFactor(0.85)
                            // 决策打标微缩行（用户裁决）：任务难度 + 模型能力/价位
                            // + 网关评分 + 难度↔能力匹配（英文标签 + 背景胶囊）
                            if r.difficulty != nil || r.capability != nil || r.score != nil {
                                HStack(spacing: 3) {
                                    if let d = r.difficulty {
                                        let (label, col) = AppState.diffBadge(d)
                                        Text(label)
                                            .font(.system(size: 8.5, weight: .medium))
                                            .padding(.horizontal, 4).padding(.vertical, 1)
                                            .background(Capsule().fill(col.opacity(0.15)))
                                            .foregroundStyle(col)
                                    }
                                    if let cap = r.capability {
                                        tagPill("cap " + cap)
                                    }
                                    if let pt = r.priceTier {
                                        tagPill("tier " + pt)
                                    }
                                    if let sc = r.score {
                                        tagPill(String(format: "★%.2f", sc))
                                    }
                                    let capMap = ["basic": 0, "standard": 1, "powerful": 2]
                                    if let cap = r.capability, let d = r.difficulty,
                                       let cIdx = capMap[cap] {
                                        let dIdx = d <= 0.8 ? 0 : d <= 2.0 ? 1 : 2
                                        let m = dIdx == cIdx ? "match" : (cIdx > dIdx ? "over-fit" : "under-fit")
                                        tagPill(m, tint: dIdx == cIdx ? .green : .orange)
                                    }
                                }
                                .lineLimit(1)
                                .minimumScaleFactor(0.75)
                            }
                            if r.status != 200, let reason = r.failReason {
                                Text("Fail: \(reason)")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.red)
                                    .lineLimit(1)
                                    .help(reason)
                            }
                        }
                    }
                    Spacer(minLength: 0)
                    VStack(alignment: .trailing, spacing: 1) {
                        Text(Self.fmtMs(r.totalMs))
                            .font(.system(size: 10, design: .rounded))
                            .monospacedDigit()
                            .foregroundStyle(r.status == 200 ? .secondary : Color.red)
                        Text(r.time)
                            .font(.system(size: 9, design: .rounded))
                            .monospacedDigit()
                            .foregroundStyle(.tertiary)
                    }
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    // MARK: - Providers

    private var providersCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                sectionLabel("server.rack", t("providers"))
                Spacer(minLength: 0)
                dimTabs
            }
            if state.modelDim {
                ForEach(state.modelRows) { m in
                    modelRow(m)
                }
            } else {
                let provs = state.providers
                ForEach(provs, id: \.id) { p in
                    providerRow(p)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    /// 维度切换 tab（用户裁决）：Provider / Model —— 两维度同布局同位置，
    /// 切换零学习成本；窗口切换钮（today/yesterday）在顶部指标卡，两维度共用
    private var dimTabs: some View {
        HStack(spacing: 0) {
            dimTab("Provider", selected: !state.modelDim) { state.modelDim = false }
            dimTab("Model", selected: state.modelDim) { state.modelDim = true }
        }
        .background(Capsule().fill(Color.primary.opacity(0.06)))
        .clipShape(Capsule())
    }

    private func dimTab(_ label: String, selected: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 9, weight: selected ? .semibold : .medium))
                .foregroundStyle(selected ? Color.primary : .secondary)
                .padding(.horizontal, 7)
                .padding(.vertical, 2)
                .background(selected ? Capsule().fill(Color.primary.opacity(0.10)) : nil)
        }
        .buttonStyle(.plain)
    }

    /// Model 维度行：与 Provider 行同布局（dot + 名称 + 指标 subline），
    /// 右侧不放配额/状态（那是 provider 级信息）
    private func modelRow(_ m: ModelRow) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(Color.green)
                .frame(width: 5, height: 5)
            VStack(alignment: .leading, spacing: 1) {
                Text(m.id)
                    .font(.system(size: 11, weight: .medium))
                    .lineLimit(1)
                HStack(spacing: 5) {
                    // today 视图：展示模型实时打标（能力/价位）；yesterday 是
                    // 当前态标签对历史无意义（用户裁决）→ 不展示
                    if state.statWindowToday {
                        if let cap = m.capability, !cap.isEmpty {
                            tagPill("cap " + cap)
                        }
                        if let pt = m.priceTier, !pt.isEmpty {
                            tagPill("tier " + pt)
                        }
                    }
                    if m.tok > 0 {
                        Text(AppState.fmtTok(m.tok))
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let hit = m.hit {
                        Text("hit \(hit)%")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let rt = m.rate, rt > 0 {
                        Text(AppState.fmtRate(rt) + " t/s")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let tt = m.ttft {
                        Text("TTFT \(Self.fmtMs(tt))")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                }
                .lineLimit(1)
                .minimumScaleFactor(0.75)
            }
            Spacer(minLength: 0)
        }
    }


    @ViewBuilder
    private func providerRow(_ p: ProviderRow) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(p.enabled ? (p.available && !p.quotaExhausted ? Color.green : Color.orange) : Color.gray)
                .frame(width: 5, height: 5)
            VStack(alignment: .leading, spacing: 1) {
                Text(p.key)
                    .font(.system(size: 11, weight: .medium))
                    .lineLimit(1)
                HStack(spacing: 5) {
                    if !p.enabled {
                        Text(t("disabled"))
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                    }
                    if p.tokensWin > 0 {
                        Text(AppState.fmtTok(p.tokensWin))
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let cp = p.cachePctWin {
                        Text("hit \(cp)%")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let rt = p.avgRate, rt > 0 {
                        Text(AppState.fmtRate(rt) + " t/s")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                    if let tt = p.avgTtftMs {
                        Text("TTFT \(Self.fmtMs(tt))")
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                }
                .lineLimit(1)
                // 空间不足先缩字再截断——保证 subline 不被右侧配额条挤没
                .minimumScaleFactor(0.75)
            }
            Spacer(minLength: 0)
            // 昨天/历史视图：配额进度与冷却状态都是**当前态**，对历史无意义
            // （用户裁决）→ 不展示，行内只留窗口指标（tokens/hit/速率/TTFT）
            if state.statWindowToday {
                trailingCell(p)
                    .frame(width: 88, alignment: .trailing)
            }
        }
    }

    @ViewBuilder
    private func trailingCell(_ p: ProviderRow) -> some View {
        // 统一双排模板：第一行状态词（cooldown / 5h full / monthly full / 余额），
        // 第二行倒计时。原因与模型名进 tooltip——与配额窗口态视觉节奏一致
        if let cd = p.cooldownUntil, cd > Date() {
            let txt = Self.fmtCountdown(cd)
            // 配额类冷却（上游 429 说 "quota window exhausted (monthly)"）本质就是
            // 窗口耗尽——并轨到配额窗态同款文案（月度 full + 倒计时），区别于
            // 纯健康冷却（失败驱动）。两条来源在 tooltip 里都写清楚。
            let quotaStyle = p.cooldownCause?.contains("quota window exhausted") == true
            VStack(alignment: .trailing, spacing: 1) {
                Text(quotaStyle ? Self.quotaFullLabel(p.cooldownCause ?? "") : t("cooldown"))
                    .font(.system(size: 9.5, weight: .semibold))
                    .foregroundStyle(.red)
                    .minimumScaleFactor(0.8)
                Text(txt)
                    .font(.system(size: 9, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(Color.red.opacity(0.8))
            }
            .lineLimit(1)
            .help(Self.cooldownHelp(p))
        } else if !p.blockedModels.isEmpty {
            // w3 永久黑名单（上游 404 且不在模型列表）——仅 Web 手动恢复
            VStack(alignment: .trailing, spacing: 1) {
                Text("removed")
                    .font(.system(size: 9.5, weight: .semibold))
                    .foregroundStyle(.red)
                Text("\(p.blockedModels.count) · restore in Dashboard")
                    .font(.system(size: 8.5))
                    .foregroundStyle(Color.red.opacity(0.75))
            }
            .lineLimit(1)
            .help("\(p.blockedModels.joined(separator: ", ")) — removed by upstream (not in the provider model list). Manual restore via Dashboard → Providers → Restore")
        } else if let used = p.quotaUsed, let scope = p.quotaScope {
            if p.quotaExhausted {
                VStack(alignment: .trailing, spacing: 1) {
                    Text("\(scope) full")
                        .font(.system(size: 9.5, weight: .semibold))
                        .foregroundStyle(.red)
                    if let reset = p.quotaReset {
                        Text(Self.fmtCountdown(reset))
                            .font(.system(size: 9, design: .rounded))
                            .monospacedDigit()
                            .foregroundStyle(Color.red.opacity(0.8))
                    }
                }
                .lineLimit(1)
                .help("\(scope) quota exhausted — recovers at \(p.quotaReset.map { Self.fmtAbs($0) } ?? "?")")
            } else {
                HStack(spacing: 5) {
                    quotaBar(used).frame(width: 46)
                    // 固定宽度+右对齐：否则 "1%" 比 "77%" 少一位，整块 HStack 变窄
                    // → 进度条被推向右侧，各行的条不对齐
                    Text("\(used)%")
                        .font(.system(size: 10, design: .rounded))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .frame(width: 34, alignment: .trailing)
                }
                .help("\(scope) quota usage · \(Self.srcTip(p.quotaSource))")
            }
        } else if let bal = p.paygBalance {
            Text("\(bal.currency) \(String(format: "%.2f", bal.total))")
                .font(.system(size: 10, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(bal.available ? Color.secondary : Color.red)
                .help(bal.available ? "Account balance" : "Balance exhausted — top up required")
        } else {
            Text(t("payg"))
                .font(.system(size: 9.5))
                .foregroundStyle(.tertiary)
        }
    }


    // MARK: - 控制区
    // 行 1：主行动（GitHub / 完整面板）—— 等宽 tinted 胶囊
    // 行 2：次要控制（停止 / 退出 / 自启）—— 小号胶囊同语言
    private var controls: some View {
        VStack(spacing: 6) {
            HStack(spacing: 6) {
                chip(icon: "safari", label: t("github"), tint: .accentColor, help: t("github")) {
                    NSWorkspace.shared.open(Config.githubURL)
                }
                chip(icon: "gauge.with.needle", label: t("dashboard"), tint: .accentColor, help: t("dashboard")) {
                    if let url = URL(string: Config.baseURL) { NSWorkspace.shared.open(url) }
                }
            }
            HStack(spacing: 6) {
                gatewayChip
                chip(icon: "power", label: t("quit"), tint: .red, help: t("quit"), danger: true) {
                    showExitConfirm = true
                }
                Spacer(minLength: 2)
                Toggle(isOn: $autostart) { EmptyView() }
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .labelsHidden()
                    .onChange(of: autostart) { on in applyAutostart(on) }
                Text(t("autostart"))
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func chip(icon: String, label: String, tint: Color, help: String,
                      danger: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 5) {
                Image(systemName: icon).font(.system(size: 10, weight: .semibold))
                Text(label).font(.system(size: 11, weight: .medium)).lineLimit(1)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.055)))
            .foregroundStyle(tint)
        }
        .buttonStyle(.plain)
        .help(help)
    }

    /// 停止/启动 胶囊：healthy → 红调（停止），否则 accent（启动）
    private var gatewayChip: some View {
        Button {
            Task { await toggleGateway() }
        } label: {
            HStack(spacing: 5) {
                if state.starting {
                    ProgressView().controlSize(.mini)
                } else {
                    Image(systemName: state.healthy ? "stop.fill" : "play.fill")
                        .font(.system(size: 10, weight: .semibold))
                }
                Text(gatewayChipLabel)
                    .font(.system(size: 11, weight: .medium))
                    .lineLimit(1)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity)
            .background(RoundedRectangle(cornerRadius: 8)
                .fill((state.healthy ? Color.red : Color.accentColor).opacity(0.12)))
            .foregroundStyle(state.healthy ? Color.red : Color.accentColor)
        }
        .buttonStyle(.plain)
        .disabled(state.starting)
        .help(state.healthy ? t("stop_gw") : t("start_gw"))
    }

    private var gatewayChipLabel: String {
        if state.starting { return t("starting") }
        return state.healthy ? t("stop_gw") : t("start_gw")
    }

    /// 品牌徽章 = 官方 v5 彩盘（与菜单栏/桌面图标同源）
    @MainActor
    private var brandBadge: some View {
        let img = NSImage(contentsOf:
            Bundle.module.url(forResource: "tray-icon-color@2x", withExtension: "png")!
        ) ?? NSImage()
        img.size = NSSize(width: 28, height: 28)
        return Image(nsImage: img).frame(width: 28, height: 28)
    }

    // MARK: - 小件

    private var groupBackground: some View {
        RoundedRectangle(cornerRadius: 10)
            .fill(Color.primary.opacity(0.055))
    }

    private func sectionLabel(_ symbol: String, _ title: String) -> some View {
        HStack(spacing: 5) {
            Image(systemName: symbol)
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.tertiary)
            Text(title)
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(.secondary)
                .textCase(nil)
        }
    }

    /// 打标胶囊（英文小标签 + 背景色）
    private func tagPill(_ text: String, tint: Color = .secondary) -> some View {
        Text(text)
            .font(.system(size: 8.5))
            .padding(.horizontal, 4).padding(.vertical, 1)
            .background(Capsule().fill(Color.primary.opacity(0.08)))
            .foregroundStyle(tint)
    }

    private func quotaBar(_ pct: Int) -> some View {
        // 三档配色与 Web 同源（阈值 90 / soft_pct）
        let tint: Color = pct >= 90 ? .red : (pct >= state.softPct ? .orange : .accentColor)
        // ⚠ 必须显式给 total：`ProgressView(value:)` 的 total 默认 1.0，
        // 于是任何 ≥1 的百分数都被画成满格（1% 也显示满格）——曾因此"看着满了还是 1%"
        return ProgressView(value: Double(min(max(pct, 0), 100)), total: 100)
            .tint(tint)
            .progressViewStyle(.linear)
            .accessibilityValue("\(pct)%")
    }

    // MARK: - 操作

    private func toggleGateway() async {
        if state.healthy {
            await gateway.stop()
        } else {
            await gateway.ensureRunning()
        }
        try? await Task.sleep(nanoseconds: 400_000_000)
        await state.pollLive()
        await state.pollData()
    }

    private func applyAutostart(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: "ev-autostart")
        let uid = getuid()
        let action = on ? "enable" : "disable"
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/zsh")
        p.arguments = ["-c", "launchctl \(action) gui/\(uid)/\(Config.trayLabel)"]
        try? p.run()
    }

    private func quitAll() {
        Task {
            await GatewayProcess.shared.stop()
            NSApp.terminate(nil)
        }
    }
}

// 时长格式：>=1s 折秒，<1s 毫秒
extension PanelView {
    /// 上游配额信息（"quota window exhausted (monthly)"）→ 窗口态标签。
    /// 通用提取括号内的窗口名——逐值枚举会随服务端窗口名演进而漏判
    /// （"(weekly/monthly)" 曾因此被显示成 "5h full"）。
    static func quotaFullLabel(_ cause: String) -> String {
        if let r = cause.range(of: "exhausted (") {
            let rest = cause[r.upperBound...]
            if let close = rest.range(of: ")") {
                return String(rest[..<close.lowerBound]) + " full"
            }
        }
        return "5h full"
    }

    /// cooldown tooltip：健康冷却（失败驱动、模型级）与配额窗口（数据驱动、账号级）
    /// 是两条独立的不可用来源——把出处写清楚，免得与 5h/weekly full 混淆
    static func cooldownHelp(_ p: ProviderRow) -> String {
        let quotaStyle = p.cooldownCause?.contains("quota window exhausted") == true
        var s = quotaStyle
            ? "Quota window exhausted per upstream 429 (account-level window)"
            : "Health cooldown (upstream failure signal, per model"
        if !quotaStyle, let m = p.cooldownModel { s += ": \(m)" }
        if quotaStyle { s += "" } else { s += ")" }
        if let c = p.cooldownCause, !c.isEmpty, !quotaStyle { s += " — \(c)" }
        if quotaStyle, let m = p.cooldownModel { s += " — observed on \(m)" }
        s += ". Recovery: exact reset time comes from the upstream response (Retry-After / reset header), "
        s += "else platform defaults (quota 5m · rate-limit 60s×hits · auth/payment 30m×hits · missing model 24h), capped 24h"
        if let reset = p.quotaReset {
            s += ". Quota window resets at \(fmtAbs(reset))"
        }
        return s
    }

    static func fmtMs(_ ms: Int) -> String {
        if ms >= 60_000 { return String(format: "%dm%02ds", ms / 60_000, (ms % 60_000) / 1000) }
        if ms >= 1000 { return String(format: "%.1fs", Double(ms) / 1000) }
        return "\(ms)ms"
    }
    // w2 配额信息源标签（quota refresh source，非 provider 接入方式）：
    // live = 官方配额端点实测（watchdog 90s 自动刷新，失败兜底 1h 再试）；
    // hdr  = 官方限流响应头学习（该账号每次请求到达即更新）；
    // est  = 无官方信号，网关按 tokens×单价 推算（每次轮询重算）
    static func srcTag(_ source: String) -> String {
        switch source {
        case "official": return "live"
        case "official_headers": return "hdr"
        case "estimated": return "est"
        default: return ""
        }
    }

    static func srcTip(_ source: String) -> String {
        switch source {
        case "official":
            return "Quota LIVE from the provider's official API — watchdog refreshes every ~90s (retry 1h on failure)"
        case "official_headers":
            return "Quota learned from official rate-limit response headers — updates with each request on this account"
        case "estimated":
            return "No official quota source — gateway estimates from tokens × unit price, recomputed each panel poll"
        default: return ""
        }
    }

    /// 统一倒计时档位（用户裁决）：
    /// ≥24h → xxdxxhxxm（不显秒）；≥1h → xxhxxm（不显秒）；<1h → xxmxxs（秒级倒计时）
    static func fmtCountdown(seconds: Int) -> String {
        let s = max(0, seconds)
        if s >= 86_400 { return "\(s / 86_400)d\(s % 86_400 / 3_600)h\(s % 3_600 / 60)m" }
        if s >= 3_600 { return "\(s / 3_600)h\(s % 3_600 / 60)m" }
        if s >= 60 { return "\(s / 60)m\(s % 60)s" }
        return "\(s)s"
    }

    static func fmtCountdown(_ until: Date) -> String {
        fmtCountdown(seconds: Int(until.timeIntervalSinceNow))
    }
    static func fmtAbs(_ d: Date) -> String {
        d.formatted(date: .abbreviated, time: .shortened)
    }
}
