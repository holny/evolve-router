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
            TrayIconView(icon: delegate.icon)
        }
        .menuBarExtraStyle(.window)
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    static let shared = AppDelegate()

    let state = AppState()
    /// 图标轻模型：pulse/失败色只打菜单栏小图，不再唤醒面板视图图
    let icon = IconModel()
    let gateway = GatewayProcess.shared
    private var liveTask: Task<Void, Never>?
    private var dataTask: Task<Void, Never>?
    private var pulseTask: Task<Void, Never>?
    private var bootTask: Task<Void, Never>?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // 双频轮询（live 1s / data 3s，data 关面板降频 30s）+ 呼吸 0.5s——
        // Task 循环常驻，面板开合不受影响
        liveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.state.pollLive(icon: self.icon)
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
        dataTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.state.pollData()
                // 关面板 = 只养快照（15s 一轮，零发布零布局；数据越新，
                // 打开面板时 flush 的差异越小 = 打开越轻）；开着才 3s 实时
                let cadence: UInt64 = self.state.panelVisible ? 3_000_000_000 : 15_000_000_000
                try? await Task.sleep(nanoseconds: cadence)
            }
        }
        pulseTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                if self.icon.healthy && self.icon.inflight > 0 {
                    self.icon.pulse.toggle()
                } else if self.icon.pulse {
                    self.icon.pulse = false
                }
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
        }
        // 启动即拉起网关（幂等；外部实例探测内置）
        bootTask = Task {
            await GatewayProcess.shared.ensureRunning()
        }
        // 启动预热：跑一次完整 pollData 并直接发布，养熟 published，
        // 避免首次点击面板时“空白首帧 → flush → 重排 + 窗口变高”的卡顿。
        bootTask = Task {
            await GatewayProcess.shared.ensureRunning()
            await state.bootstrapPublishedOnce()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        liveTask?.cancel()
        dataTask?.cancel()
        pulseTask?.cancel()
        bootTask?.cancel()
    }
}
