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
        state.recent = [
            RecentDecision(model: "opencode-go-github/glm-5.3", status: 200, totalMs: 13_540, ttftMs: 5_012, tokS: 92.4, agent: "opencode", time: "22:58"),
            RecentDecision(model: "zhipuai-coding-plan/glm-5.3", status: 500, totalMs: 8_214, ttftMs: 4_410, tokS: nil, agent: "pi", time: "22:41"),
        ]
        state.providers = [
            ProviderRow(key: "opencode-go", enabled: true, available: true, cooldownUntil: nil, cooldownCause: nil, pct: 4, tokens24h: 92_000, planKind: "go"),
            ProviderRow(key: "zhipuai-coding-plan", enabled: true, available: false, cooldownUntil: Date().addingTimeInterval(8_520), cooldownCause: "transient error (cooling down)", pct: 71, tokens24h: 51_000, planKind: "coding"),
            ProviderRow(key: "minimax-coding", enabled: false, available: true, cooldownUntil: nil, cooldownCause: nil, pct: nil, tokens24h: 0, planKind: "coding"),
        ]
        state.lastSync = Date()

        let win = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 336, height: 600),
            styleMask: [.titled, .resizable],
            backing: .buffered, defer: false
        )
        win.title = "Panel — \(scheme == .dark ? "dark" : "light")"
        win.isReleasedWhenClosed = false
        win.backgroundColor = scheme == .dark
            ? NSColor(deviceWhite: 0.08, alpha: 0.96)
            : NSColor(deviceWhite: 0.94, alpha: 0.96)
        let host = NSHostingView(
            rootView: PanelView(state: state, gateway: .shared)
                .environment(\.colorScheme, scheme)
        )
        win.contentView = host
        win.center()
        win.makeKeyAndOrderFront(nil)
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
