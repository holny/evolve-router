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
            if !state.plans.isEmpty { plansCard }
            if !state.recent.isEmpty { decisionsCard }
            if !state.providers.isEmpty { providersCard }
            controls
        }
        .padding(14)
        .frame(width: 336)
        .task { await state.pollData() }
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
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 28, height: 28)
                .background(Circle().fill(Color.accentColor))
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

    // MARK: - 方案 5h 用量

    private var plansCard: some View {
        VStack(alignment: .leading, spacing: 7) {
            sectionLabel("hourglass", t("plan_5h"))
            ForEach(state.plans, id: \.0) { name, pct in
                HStack(spacing: 8) {
                    planBadge(name)
                    quotaBar(pct)
                    Text(pct > 999 ? ">999%" : "\(pct)%")
                        .font(.system(size: 11, design: .rounded))
                        .monospacedDigit()
                        .foregroundStyle(pct >= 85 ? Color.orange : .secondary)
                        .frame(width: 46, alignment: .trailing)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(groupBackground)
    }

    private func planBadge(_ name: String) -> some View {
        Text(name)
            .font(.system(size: 10, weight: .semibold, design: .rounded))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Capsule().fill(Color.accentColor.opacity(0.14)))
            .foregroundStyle(Color.accentColor)
            .lineLimit(1)
            .minimumScaleFactor(0.8)
    }

    // MARK: - 最近决策

    private var decisionsCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            sectionLabel("sparkle.middle", t("recent"))
            ForEach(state.recent) { r in
                HStack(spacing: 7) {
                    Circle()
                        .fill(r.status == 200 ? Color.green : Color.red)
                        .frame(width: 5, height: 5)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(r.model)
                            .font(.system(size: 11, weight: .medium))
                            .lineLimit(1)
                        HStack(spacing: 6) {
                            Text(r.time)
                                .font(.system(size: 9.5))
                                .foregroundStyle(.tertiary)
                            if let tt = r.ttftMs {
                                Text("TTFT \(tt)ms")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                            }
                            if let s = r.tokS, s > 0 {
                                Text(String(format: "%.0f tok/s", s))
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                            }
                        }
                    }
                    Spacer(minLength: 0)
                    Text(r.status == 200 ? "\(r.totalMs)ms" : t("fail_short"))
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
            ForEach(state.providers) { p in
                HStack(spacing: 7) {
                    Circle()
                        .fill(p.enabled ? (p.available ? Color.green : Color.orange) : Color.gray)
                        .frame(width: 5, height: 5)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(p.key)
                            .font(.system(size: 11, weight: .medium))
                            .lineLimit(1)
                        HStack(spacing: 5) {
                            if let cd = p.cooldownMs, cd > 0 {
                                Text("\(t("cooldown")) \(cd / 60_000)m")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.orange)
                            }
                            if !p.enabled {
                                Text(t("disabled"))
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                            }
                            if p.tokens24h > 0 {
                                Text("\(AppState.fmtTok(p.tokens24h)) · 24h")
                                    .font(.system(size: 9.5))
                                    .foregroundStyle(.tertiary)
                                    .monospacedDigit()
                            }
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
                    } else {
                        Text(p.planKind == "api" ? t("payg") : "—")
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

    private var controls: some View {
        VStack(spacing: 8) {
            gatewayButton
            HStack(spacing: 6) {
                Toggle(t("autostart"), isOn: $autostart)
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .font(.system(size: 11))
                    .onChange(of: autostart) { on in
                        applyAutostart(on)
                    }
                Spacer()
                Link(destination: Config.githubURL) {
                    Image(systemName: "safari")
                        .font(.system(size: 12))
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help(t("github"))
                Button {
                    NSWorkspace.shared.open(URL(string: Config.baseURL)!)
                } label: {
                    Image(systemName: "gauge.with.needle")
                        .font(.system(size: 12))
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help(t("dashboard"))
                Button(role: .destructive) {
                    showExitConfirm = true
                } label: {
                    Image(systemName: "power")
                        .font(.system(size: 12))
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.red)
                .help(t("quit"))
            }
        }
    }

    private var gatewayButton: some View {
        Button {
            Task { await toggleGateway() }
        } label: {
            HStack(spacing: 6) {
                if state.starting {
                    ProgressView()
                        .controlSize(.mini)
                } else {
                    Image(systemName: state.healthy ? "stop.fill" : "play.fill")
                        .font(.system(size: 10, weight: .semibold))
                }
                Text(gatewayButtonLabel)
                    .font(.system(size: 12, weight: .medium))
                Spacer()
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(state.healthy ? Color.red.opacity(0.12) : Color.accentColor.opacity(0.16))
        )
        .foregroundStyle(state.healthy ? Color.red : Color.accentColor)
        .disabled(state.starting)
    }

    private var gatewayButtonLabel: String {
        if state.starting { return t("starting") }
        return state.healthy ? t("stop_gw") : t("start_gw")
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
