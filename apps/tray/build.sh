#!/bin/bash
# EvolveRouter 构建：SwiftPM 编译 → 组 .app bundle → 可选安装
# 用法: apps/tray/build.sh [--install]
set -euo pipefail
cd "$(dirname "$0")"

APP_NAME="EvolveRouter"
BUNDLE_ID="ai.evolve.router.tray"

echo "[1/3] swift build --release ..."
swift build -c release

echo "[2/3] 组装 ${APP_NAME}.app ..."
APP_DIR="build/${APP_NAME}.app/Contents"
mkdir -p "$APP_DIR/MacOS" "$APP_DIR/Resources"
cp .build/release/evolve-router-tray "$APP_DIR/MacOS/${APP_NAME}"

cat > "$APP_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>${APP_NAME}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>LSUIElement</key><true/>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
</dict>
</plist>
PLIST
codesign --force --sign - "build/${APP_NAME}.app" 2>/dev/null || true

if [[ "${1:-}" == "--install" ]]; then
    echo "[3/3] 安装到 /Applications 并启动 ..."
    rm -rf "/Applications/${APP_NAME}.app"
    cp -R "build/${APP_NAME}.app" /Applications/
    open "/Applications/${APP_NAME}.app"
    # 登录自启 + 崩溃拉起（LaunchAgent）
    TRAY_PLIST="$HOME/Library/LaunchAgents/ai.evolve-router.tray.plist"
    mkdir -p "$HOME/Library/LaunchAgents"
    cat > "$TRAY_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>ai.evolve-router.tray</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/${APP_NAME}.app/Contents/MacOS/${APP_NAME}</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>Crashed</key><true/></dict>
</dict>
</plist>
PLIST
    launchctl bootout gui/$(id -u)/ai.evolve-router.tray 2>/dev/null || true
    launchctl bootstrap gui/$(id -u) "$TRAY_PLIST"
    echo "完成：菜单栏出现分支图标即运行中（登录自启可在 系统设置 → 通用 → 登录项 添加）"
else
    echo "[3/3] 跳过安装（--install 可装到 /Applications 并启动）"
    echo "产物: apps/tray/build/${APP_NAME}.app"
fi