import SwiftUI
import AppKit

/// 面板 v2：原生分组式层级（系统字号 / SF Symbols 区段标头 / 单层容器）
public struct PanelView: View {
    @ObservedObject public var state: AppState
    @ObservedObject public var gateway: GatewayProcess
    @State private var autostart = UserDefaults.standard.object(forKey: "ev-autostart") == nil
        ? true
        : UserDefaults.standard.bool(forKey: "ev-autostart")
    /// 有在飞请求时的重启确认（用户裁决：重启前提示在飞的流）
    /// ⚠ MenuBarExtra 里系统 confirmationDialog 的按钮 action 不触发（实测
    /// 2026-10-17，与 .help() 同族失效；Quit 曾因此永远退不出）→ 确认流程
    /// 用面板内嵌行实现，Apple 对话框一律不用
    /// 带按钮标识（review 2026-10-23）：此前 Stop 与 Restart 共用一个武装位，
    /// 武装 Stop 后再点 Restart 会执行到 Stop
    @State private var pendingAction: (id: String, action: () -> Void)?
    /// Quit 已受理（排空在走）：按钮变禁用态给反馈，防"点了没反应"再点
    @State private var quitting = false
    /// Restart 已受理（停+拉起 ~10s）：按钮禁用防重复点
    @State private var restarting = false

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
        // ⚠ 固定高度（2026-10-19 实证）：面板此前只有 .frame(width:) 没有高度，
        // MenuBarExtra 场景每次重测量（图标呼吸 2Hz、任何发布）都要遍历
        // LazyVStack ForEach 全部行求内容高 → 每次 ~0.5s 主线程阻塞，面板关着
        // 也卡死（/tmp/er_busy.txt：busy 时 flushTransactions 1636 vs idle 1）。
        // 宽高都定死后 _FrameLayout.sizeThatFits 短路返回，测量 O(1)。
        // 结构（2026-10-19 二改）：数据卡放滚动区，controls/在飞确认条**钉底**——
        // 此前确认条排在滚动内容末尾，固定高度下被推出可视区 → 有在飞时点
        // Restart/Quit "点了没反应"（确认条根本看不见）。
        VStack(spacing: 10) {
            // ⚠ 无外层滚动（2026-10-23 用户反馈：面板头跟着内容一起滑 = 外滚被
            // 激活，观感即"整面板滑动窗口"）。定高内容区块（header/metrics/
            // providers）依次排列，Recent 卡用 maxHeight: .infinity 弹性吸收
            // 全部余量/变化——内滚消化列表行数变化，banner/确认条出现时挤缩
            // 的也是 Recent，其余区块永远不动。
            VStack(spacing: 10) {
                header
                decisionBanner
                metricsCard
                if !state.providers.isEmpty {
                    // Providers 行的 tokens/速率/TTFT 也是窗口口径 → 切换中同样置灰
                    providersCard
                        .opacity(state.windowLoading ? 0.5 : 1)
                }
                if !state.recent.isEmpty {
                    decisionsCard
                        .frame(maxHeight: .infinity, alignment: .top)
                }
            }
            .padding(.horizontal, 14)
            .padding(.top, 14)
            .padding(.bottom, 4)
            controls
                .padding(.horizontal, 14)
            if pendingAction != nil {
                inflightConfirmBar
                    .padding(.horizontal, 14)
            }
        }
        .padding(.bottom, 14)
        // 高度对齐内容自然高度：定高了测量才 O(1)，但高度必须贴着内容——
        // 多了在钉底按钮上方露缝，少了外层滚动被激活（2026-10-23 实测：providers
        // 内滚 172 + recent 186 时代总高超出视口 ~50pt → 整面板滑动；收 148/148
        // 后内容 ≈ 636，定高 640 贴合）。decisionBanner（降级横幅 ~40pt）出现的
        // 少见态由外层滚动兜底。
        .frame(width: 336, height: 640)
        // 面板开合门控（2026-10-19）：关闭期间 AppState 只养快照（零发布），
        // 打开瞬间 flush + 立即拉一轮最新——菜单栏图标不再被隐藏面板的
        // 每秒全量重测量拖死；预览/截图模式（previewSeed）不打扰种子数据
        .onAppear {
            guard !previewSeed else { return }
            state.setPanelVisible(true)
            // 延迟刷新：首帧先用 snapshot 呈现并落定布局，800ms 后再拉一轮最新
            // （250ms 会撞上首帧渲染——每次打开都变成“布局+渲染×2”，抓包实证）；
            // 随后 dataTask 每 3s 持续刷新
            Task {
                try? await Task.sleep(nanoseconds: 800_000_000)
                await state.pollData()
            }
        }
        .onDisappear {
            guard !previewSeed else { return }
            state.setPanelVisible(false)
        }
    }

    // MARK: - 头部（品牌位 + 状态行）

    private var header: some View {
        HStack(spacing: 10) {
            // 品牌徽章 = 官方 v5 彩盘（与菜单栏/桌面图标同源）
            brandBadge
            // 头行定稿（第四版）：一行只放一件事，元信息压成一条安静的小字
            //  ① 状态（绿点 + 13pt 主字）
            //  ② 元信息（9.5pt tertiary，图标代替 "synced" 字样）：
            //     🕐 11:48 · v0.1.7 · judged by decision model + rules
            // 头行（用户指定版式）：
            //   ① ● Running · 8787 ……… v0.1.7
            //   ② 🕐 11:49 · judged by decision model + rules
            HStack(spacing: 5) {
                Circle()
                    .fill(statusColor)
                    .frame(width: 7, height: 7)
                    .padding(.top, 1)
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 6) {
                        Text(statusText)
                            .font(.system(size: 13, weight: .semibold))
                            .lineLimit(1)
                            .minimumScaleFactor(0.85)
                            .layoutPriority(1)
                        Spacer(minLength: 6)
                        if !state.version.isEmpty {
                            Text("v\(state.version)")
                                .font(.system(size: 9, weight: .medium, design: .rounded))
                                .foregroundStyle(.tertiary)
                                .fixedSize()
                        }
                    }
                    HStack(spacing: 4) {
                        if let ts = state.lastSync {
                            Image(systemName: "clock")
                                .font(.system(size: 8.5))
                            Text(ts.formatted(date: .omitted, time: .shortened))
                                .fixedSize()
                            Text("·").opacity(0.6)
                        }
                        Text("\(t("judge")) \(judgeLabel)")
                            .lineLimit(1)
                            .minimumScaleFactor(0.85)
                            .foregroundStyle(configuredJudge
                                             ? Color.secondary
                                             : Color.secondary.opacity(0.62))
                    }
                    .font(.system(size: 9.5))
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
        }
    }

    /// 头行 = **配置情况**（用户裁决）：配置了哪条判定路径。
    /// 每条 request 各自由谁判定 → 看该行行内标签（mode 胶囊），两者职责分开。
    /// - 无槽位 / 显式 heuristic → rules only
    /// - 有槽位 + backend=decision_model（双判官融合）→ decision model + rules
    /// - 有槽位 + 其它 backend → decision model
    private var judgeLabel: String {
        let backend = state.decisionBackend.lowercased()
        if state.activeSlots.isEmpty || backend == "heuristic" {
            return t("j_rules")
        }
        return (backend == "decision_model" || backend == "typesafe") ? t("j_model_rules") : t("j_model")
    }

    /// 在飞请求摘要（谁在飞、飞了多久）——重启提示与悬停都用它
    private var inflightSummary: String {
        state.inflightDetail.prefix(3).map { r in
            "\(r.model) · \(r.ageSec)s"
        }.joined(separator: "\n")
    }

    /// degraded 横幅文案：原因 + **自动重试状态**（用户问"后续请求还会继续重试吗"）
    private var degradedText: String {
        var s = state.decisionFailReason.isEmpty
            ? t("dm_degraded")
            : String(format: t("dm_degraded_reason"), state.decisionFailReason)
        if state.decisionRetryInMs > 0 {
            s += " · " + String(format: t("dm_retrying"), max(1, state.decisionRetryInMs / 1000))
        } else if state.decisionFails > 0 {
            s += " · " + t("dm_retrying_now")
        }
        return s
    }

    /// 配额信息源短词（"探测配额"的可见证据）：official（官方 API/响应头）→ obs
    /// official_headers → hdr；estimated（网关推算）→ est
    /// 档位胶囊的取值：生效档位优先，缺失时回落显式档位（服务端字段缺失也不丢信息）
    private func displayTier(_ p: ProviderRow) -> String {
        let t = p.tierEffective.isEmpty ? p.tier : p.tierEffective
        return t == "-" ? "" : t
    }

    /// Providers / Model 列表的滚动窗口高度（三行制后每行约 48pt → 这里约 3 行）。
    /// 172 时头部+指标+双卡+图例总高超出滚动视口 ~50pt，外层滚动被激活成
    /// "整面板滑动"（2026-10-23 实测）——收到 148 让总高回到视口内，
    /// 外层滚动只留给降级横幅等少见态兜底
    private let providersScrollH: CGFloat = 148

    /// 配额百分比文案：**估算来源**加 `~` 前缀（与 web 同口径）。
    /// 不用 "quota: estimated/headers" 这类行话标签——数字自己表达可信度，
    /// 省掉一行噪音（用户反馈 2026-10-17）。
    static func quotaPctText(_ pct: Int, src: String) -> String {
        src == "estimated" ? "~\(pct)%" : "\(pct)%"
    }

    /// 是否配置了决策模型（头行用它决定配色；判定路径文案见 judgeLabel）
    private var configuredJudge: Bool {
        !state.activeSlots.isEmpty && state.decisionBackend.lowercased() != "heuristic"
    }

    // MARK: - 决策模型横幅（missing/stale 提示；点按打开 Dashboard）

    @ViewBuilder
    private var decisionBanner: some View {
        if state.decisionStatus == "missing"
            || state.decisionStatus == "stale"
            || state.decisionStatus == "degraded" {
            let stale = state.decisionStatus == "stale"
            let degraded = state.decisionStatus == "degraded"
            Button {
                openDashboard()
            } label: {
                HStack(spacing: 7) {
                    Image(systemName: stale ? "checkmark.seal" : "exclamationmark.triangle")
                        .font(.system(size: 10, weight: .semibold))
                    VStack(alignment: .leading, spacing: 1) {
                        Text(degraded ? degradedText : (stale ? t("dm_stale") : t("dm_missing")))
                            .font(.system(size: 10.5, weight: .semibold))
                            .lineLimit(1)
                        if !stale {
                            // degraded / missing 两态各自的引导（stale 在上面的分支里已 early-return）
                            Text(degraded ? t("dm_degraded_hint") : t("dm_missing_hint"))
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
            // 切换窗口时三个数字置灰过渡（避免“标签已换、数字还是旧的”被当成错数据）
            Group {
                metric("gauge.with.needle", label: t("reqs"), value: "\(state.requests)")
                metric("arrow.up.right", label: t("success_rate"), value: "\(state.successRate)%")
                metric("number", label: t("tokens"), value: state.tokensK)
            }
            // ⚠ 不挂 .animation——插值属性每帧重建样式化文本 display list；
            // 置灰用瞬时切换即可
            .opacity(state.windowLoading ? 0.35 : 1)
            Spacer(minLength: 0)
            // 窗口标签即切换钮（用户裁决）：今天 ⇄ 昨天——同时作用于顶部指标与 Providers 行
            Button {
                state.setStatWindow(!state.statWindowToday)
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
                // ⚠ 不加 contentTransition(.numericText())——数值动画每帧重建
                // 文本 display list（ShapeStyledDisplayList/renderKeyedText 全链
                // 分配，2026-10-19 采样实证），3s 一轮轮询下持续白耗主线程
        }
    }

    // MARK: - 最近决策

    private var decisionsCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                sectionLabel("clock.arrow.circlepath", t("recent"))
                Spacer(minLength: 0)
                // 在飞提示（用户裁决：放 Recent requests 标题右侧，右对齐）
                if state.inflight > 0 {
                    HStack(spacing: 4) {
                        Image(systemName: "antenna.radiowaves.left.and.right")
                            .font(.system(size: 8.5))
                        Text(String(format: t("inflight_n"), state.inflight))
                            .font(.system(size: 9))
                    }
                    .foregroundStyle(.orange)
                        }
            }
            // 最近 10 天（用户裁决）：固定高度滚动窗口——条数多了也不占面板高度
            let rows = state.recent
            ScrollView(.vertical, showsIndicators: true) {
                LazyVStack(alignment: .leading, spacing: 6) {
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
                                                        if let tt = r.ttftMs {
                                    Text("TTFT \(Self.fmtMs(tt))")
                                                            }
                                if let s = r.tokS, s > 0 {
                                    Text(AppState.fmtRate(s) + " t/s")
                                                            }
                                if r.totalTokens > 0 {
                                    Text("\(AppState.fmtTok(r.totalTokens))(\(r.cachePct ?? 0)%)")
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
            }
            // 高度弹性（2026-10-23）：Recent 卡是面板的"吸收余量"区块——
            // 卡体被 maxHeight:.infinity 给到多少，内滚就拿多少；不再定高
            // .frame(height: 148)
            // 标签示例说明放在**卡内**、列表下方（用户裁决 2026-10-17：
            // 与 Model 列表下的模型标签图例同构，各自归各自卡片，不占面板底部一行）
            requestTagLegend
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    // MARK: - Providers

    private var providersCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                // 标题随维度切换（Providers / Models），tab 也用复数
                sectionLabel("server.rack", state.modelDim ? t("models") : t("providers"))
                // 标明数据窗口（today / yesterday）：顶部指标卡可切换，这里是同一口径。
                // 原先的 "top 6 by usage" 上限已放开（Provider 12 / Model 50，可滚动）
                // → 文案不再成立，改显示窗口（用户裁决 2026-10-17）
                Text(state.statWindowToday ? t("window_today") : t("window_yesterday"))
                    .font(.system(size: 8.5, weight: .medium))
                    .foregroundStyle(.tertiary)
                Spacer(minLength: 0)
                dimTabs
            }
            // 两个维度都在固定高度窗口内滚动（用户裁决 2026-10-17）：行数多了也不撑高面板
            if state.modelDim {
                ScrollView(.vertical, showsIndicators: true) {
                    LazyVStack(alignment: .leading, spacing: 6) {
                        ForEach(state.modelRows) { m in
                            modelRow(m)
                        }
                    }
                }
                .frame(height: providersScrollH)
                // model 的标签示例放在 models 下面（不在面板底部再占一行）
                if state.statWindowToday {
                    VStack(alignment: .leading, spacing: 2) {
                        modelTagLegend
                        prioLegend
                        prioNotice
                    }
                }
            } else {
                let provs = state.providers
                ScrollView(.vertical, showsIndicators: true) {
                    LazyVStack(alignment: .leading, spacing: 6) {
                        ForEach(provs, id: \.id) { p in
                            providerRow(p)
                        }
                    }
                }
                .frame(height: providersScrollH)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    /// 冷却原因压缩成一行短文本（面板空间有限；完整说明仍在 tooltip）
    static func shortCause(_ cause: String) -> String {
        let one = cause.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
        return one.count <= 34 ? one : String(one.prefix(33)) + "…"
    }

    /// 探活倒计时（ms → 人读）：<60s 给秒，否则给分
    static func fmtProbe(_ ms: Int) -> String {
        let s = max(0, ms / 1000)
        return s < 60 ? "\(s)s" : "\(s / 60)m"
    }

    /// 维度切换 tab（用户裁决）：Provider / Model —— 两维度同布局同位置，
    /// 切换零学习成本；窗口切换钮（today/yesterday）在顶部指标卡，两维度共用
    private var dimTabs: some View {
        HStack(spacing: 0) {
            dimTab(t("providers"), selected: !state.modelDim) { state.modelDim = false }
            dimTab(t("models"), selected: state.modelDim) { state.modelDim = true }
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

    /// Model 维度行：与 Provider / Recent 同构的**三行**——
    /// ① 名称 ② 数据（tokens/hit/速率/TTFT）③ 标签（capability/price）
    private func modelRow(_ m: ModelRow) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(Color.green)
                .frame(width: 5, height: 5)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 5) {
                    Text(m.id)
                        .font(.system(size: 11, weight: .medium))
                        .lineLimit(1)
                    prioControl(m)
                    Spacer(minLength: 4)
                    // 报错状态（与 Provider 行同构：状态右对齐）：
                    // 不可用（no-key/removed） > 冷却中 > 失败率
                    if let why = m.unusableReason, !why.isEmpty {
                        Text(why == "no-key" ? t("noKey") : why)
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(.red)
                    } else if m.coolingMs > 0, let msg = m.coolMsg, !msg.isEmpty {
                        // 非账户级失败原因直接显示（如 unsupported / auth failed /
                        // truncated）；quota/payment 属 provider 行 → coolMsg 为 nil，不重复
                        Text(Self.shortCause(msg) + " " + Self.fmtProbe(m.coolingMs))
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(.red)
                    } else if m.coolingMs > 0 {
                        // 冷却但无可用原因（含账户级配额）→ 不显示（provider 行已有）
                        EmptyView()
                    } else if let f = m.failPct, f > 0 {
                        Text("fail \(f)%")
                            .font(.system(size: 9.5, weight: .semibold))
                            .foregroundStyle(.red)
                    }
                }
                .lineLimit(1)
                .minimumScaleFactor(0.8)
                // ② 数据行
                HStack(spacing: 5) {
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
                // ③ 标签行（today 视图才展示：yesterday 的标签是当前态，对历史无意义）
                if state.statWindowToday,
                   (m.capability?.isEmpty == false) || (m.priceTier?.isEmpty == false) {
                    HStack(spacing: 4) {
                        if let cap = m.capability, !cap.isEmpty {
                            tagPill(cap, tint: Self.tagColorCap, bg: Self.tagColorCap.opacity(0.14), help: Self.tagHelpCap)
                        }
                        if let pt = m.priceTier, !pt.isEmpty {
                            tagPill(pt, tint: Self.tagColorPrice, bg: Self.tagColorPrice.opacity(0.14), help: Self.tagHelpPrice)
                        }
                    }
                    .lineLimit(1)
                    .minimumScaleFactor(0.75)
                }
            }
            Spacer(minLength: 0)
        }
    }

    /// Provider 行：与 Recent / Model 同构的**三行**
    /// ① 名称 + 三选一状态（配额进度 / 余额 / 窗口用完警告——互斥，同一行）
    /// ② 各项数据（tokens / hit / 速率 / TTFT）
    /// ③ 标签（档位 / 模型数 / 免费 / 降级 / 去申请 key）
    @ViewBuilder
    private func providerRow(_ p: ProviderRow) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(p.enabled ? (p.available && !p.quotaExhausted ? Color.green : Color.orange) : Color.gray)
                .frame(width: 5, height: 5)
            VStack(alignment: .leading, spacing: 1) {
                // ① 名称 + 状态（三选一，**右对齐**：配额进度/余额/窗口警告贴右边缘）
                HStack(spacing: 5) {
                    Text(p.key)
                        .font(.system(size: 11, weight: .medium))
                        .lineLimit(1)
                        .minimumScaleFactor(0.85)
                    Spacer(minLength: 4)
                    providerStateInline(p)
                }
                .lineLimit(1)
                .minimumScaleFactor(0.8)
                // ② 数据
                providerMetricsLine(p)
                // ③ 标签
                providerLabelsLine(p)
            }
            // 占满可用宽度：让第①行的 Spacer 把状态推到右边缘
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// 三选一状态：配额进度 / 余额 / 窗口用完警告（互斥）。
    /// 昨天/历史视图里这些都是**当前态**，对历史无意义（既有裁决）→ 不显示。
    @ViewBuilder
    private func providerStateInline(_ p: ProviderRow) -> some View {
        if state.statWindowToday {
            if let cd = p.cooldownUntil, cd > Date() {
                let quotaStyle = p.cooldownCause?.contains("quota window exhausted") == true
                Text(quotaStyle ? Self.quotaFullLabel(p.cooldownCause ?? "") : t("cooldown"))
                    .font(.system(size: 9.5, weight: .semibold))
                    .foregroundStyle(.red)
                        Text(Self.fmtCountdown(cd))
                    .font(.system(size: 9, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(Color.red.opacity(0.8))
            } else if !p.blockedModels.isEmpty {
                Text("removed \(p.blockedModels.count)")
                    .font(.system(size: 9.5, weight: .semibold))
                    .foregroundStyle(.red)
                    } else if let used = p.quotaUsed, let scope = p.quotaScope {
                if p.quotaExhausted {
                    Text("\(scope) full")
                        .font(.system(size: 9.5, weight: .semibold))
                        .foregroundStyle(.red)
                    if let reset = p.quotaReset {
                        Text(Self.fmtCountdown(reset))
                            .font(.system(size: 9, design: .rounded))
                            .monospacedDigit()
                            .foregroundStyle(Color.red.opacity(0.8))
                    }
                } else {
                    quotaBar(used).frame(width: 36)
                    Text(Self.quotaPctText(used, src: p.quotaSource))
                        .font(.system(size: 10, design: .rounded))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                            }
            } else if let bal = p.paygBalance {
                Text("\(bal.currency) \(String(format: "%.2f", bal.total))")
                    .font(.system(size: 10, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(bal.available ? Color.secondary : Color.red)
                    } else {
                Text(t("payg"))
                    .font(.system(size: 9.5))
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// ② 数据行：与 Model 行的数据一致
    @ViewBuilder
    private func providerMetricsLine(_ p: ProviderRow) -> some View {
        HStack(spacing: 5) {
            if !p.enabled {
                Text(t("disabled")).font(.system(size: 9.5)).foregroundStyle(.tertiary)
            }
            if p.tokensWin > 0 {
                Text(AppState.fmtTok(p.tokensWin))
                    .font(.system(size: 9.5)).foregroundStyle(.tertiary).monospacedDigit()
            }
            if let cp = p.cachePctWin {
                Text("hit \(cp)%")
                    .font(.system(size: 9.5)).foregroundStyle(.tertiary).monospacedDigit()
            }
            if let rt = p.avgRate, rt > 0 {
                Text(AppState.fmtRate(rt) + " t/s")
                    .font(.system(size: 9.5)).foregroundStyle(.tertiary).monospacedDigit()
            }
            if let tt = p.avgTtftMs {
                Text("TTFT \(Self.fmtMs(tt))")
                    .font(.system(size: 9.5)).foregroundStyle(.tertiary).monospacedDigit()
            }
        }
        .lineLimit(1)
        .minimumScaleFactor(0.75)
    }

    /// ③ 标签行：档位 / 模型数 / 免费 / 降级 / 缺 key
    @ViewBuilder
    private func providerLabelsLine(_ p: ProviderRow) -> some View {
        HStack(spacing: 4) {
            if !displayTier(p).isEmpty {
                Text(displayTier(p))
                    .font(.system(size: 8.5, weight: .medium))
                    .padding(.horizontal, 4).padding(.vertical, 0.5)
                    .background(Capsule().fill(Color.secondary.opacity(0.14)))
                    .foregroundStyle(.secondary)
            }
            if p.modelCount > 0 {
                Text("\(p.modelCount) models")
                    .font(.system(size: 8.5))
                    .foregroundStyle(.quaternary)
            }
            if p.freeModels > 0 {
                Text("\(p.freeModels) free")
                    .font(.system(size: 8.5, weight: .medium))
                    .padding(.horizontal, 4).padding(.vertical, 0.5)
                    .background(Capsule().fill(Color.green.opacity(0.16)))
                    .foregroundStyle(.green)
            }
            if !p.issueLabel.isEmpty {
                Text(t("degraded") + " · " + Self.fmtProbe(p.nextProbeMs))
                    .font(.system(size: 8.5, weight: .medium))
                    .padding(.horizontal, 4).padding(.vertical, 0.5)
                    .background(Capsule().fill(Color.red.opacity(0.16)))
                    .foregroundStyle(.red)
            }
            // 非配额类冷却：原因原文放明面（tooltip 在 MenuBarExtra 基本不触发）。
            // 配额类冷却不重复显示——行①的 `5h full / monthly full` 已经足够。
            if state.statWindowToday,
               let cd = p.cooldownUntil, cd > Date(),
               let cause = p.cooldownCause, !cause.isEmpty,
               !cause.contains("quota window exhausted") {
                Text(Self.shortCause(cause))
                    .font(.system(size: 8.5))
                    .foregroundStyle(Color.red.opacity(0.85))
                    .lineLimit(1)
            }
            if p.needsKey {
                if let url = URL(string: p.keyURL), !p.keyURL.isEmpty {
                    Link(destination: url) {
                        Text(t("getKey"))
                            .font(.system(size: 8.5, weight: .medium))
                            .padding(.horizontal, 4).padding(.vertical, 0.5)
                            .background(Capsule().fill(Color.orange.opacity(0.18)))
                            .foregroundStyle(.orange)
                    }
                    .buttonStyle(.plain)
                } else {
                    Text(t("noKey"))
                        .font(.system(size: 8.5, weight: .medium))
                        .padding(.horizontal, 4).padding(.vertical, 0.5)
                        .background(Capsule().fill(Color.orange.opacity(0.18)))
                        .foregroundStyle(.orange)
                }
            }
        }
        .lineLimit(1)
        .minimumScaleFactor(0.75)
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
            }
            HStack(spacing: 5) {
                gatewayChip
                textChip(label: restarting ? t("restarting") : t("restart"),
                         tint: restarting ? .secondary : .accentColor, help: t("restart_help"),
                         disabled: restarting) {
                    confirmIfInflight("restart") {
                        restarting = true
                        Task {
                            await restartGateway()
                            restarting = false
                        }
                    }
                }
                textChip(label: quitting ? t("quitting") : t("quit"),
                         tint: quitting ? .secondary : .red, help: t("quit"),
                         danger: !quitting, disabled: quitting) {
                    // 网关 SIGTERM 会等排空（≤3s），quitting 态给反馈防重复点
                    quitting = true
                    quitAll()
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
                          danger: Bool = false, disabled: Bool = false,
                          action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 11, weight: .medium))
                .lineLimit(1)
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .frame(maxWidth: .infinity)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(disabled ? 0.03 : 0.055)))
                .foregroundStyle(tint.opacity(disabled ? 0.55 : 1))
        }
        .buttonStyle(.plain)
        .disabled(disabled)
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
    }

    /// 停止/启动 胶囊：healthy → 红调（停止），否则 accent（启动）
    private var gatewayChip: some View {
        Button {
            confirmIfInflight("toggle") { Task { await toggleGateway() } }
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
    }

    private var gatewayChipLabel: String {
        if state.starting { return t("starting") }
        return state.healthy ? t("stop_gw") : t("start_gw")
    }

    /// 品牌徽章 = 官方 v5 彩盘（与菜单栏/桌面图标同源）
    @MainActor
    private var brandBadge: some View {
        // 资源缺失回退空图（不炸）：与 TrayIcon.load 的 ?? NSImage() 同口径
        let img = Bundle.module.url(forResource: "tray-icon-color@2x", withExtension: "png")
            .flatMap { NSImage(contentsOf: $0) } ?? NSImage()
        var sized = img
        sized.size = NSSize(width: 28, height: 28)
        return Image(nsImage: sized).frame(width: 28, height: 28)
    }

    // MARK: - 小件

    private var groupBackground: some View {
        RoundedRectangle(cornerRadius: 10)
            .fill(Color.primary.opacity(0.055))
    }

    private func sectionLabel(_ symbol: String, _ title: String) -> some View {
        HStack(spacing: 5) {
            Image(systemName: symbol)
                .font(.system(size: 9.5, weight: .semibold))
                .foregroundStyle(.secondary)
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
        // 面板内 tooltip 不触发（MenuBarExtra）→ 不挂 .help（每帧多建属性喂 AG 表）
        return t
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
    /// Recent 请求标签的图例（difficulty/domain/flags/fit/mode）——留在 Recent 卡下方
    private var requestTagLegend: some View {
        HStack(spacing: 7) {
            legendItem(Self.tagColorDiff, "difficulty")
            legendItem(Self.tagColorDomain, "domain")
            legendItem(Self.tagColorFlag, "flags")
            legendItem(Self.tagColorFit, "fit")
            legendItem(Self.tagColorMode, "mode")
        }
        .font(.system(size: 8.5))
        .lineLimit(1)
        .minimumScaleFactor(0.8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// 模型标签的图例（capability/price）——放在 Model 列表**下面**（用户裁决 2026-10-17）
    /// 优先级控制：色点（当前档位）+ ▲▼ 微型步进。与 Web dashboard 写同一
    /// /api/priority、读同一 /api/model-tags——双入口同源同步
    private func prioControl(_ m: ModelRow) -> some View {
        let level = state.modelTagMap[m.id]?.priority ?? 0
        return HStack(spacing: 2) {
            Circle()
                .fill(Self.prioColor(level))
                .frame(width: 6, height: 6)
                .help(t("prio_" + Self.prioKeyName(level)))
            VStack(spacing: 0) {
                Button {
                    state.setPriority(model: m.id, level: min(3, level + 1))
                } label: {
                    Image(systemName: "chevron.up")
                        .font(.system(size: 6, weight: .bold))
                        .frame(width: 9, height: 6)
                }
                .buttonStyle(.plain)
                .foregroundStyle(level >= 3 ? .clear : .secondary)
                .disabled(level >= 3)
                Button {
                    state.setPriority(model: m.id, level: max(-3, level - 1))
                } label: {
                    Image(systemName: "chevron.down")
                        .font(.system(size: 6, weight: .bold))
                        .frame(width: 9, height: 6)
                }
                .buttonStyle(.plain)
                .foregroundStyle(level <= -3 ? .clear : .secondary)
                .disabled(level <= -3)
            }
            .help(t("prio_help"))
        }
    }

    /// 档位色阶（低→高 = 冷→暖单调，避开橙红警示色）：灰 → 深蓝 → 天蓝 →
    /// 空心(不干预) → 青 → 绿 → 亮黄绿。色相沿档位单调递进、亮度递增，
    /// Pinned = 最亮的绿（"满格绿灯放行"），不与报错红/警告黄混淆
    static func prioColor(_ l: Int) -> Color {
        switch l {
        case -3: return Color.gray
        case -2: return Color.blue
        case -1: return Color.cyan
        case 1: return Color.teal
        case 2: return Color.green
        case 3: return Color(red: 0.52, green: 0.80, blue: 0.09)  // #84cc16 lime
        default: return Color.secondary.opacity(0.35)  // 0 = 不干预（浅色空心感）
        }
    }

    static func prioKeyName(_ l: Int) -> String {
        l < 0 ? "m\(-l)" : (l == 0 ? "0" : "p\(l)")
    }

    /// 优先级图例：只列**当前有模型在用**的非默认档（省占位；无自定义时不显示）。
    /// 格式 = "Priority:" 前缀 + 多个色点标签，与 Web dashboard 同构
    private var prioLegend: some View {
        let levels = Set(state.modelTagMap.values.map(\.priority)).filter { $0 != 0 }.sorted()
        return Group {
            if !levels.isEmpty {
                HStack(spacing: 7) {
                    Text("Priority:").foregroundStyle(.tertiary)
                    ForEach(levels, id: \.self) { l in
                        HStack(spacing: 3) {
                            Circle().fill(Self.prioColor(l)).frame(width: 6, height: 6)
                            Text(t("prio_" + Self.prioKeyName(l))).foregroundStyle(.tertiary)
                        }
                    }
                }
                .font(.system(size: 8.5))
                .lineLimit(1)
                .minimumScaleFactor(0.8)
            }
        }
    }

    /// 降档提示（同 provider 旧 Pinned 被自动降档时，数秒后自动清除）
    private var prioNotice: some View {
        Group {
            if let n = state.priorityNotice {
                Text(n)
                    .font(.system(size: 8.5))
                    .foregroundStyle(.orange)
                    .lineLimit(2)
                    .minimumScaleFactor(0.8)
            }
        }
    }

    private var modelTagLegend: some View {
        HStack(spacing: 7) {
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

    /// 有在飞请求时的确认（重启会**排空**它们，但用户应当知情；60s 上限后强退）
    /// 确认条已亮时再点同一按钮 = 直接执行（第二击即确认，不用挪手去找橙色按钮）
    /// 在飞确认：同一按钮第二击 = 执行；换按钮 = 改武装到新按钮
    private func confirmIfInflight(_ id: String, _ action: @escaping () -> Void) {
        if state.inflight > 0 {
            if let armed = pendingAction {
                if armed.id == id {
                    pendingAction = nil
                    armed.action()
                } else {
                    pendingAction = (id, action)
                }
            } else {
                pendingAction = (id, action)
            }
        } else {
            action()
        }
    }

    /// 在飞确认行（面板内嵌，替代 MenuBarExtra 里不可用的 Apple 对话框）
    private var inflightConfirmBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 9))
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(String(format: t("inflight_confirm_title"), state.inflight))
                    .font(.system(size: 10, weight: .medium))
                    .lineLimit(1)
                    .minimumScaleFactor(0.85)
                Text(t("inflight_confirm_hint"))
                    .font(.system(size: 8.5))
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
            Button {
                if let p = pendingAction { pendingAction = nil; p.action() }
            } label: {
                Text(t("inflight_confirm_go"))
                    .font(.system(size: 10, weight: .semibold))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(RoundedRectangle(cornerRadius: 7).fill(Color.orange.opacity(0.16)))
            }
            .buttonStyle(.plain)
            Button(t("cancel")) { pendingAction = nil }
                .buttonStyle(.plain)
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color.orange.opacity(0.08)))
    }

    /// 面板"Restart"：停掉网关子进程再拉起（非 [decision] 配置改动靠它生效）
    /// ⚠ stop() 会置 userStopped=true（Stop 修复），重启是"停了马上再要"的
    /// 显式意图 → 拉起前必须清除，否则 ensureRunning 直接 return（实测回归）
    /// ⚠ stop() 只管 owned 子进程（owned=false 时 no-op）→ 再叠加 pgrep 兜底，
    /// 否则"外部实例"误判态下 Restart 永远无效（2026-10-19 实证）
    private func restartGateway() async {
        await gateway.stop()
        await gateway.stopGatewayWithSigterm()
        try? await Task.sleep(nanoseconds: 600_000_000)
        gateway.clearUserStopped()
        await gateway.ensureRunning()
        await state.pollLive()
        await state.pollData()
    }

    private func toggleGateway() async {
        if state.healthy {
            await gateway.stop()
        } else {
            // Start = 用户要它跑：清除 Stop 意图，自愈恢复工作
            gateway.clearUserStopped()
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
        do {
            try p.run()
            p.waitUntilExit()
            if p.terminationStatus != 0 {
                NSLog("evolve-tray: launchctl \(action) failed status=\(p.terminationStatus)")
            }
        } catch {
            NSLog("evolve-tray: launchctl \(action) spawn error: \(error)")
        }
    }

    private func quitAll() {
        Task { @MainActor in
            // 无论本 app 是否 spawn 过网关，退出前都停掉它（守卫在 stop 内部：
            // owned=false 且无子进程 → 直接返回，不会误杀外部实例）
            await GatewayProcess.shared.stop()
            await GatewayProcess.shared.stopGatewayWithSigterm()
            // launchd KeepAlive：SIGKILL 会被当成"崩溃"而自动拉起——
            // 正常退出必须走 NSApp.terminate 自身，不能用 kill
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
