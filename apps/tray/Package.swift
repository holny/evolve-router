// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "evolve-tray",
    platforms: [.macOS(.v13)],
    targets: [.executableTarget(name: "evolve-tray", path: "Sources/evolve-tray")]
)
