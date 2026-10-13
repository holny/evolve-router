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
            metricsCard
            if !state.recent.isEmpty { decisionsCard }
            if !state.providers.isEmpty { providersCard }
            controls
        }
        .padding(14)
        .frame(width: 336)
        .task {
            if ProcessInfo.processInfo.environment["EV_PREVIEW_STATIC"] == nil {
                await state.pollData()
            }
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
                if let ts = state.lastSync {
                    Text("\(t("last_sync")) \(ts.formatted(date: .omitted, time: .shortened))")
                        .font(.system(size: 10))
                        .foregroundStyle(.tertiary)
                }
            }
            Spacer()
            Button {
                let cfg = """
                baseURL  = \(Config.baseURL)/v1
                model    = auto
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
            Text(t("last_24h"))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
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
                        VStack(alignment: .leading, spacing: 1) {
                            HStack(spacing: 5) {
                                let routing = r.preprocessMs + r.decisionMs
                                let llm = max(0, r.totalMs - (r.ttftMs ?? 0) - routing)
                                Text("ER \(Self.fmtMs(routing))")
                                    .help("EvolveRouter routing decision time")
                                if let tt = r.ttftMs {
                                    Text("TTFT \(Self.fmtMs(tt))")
                                        .help("upstream time-to-first-token")
                                }
                                Text("LLM \(Self.fmtMs(llm))")
                                    .help("upstream generation time")
                                if let s = r.tokS, s > 0 {
                                    Text(String(format: "%.0f tok/s", s))
                                }
                                Text("\(AppState.fmtTok(r.totalTokens))(\(r.cachePct ?? 0)%)")
                                    .help("total tokens (cache hit rate in parens)")
                            }
                            .font(.system(size: 9.5))
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .minimumScaleFactor(0.8)
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
                    Text(Self.fmtMs(r.totalMs))
                        .font(.system(size: 10, design: .rounded))
                        .monospacedDigit()
                        .foregroundStyle(r.status == 200 ? .secondary : Color.red)
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
            sectionLabel("server.rack", t("providers"))
            let provs = state.providers
            ForEach(provs, id: \.id) { p in
                HStack(spacing: 7) {
                    Circle()
                        .fill(p.enabled ? (p.available ? Color.green : Color.orange) : Color.gray)
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
                            if p.tokens5h > 0 {
                                Text("\(AppState.fmtTok(p.tokens5h))·5h")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                                    .monospacedDigit()
                            }
                            if let cp = p.cachePct5h {
                                Text("hit \(cp)%")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                                    .monospacedDigit()
                            }
                            if let rt = p.avgRate {
                                Text(String(format: "%.0f t/s", rt))
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
                            if let until = p.cooldownUntil, until > Date() {
                                let remain = Int(until.timeIntervalSinceNow)
                                let txt = remain >= 3600
                                    ? "\(remain/3600)h \(remain%3600/60)m"
                                    : remain >= 60 ? "\(remain/60)m \(remain%60)s" : "\(remain)s"
                                Text("\(t("cooldown")) \(txt)\(p.cooldownCause.map { c in " · \(c)" } ?? "")")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.red)
                                    .monospacedDigit()
                                    .lineLimit(1)
                                    .help(p.cooldownCause ?? "")
                            }
                    }
                    Spacer(minLength: 0)
                    if let pct = p.pct {
                        quotaBar(pct)
                            .frame(width: 46)
                        Text("\(pct)%")
                            .font(.system(size: 10, design: .rounded))
                            .monospacedDigit()
                            .foregroundStyle(.secondary)
                            .frame(width: 36, alignment: .trailing)
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
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
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

    private func quotaBar(_ pct: Int) -> some View {
        ProgressView(value: Double(min(max(pct, 0), 100)))
            .tint(pct >= 85 ? .orange : .accentColor)
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
    static func fmtMs(_ ms: Int) -> String {
        ms >= 1000 ? String(format: "%.1fs", Double(ms) / 1000) : "\(ms)ms"
    }
}
