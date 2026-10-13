import AppKit
import SwiftUI

/// 四态模板图标：stopped 暗淡 / starting 呼吸接入 / busy 呼吸 / idle 常态 / fail 叹号 60s
enum TrayIcon {
    // 用户裁决：菜单栏 = 彩盘徽章（v5 几何、盘填渐变与弧同源）——5 张预生成染色资源
    // 在 make-icons 阶段由 docs/assets/logo.svg 派生的 matte 抠像得出，
    // 运行时仅加载，不再做像素级重绘（避开 NSBitmapImageRep 不可靠陷阱）
    static let idleBase: NSImage = load("tray-icon-color@2x")
    static let success: NSImage = load("tray-icon-color-green@2x")
    static let fail: NSImage = load("tray-icon-color-yellow@2x")
    static let stopped: NSImage = load("tray-icon-color-white@2x")
    static let starting: NSImage = load("tray-icon-color-gray@2x")

    private static func load(_ name: String) -> NSImage {
        let url = Bundle.module.url(forResource: name, withExtension: "png")
        let img = url.flatMap { NSImage(contentsOf: $0) } ?? NSImage()
        img.isTemplate = false
        img.size = NSSize(width: 22, height: 22)
        return img
    }

    /// busy 呼吸：给任意 base 图打 alpha（0.45↔1.0 切换）
    static func breath(_ img: NSImage, alpha a: CGFloat) -> NSImage {
        NSImage(size: NSSize(width: 22, height: 22), flipped: false) { _ in
            guard let ctx = NSGraphicsContext.current?.cgContext,
                  let cg = img.cgImage(forProposedRect: nil, context: nil, hints: nil) else { return true }
            ctx.interpolationQuality = .high
            ctx.setAlpha(a)
            ctx.draw(cg, in: CGRect(x: 0, y: 0, width: 22, height: 22))
            return true
        }
    }
}

public struct TrayIconView: View {
    @ObservedObject public var state: AppState

    public init(state: AppState) {
        _state = ObservedObject(wrappedValue: state)
    }

    public var body: some View {
        let ns: NSImage = Self.render(state: state.iconState, pulse: state.pulse)
        Image(nsImage: ns)
            .accessibilityLabel(accessibilityText)
    }

    /// 5 态形态不变、只换色+呼吸：
    /// - stopped → 全灰
    /// - starting → 全灰 + 0.45↔1 呼吸
    /// - busy   → 沿用 last-result 染色（白/绿/黄） + 呼吸
    /// - idle   → last-result 染色；无前置结果时为白 ER
    static func render(state: IconState, pulse: Bool) -> NSImage {
        switch state {
        case .stopped:
            return TrayIcon.stopped
        case .starting:
            return pulse ? TrayIcon.breath(TrayIcon.starting, alpha: 0.45) : TrayIcon.starting
        case .busy:
            // 路由中固定绿 ER（用户裁决：绿只在路由时显示，请求结束回白）
            return pulse ? TrayIcon.breath(TrayIcon.success, alpha: 0.45) : TrayIcon.success
        case .fail:
            // idle+失败：黄色 ER（持续到下次请求开始）
            return TrayIcon.fail
        case .idle, .success:
            return TrayIcon.idleBase
        }
    }

    private var accessibilityText: String {
        switch state.iconState {
        case .stopped: return "EvolveRouter — " + t("stopped")
        case .fail: return "EvolveRouter — " + t("gateway_err")
        case .success: return "EvolveRouter — " + t("healthy")
        case .busy, .idle, .starting: return "EvolveRouter — " + t("healthy")
        }
    }
}
