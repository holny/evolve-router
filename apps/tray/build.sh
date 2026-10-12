#!/bin/bash
# EvolveRouter 单二进制打包/安装：target/release/evo-router → .app 壳（argv0 免参自动 tray 模式）
# 用法: apps/tray/build.sh [--install]
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd ../.. && pwd)"
APP_NAME="EvolveRouter"
BIN="$ROOT/target/release/evo-router"

echo "[1/2] cargo build（workspace，tray 默认 feature）..."
(cd "$ROOT" && cargo build --release)

echo "[2/2] 组装 ${APP_NAME}.app（同一 evo-router 二进制 + Info 壳）..."
APP_DIR="build/${APP_NAME}.app/Contents"
rm -rf "build/${APP_NAME}.app" && mkdir -p "$APP_DIR/MacOS" "$APP_DIR/Resources"
cp "$BIN" "$APP_DIR/MacOS/${APP_NAME}"
cat > "$APP_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>ai.evolve.router.tray</string>
    <key>CFBundleExecutable</key><string>${APP_NAME}</string>
    <key>CFBundleIconFile</key><string>${APP_NAME}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>LSUIElement</key><true/>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
</dict>
</plist>
PLIST
# 图标管线（源 logo-template.svg → assets/logo-alpha-1024.png；不存在则跳过 icns）
[[ -f "$ROOT/apps/tray/assets/logo-alpha-1024.png" ]] || bash "$ROOT/apps/tray/make-icons.sh" >/dev/null 2>&1 || true
if [[ -f "$ROOT/apps/tray/assets/logo-alpha-1024.png" ]]; then
    ICONSET="build/${APP_NAME}.iconset"
    rm -rf "$ICONSET" && mkdir -p "$ICONSET"
    for s in 16 32 64 128 256 512 1024; do
        sips -z $s $s "$ROOT/apps/tray/assets/logo-alpha-1024.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1 || true
    done
    iconutil -c icns "$ICONSET" -o "$APP_DIR/Resources/${APP_NAME}.icns" 2>/dev/null || true
fi
codesign --force --sign - "build/${APP_NAME}.app" 2>/dev/null || true

if [[ "${1:-}" == "--install" ]]; then
    echo "[3/3] 安装到 /Applications ..."
    launchctl bootout gui/$(id -u)/ai.evolve-router.tray 2>/dev/null || true
    # 收敛单 agent：老双 agent 时代的 serve 网关 agent 移除（tray 同进程已含网关）
    launchctl bootout gui/$(id -u)/ai.evolve-router.gateway 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/ai.evolve-router.gateway.plist"
    pkill -x "${APP_NAME}" 2>/dev/null || true
    sleep 1
    rm -rf "/Applications/${APP_NAME}.app"
    cp -R "build/${APP_NAME}.app" /Applications/
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
    launchctl bootstrap gui/$(id -u) "$TRAY_PLIST"
    pgrep -x "${APP_NAME}" >/dev/null || open "/Applications/${APP_NAME}.app"
    echo "完成：单二进制 .app 已安装（菜单栏图标+同进程网关）"
else
    echo "产物: apps/tray/build/${APP_NAME}.app（唯一可执行 Contents/MacOS = evo-router）"
fi
