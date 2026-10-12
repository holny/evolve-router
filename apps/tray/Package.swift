// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "evolve-router-tray",
    platforms: [.macOS(.v13)],
    targets: [
        // 共享 UI 核心（面板/状态/网关管理/i18n/图标）
        .target(
            name: "TrayCore",
            path: "Sources/TrayCore",
            resources: [.process("Resources")]
        ),
        // 菜单栏壳（@main 入口）
        .executableTarget(
            name: "evolve-router-tray",
            dependencies: ["TrayCore"],
            path: "Sources/EvolveRouterApp"
        ),
        // 开发 harness：假数据渲染真实 PanelView，双主题截图自验设计
        .executableTarget(
            name: "panel-preview",
            dependencies: ["TrayCore"],
            path: "Sources/PanelPreview"
        ),
    ]
)
