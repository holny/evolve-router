import SwiftUI
import AppKit
import TrayCore

@main
struct EvolveRouterApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra {
            PanelView(state: delegate.state, gateway: delegate.gateway)
        } label: {
            TrayIconView(state: delegate.state)
        }
        .menuBarExtraStyle(.window)
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    static let shared = AppDelegate()

    let state = AppState()
    let gateway = GatewayProcess.shared
    private var liveTask: Task<Void, Never>?
    private var dataTask: Task<Void, Never>?
    private var pulseTask: Task<Void, Never>?
    private var bootTask: Task<Void, Never>?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // 双频轮询（live 1s / data 3s）+ 呼吸 0.5s——Task 循环常驻，面板开合不受影响
        liveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.state.pollLive()
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
        dataTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.state.pollData()
                try? await Task.sleep(nanoseconds: 3_000_000_000)
            }
        }
        pulseTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                if self.state.healthy && self.state.inflight > 0 {
                    self.state.pulse.toggle()
                } else if self.state.pulse {
                    self.state.pulse = false
                }
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
        }
        // 启动即拉起网关（幂等；外部实例探测内置）
        bootTask = Task {
            await GatewayProcess.shared.ensureRunning()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        liveTask?.cancel()
        dataTask?.cancel()
        pulseTask?.cancel()
        bootTask?.cancel()
    }
}
