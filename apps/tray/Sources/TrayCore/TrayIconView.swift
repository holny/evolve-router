import AppKit
import SwiftUI

/// 四态模板图标：stopped 暗淡 / starting 呼吸接入 / busy 呼吸 / idle 常态 / fail 叹号 60s
enum TrayIcon {
    static let base: NSImage = {
        let url = Bundle.module.url(forResource: "tray-icon@2x", withExtension: "png")
        let img = url.flatMap { NSImage(contentsOf: $0) } ?? NSImage()
        img.isTemplate = true
        img.size = NSSize(width: 22, height: 22)
        return img
    }()

    /// 按 alpha 重绘（template 只取 alpha 通道：alpha 缩放 = 菜单栏内变淡）
    static func alpha(_ a: CGFloat) -> NSImage {
        NSImage(size: NSSize(width: 22, height: 22), flipped: false) { _ in
            if let ctx = NSGraphicsContext.current?.cgContext,
               let cg = base.cgImage(forProposedRect: nil, context: nil, hints: nil) {
                ctx.interpolationQuality = .high
                ctx.setAlpha(a)
                ctx.draw(cg, in: CGRect(x: 0, y: 0, width: 22, height: 22))
            }
            return true
        }
    }
    static let idle = alpha(1.0)
    static let dim = alpha(0.35)
    static let busy = alpha(0.45)

    static let fail: NSImage = {
        let cfg = NSImage.SymbolConfiguration(pointSize: 16, weight: .medium)
        let img = (NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: t("gateway_err"))?
            .withSymbolConfiguration(cfg)) ?? NSImage()
        img.isTemplate = true
        return img
    }()

    /// starting：端口符号，接续呼吸由 alpha 翻转驱动
    static let startingBusy = busy
}

public struct TrayIconView: View {
    @ObservedObject public var state: AppState

    public init(state: AppState) {
        _state = ObservedObject(wrappedValue: state)
    }

    public var body: some View {
        let ns: NSImage = {
            switch state.iconState {
            case .stopped: return TrayIcon.dim
            case .fail:    return TrayIcon.fail
            case .busy:    return state.pulse ? TrayIcon.busy : TrayIcon.idle
            case .starting: return state.pulse ? TrayIcon.busy : TrayIcon.dim
            case .idle:    return TrayIcon.idle
            }
        }()
        Image(nsImage: ns)
            .accessibilityLabel(accessibilityText)
    }

    private var accessibilityText: String {
        switch state.iconState {
        case .stopped: return "EvolveRouter — " + t("stopped")
        case .fail: return "EvolveRouter — " + t("gateway_err")
        case .starting: return "EvolveRouter — " + t("starting")
        case .busy, .idle: return "EvolveRouter — " + t("healthy")
        }
    }
}
