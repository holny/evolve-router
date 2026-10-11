// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "evolve-router-tray",
    platforms: [.macOS(.v13)],
    targets: [.executableTarget(name: "evolve-router-tray", path: "Sources/evolve-router-tray", resources: [.process("Resources")])]
)
