#!/bin/bash
# EvolveRouter B-路线打包：Swift 壳（MenuBarExtra 原生面板）+ Rust 网关 helper → 双 Mach-O 的 .app
# 结构: Contents/MacOS/{EvolveRouter(壳,exe) evo-router(serve helper)}
# 用法: apps/tray/build.sh [--install]
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd ../.. && pwd)"
APP_NAME="EvolveRouter"
SHELL_BIN="apps/tray/.build/release/evolve-router-tray"
GATEWAY_BIN="$ROOT/target/release/evo-router"

echo "[1/4] cargo build（workspace，网关 helper）..."
(cd "$ROOT" && cargo build --release)

echo "[2/4] swift build（壳，MenuBarExtra 原生面板）..."
swift build -c release --package-path "$ROOT/apps/tray"
[[ -f "$ROOT/$SHELL_BIN" ]] || { echo "壳二进制缺失: $SHELL_BIN"; exit 1; }

echo "[3/4] 组装 ${APP_NAME}.app（双二进制 + Info 壳）..."
APP_DIR="build/${APP_NAME}.app/Contents"
rm -rf "build/${APP_NAME}.app" && mkdir -p "$APP_DIR/MacOS" "$APP_DIR/Resources"
cp "$ROOT/$SHELL_BIN" "$APP_DIR/MacOS/${APP_NAME}"
cp "$GATEWAY_BIN" "$APP_DIR/MacOS/evo-router"
chmod +x "$APP_DIR/MacOS/${APP_NAME}" "$APP_DIR/MacOS/evo-router"
cat > "$APP_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>ai.evolve-router.tray</string>
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
[[ -f "$ROOT/docs/assets/logo-color.png" ]] || bash "$ROOT/apps/tray/make-icons.sh" >/dev/null 2>&1 || true
if [[ -f "$ROOT/docs/assets/logo-color.png" ]]; then
    ICONSET="build/${APP_NAME}.iconset"
    rm -rf "$ICONSET" && mkdir -p "$ICONSET"
    for s in 16 32 64 128 256 512 1024; do
        sips -z $s $s "$ROOT/docs/assets/logo-color.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1 || true
    done
    iconutil -c icns "$ICONSET" -o "$APP_DIR/Resources/${APP_NAME}.icns" 2>/dev/null || true
fi
# ad-hoc 签名两个二进制（arm64 子进程 exec 的硬门槛）+ 整体
codesign --force --sign - "$APP_DIR/MacOS/evo-router"
codesign --force --sign - "$APP_DIR/MacOS/${APP_NAME}"
codesign --force --sign - "build/${APP_NAME}.app" 2>/dev/null || true

if [[ "${1:-}" == "--install" ]]; then
    echo "[4/4] 安装到 /Applications ..."
    launchctl bootout gui/$(id -u)/ai.evolve-router.tray 2>/dev/null || true
    launchctl bootout gui/$(id -u)/ai.evolve-router.gateway 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/ai.evolve-router.gateway.plist"
    pkill -x "${APP_NAME}" 2>/dev/null || true
    pkill -f "evo-router serve" 2>/dev/null || true
    # 确认旧壳真死了（SIGTERM 偶发被 launchd 竞速拉回/未送达）
    for i in $(seq 1 10); do
        pgrep -x "${APP_NAME}" >/dev/null 2>&1 || break
        sleep 0.5
        [[ $i -eq 10 ]] && pkill -9 -x "${APP_NAME}" 2>/dev/null || true
    done
    # 等端口真释放（SIGTERM 优雅停机可拖 >1.5s；不等于新壳会误判为"外部实例"而不拉网关）
    for i in $(seq 1 10); do
        curl -s -m 1 "http://127.0.0.1:${EV_PORT:-8787}/healthz" >/dev/null 2>&1 || break
        sleep 0.5
        [[ $i -eq 10 ]] && pkill -9 -f "evo-router serve" 2>/dev/null || true
    done
    rm -rf "/Applications/${APP_NAME}.app"
    cp -R "build/${APP_NAME}.app" /Applications/
    TRAY_PLIST="$HOME/Library/LaunchAgents/ai.evolve-router.tray.plist"
    mkdir -p "$HOME/Library/LaunchAgents"
    # 用户裁决 2026-10-18：不用 zsh 监督循环（外部进程依赖）。
    # launchd KeepAlive 在 macOS 26 实测复活不可靠（SIGKILL/SIGABRT 均未复活），
    # 但 app 死亡时网关子进程活不受影响、下次登录 RunAtLoad 自愈——可接受的损失
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
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><true/></dict>
</dict>
</plist>
PLIST
    launchctl bootstrap gui/$(id -u) "$TRAY_PLIST"
    sleep 2
    pgrep -x "${APP_NAME}" >/dev/null || open "/Applications/${APP_NAME}.app"
    echo "完成：${APP_NAME}.app 已安装（Swift 壳面板 + evo-router serve 子进程网关）"
else
    echo "产物: apps/tray/build/${APP_NAME}.app（exe=Swift 壳；MacOS/evo-router=网关 helper）"
fi
