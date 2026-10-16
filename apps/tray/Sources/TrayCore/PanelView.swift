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

    /// 预览/截图模式：不触发首帧实时轮询，保证种子数据稳定渲染（README 图可复现）
    public var previewSeed: Bool

    public init(state: AppState, gateway: GatewayProcess, previewSeed: Bool = false) {
        _state = ObservedObject(wrappedValue: state)
        _gateway = ObservedObject(wrappedValue: gateway)
        self.previewSeed = previewSeed
        _autostart = State(initialValue: UserDefaults.standard.object(forKey: "ev-autostart") == nil
            ? true
            : UserDefaults.standard.bool(forKey: "ev-autostart"))
    }

    public var body: some View {
        VStack(spacing: 10) {
            header
            decisionBanner
            metricsCard
            if !state.providers.isEmpty { providersCard }
            if !state.recent.isEmpty { decisionsCard }
            // 标签颜色说明（用户裁决）：紧贴控制区之上，每项 1-2 词
            if !state.providers.isEmpty || !state.recent.isEmpty { tagLegend }
            controls
        }
        // 页脚（控制区）与页头留白对齐：菜单栏窗口底部无系统内边距，
        // 与页头同值时按钮会贴住下边框（实测页头 15pt / 页脚 8pt）→ 页脚加宽
        .padding(.horizontal, 14)
        .padding(.top, 14)
        .padding(.bottom, 26)
        .frame(width: 336)
        .task {
            if previewSeed { return }
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
                        // 判定器实况常显（用户问"没配决策模型为啥没提示"）：
                        // 显式 heuristic 是**主动选择**，所以不是告警，但状态必须看得见
                        Text("\(t("judge")) \(judgeLabel)")
                            .foregroundStyle(state.activeSlots.isEmpty ? .tertiary : .secondary)
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

    /// 当前生效的判定器（/api/live 的 config.active_slots；空 = 内置启发式）
    private var judgeLabel: String {
        state.activeSlots.isEmpty ? t("judge_heuristic") : state.activeSlots.joined(separator: "+")
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
                            // Recent 行 = 网关对这道请求的判定（用户裁决）：颜色=类别，
                            // 胶囊里的字=取值。取值全部来自服务端派生 tags（单一事实源）；
                            // 模型侧标签（capability/price）只在 Model 列表面板展示
                            if let tg = r.tags,
                               tg.difficulty != nil || tg.domain != nil || !tg.flags.isEmpty
                                || tg.mode != nil || tg.fit != nil {
                                HStack(spacing: 3) {
                                    if let d = tg.difficulty {
                                        tagPill(d, tint: Self.tagColorDiff,
                                                bg: Self.tagColorDiff.opacity(0.15), help: Self.tagHelpDiff)
                                    }
                                    if let dom = tg.domain {
                                        tagPill(dom, tint: Self.tagColorDomain,
                                                bg: Self.tagColorDomain.opacity(0.15), help: Self.tagHelpDomain)
                                    }
                                    ForEach(tg.flags, id: \.self) { fl in
                                        tagPill(fl, tint: Self.tagColorFlag,
                                                bg: Self.tagColorFlag.opacity(0.15), help: Self.tagHelpFlag)
                                    }
                                    if let fit = tg.fit {
                                        tagPill(fit, tint: Self.tagColorFit,
                                                bg: Self.tagColorFit.opacity(0.15), help: Self.tagHelpFit)
                                    }
                                    if let mode = tg.mode {
                                        tagPill(mode, tint: Self.tagColorMode,
                                                bg: Self.tagColorMode.opacity(0.15), help: Self.tagHelpMode)
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
                            tagPill(cap, tint: Self.tagColorCap, bg: Self.tagColorCap.opacity(0.14), help: Self.tagHelpCap)
                        }
                        if let pt = m.priceTier, !pt.isEmpty {
                            tagPill(pt, tint: Self.tagColorPrice, bg: Self.tagColorPrice.opacity(0.14), help: Self.tagHelpPrice)
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
            // 非 [decision] 段落改动 → 提示（重启按钮就在下一行，不再重复放一个）
            if state.configRestartRequired {
                HStack(spacing: 5) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(.orange)
                    Text(t("cfg_dirty"))
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .help(state.configRestartReason ?? t("restart_help"))
            }
            HStack(spacing: 5) {
                gatewayChip
                textChip(label: t("restart"), tint: .accentColor, help: t("restart_help")) {
                    Task { await restartGateway() }
                }
                textChip(label: t("quit"), tint: .red, help: t("quit"), danger: true) {
                    showExitConfirm = true
                }
                // Autostart 与上面三个按钮**同一行**：为此收紧内距/字号
                // （tooltip 在 MenuBarExtra 里不触发 → 文字必须可见，不能只留开关）
                Toggle(isOn: $autostart) { EmptyView() }
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .labelsHidden()
                    .onChange(of: autostart) { on in applyAutostart(on) }
                Text(t("autostart"))
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                    .fixedSize()
                Spacer(minLength: 0)
            }
        }
    }

    /// 无图标文字胶囊（控制行用：336pt 宽度下图标会挤掉 Autostart 文字）
    private func textChip(label: String, tint: Color, help: String,
                          danger: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 11, weight: .medium))
                .lineLimit(1)
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .frame(maxWidth: .infinity)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.055)))
                .foregroundStyle(tint)
        }
        .buttonStyle(.plain)
        .help(help)
    }

    private func chip(icon: String, label: String, tint: Color, help: String,
                      danger: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 5) {
                Image(systemName: icon).font(.system(size: 10, weight: .semibold))
                Text(label).font(.system(size: 11, weight: .medium)).lineLimit(1)
            }
            .padding(.horizontal, 8)
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
            .padding(.horizontal, 8)
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

    /// 打标胶囊（英文小标签 + 类别底色 + 悬停详解）
    private func tagPill(_ text: String, tint: Color = .secondary, bg: Color? = nil, help: String = "") -> some View {
        let t = Text(text)
            .font(.system(size: 8.5))
            .padding(.horizontal, 4).padding(.vertical, 1)
            .background(Capsule().fill(bg ?? Color.primary.opacity(0.08)))
            .foregroundStyle(tint)
        return help.isEmpty ? AnyView(t) : AnyView(t.help(help))
    }

    // 标签语义（色卡 + 悬停详解）：颜色 = 类别，胶囊里的字 = 该类别的具体取值
    // 配色避开红/黄系（用户裁决：别像报错/警告）→ 全用冷色 + 中性灰
    static let tagColorDiff: Color = .green
    static let tagColorDomain: Color = .teal
    static let tagColorFlag: Color = .indigo
    static let tagColorFit: Color = .cyan
    static let tagColorMode: Color = .purple
    static let tagColorCap: Color = .blue
    static let tagColorPrice: Color = .mint

    static let tagHelpDiff = "task difficulty of this request: easy / medium / hard"
    static let tagHelpDomain = "request domain classified by the gateway: code / math / data / agent / writing / lookup / chat"
    static let tagHelpFlag = "request traits detected by the gateway: trivial (nothing to route) / tools (tool-heavy turn) / vision (has images) / stakes (high-stakes)"
    static let tagHelpFit = "task difficulty ↔ model capability fit: match / over (capability above task) / under (capability below task)"
    static let tagHelpMode = "who decided the route: model (decision model) / model+rules (decision model + rule judge, conservative on conflict) / reused (session stickiness — no decision call) / rules (rule-engine fallback, decision model unavailable) / pinned (you named the model explicitly). Last three skipped the decision model"
    static let tagHelpCap = "model capability tier: basic / standard / powerful"
    static let tagHelpPrice = "model price tier: cheap / medium / premium"

    /// 标签颜色说明：色卡 = 类别名（左：request 判定 / 右：model 属性）
    /// ⚠ 菜单栏弹窗（MenuBarExtra）里 SwiftUI `.help()` 悬停提示不触发 → 取值不赘述，看胶囊
    private var tagLegend: some View {
        HStack(spacing: 7) {
            legendItem(Self.tagColorDiff, "difficulty")
            legendItem(Self.tagColorDomain, "domain")
            legendItem(Self.tagColorFlag, "flags")
            legendItem(Self.tagColorFit, "fit")
            legendItem(Self.tagColorMode, "mode")
            Spacer(minLength: 6)
            legendItem(Self.tagColorCap, "capability")
            legendItem(Self.tagColorPrice, "price")
        }
        .font(.system(size: 8.5))
        .lineLimit(1)
        .minimumScaleFactor(0.8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func legendItem(_ c: Color, _ label: String) -> some View {
        HStack(spacing: 3) {
            Circle().fill(c.opacity(0.6)).frame(width: 6, height: 6)
            Text(label).foregroundStyle(.tertiary)
        }
        .help(Self.tagHelp(label))
    }

    /// 悬停详解（部分宿主不触发，仅作补充）
    static func tagHelp(_ label: String) -> String {
        switch label {
        case "difficulty": return tagHelpDiff
        case "domain": return tagHelpDomain
        case "flags": return tagHelpFlag
        case "fit": return tagHelpFit
        case "mode": return tagHelpMode
        case "capability": return tagHelpCap
        case "price": return tagHelpPrice
        default: return tagHelpFit
        }
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

    /// 面板"Restart"：停掉网关子进程再拉起（非 [decision] 配置改动靠它生效）
    private func restartGateway() async {
        await gateway.stop()
        try? await Task.sleep(nanoseconds: 600_000_000)
        await gateway.ensureRunning()
        await state.pollLive()
        await state.pollData()
    }

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
