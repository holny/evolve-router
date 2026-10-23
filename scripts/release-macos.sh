#!/bin/bash
# EvolveRouter macOS universal2 发布打包：双架构 Rust 单文件 + universal2 合并 + 双芯 .app + 校验和
# 用法: scripts/release-macos.sh <version>  （产物落到 apps/tray/build/release/）
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)/.."
APP_NAME="EvolveRouter"
VER="${1:?用法: release-macos.sh <version>，例 v0.1.1}"
OUT="apps/tray/build/release"
rm -rf "$OUT" && mkdir -p "$OUT"

echo "[1/6] cargo 双架构 release 构建（总是构建：cargo 自身做增量，
        早期「存在即跳过」曾把 10-13 的旧二进制打进 v0.1.3 发布包）..."
for T in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "  building $T ..."
    cargo build -p evolve-server --release --target "$T"
done

echo "[2/6] lipo universal 单文件 + 版本校验（防止脏产物进包）..."
lipo -create target/aarch64-apple-darwin/release/evo-router target/x86_64-apple-darwin/release/evo-router -output "$OUT/evo-router"
GOT="$("$OUT/evo-router" --version | awk '{print $2}')"
WANT="${VER#v}"
if [[ "$GOT" != "$WANT" ]]; then
    echo "  !! 版本不符：打包内网关 = $GOT，期望 = $WANT（检查 Cargo.toml version 与 target 缓存）" >&2
    exit 1
fi
echo "  packaged gateway version: $GOT  ✓"

echo "[3/6] Swift 壳双芯构建 + 组装 .app..."
# 双架构同等失败即停（静默吞错会让 lipo 阶段才炸，难定位）
( cd apps/tray && swift build -c release --triple arm64-apple-macosx )
( cd apps/tray && swift build -c release --triple x86_64-apple-macosx )
lipo -create apps/tray/.build/arm64-apple-macosx/release/evolve-router-tray apps/tray/.build/x86_64-apple-macosx/release/evolve-router-tray -output apps/tray/build/evolve-router-tray-universal
SWIFT_UNI="apps/tray/build/evolve-router-tray-universal"
APP="apps/tray/build/${APP_NAME}.app"
APPDIR="$APP/Contents"
rm -rf "$APP" && mkdir -p "$APPDIR/MacOS" "$APPDIR/Resources"
cp "$SWIFT_UNI" "$APPDIR/MacOS/${APP_NAME}"
cp "$OUT/evo-router" "$APPDIR/MacOS/evo-router"
chmod +x "$APPDIR/MacOS/${APP_NAME}" "$APPDIR/MacOS/evo-router"
if [[ -f docs/assets/logo-color.png ]]; then
    ICONSET="apps/tray/build/${APP_NAME}.iconset"
    rm -rf "$ICONSET" && mkdir -p "$ICONSET"
    # Apple 规范 iconset 文件名（icon_NxN.png + @2x；旧实现 64x64/1024x1024
    # 不是合法成员，iconutil 会静默丢弃或报错——review 2026-10-23）
    ic() { sips -z "$2" "$2" docs/assets/logo-color.png --out "$ICONSET/icon_$1" >/dev/null 2>&1 || true; }
    ic "16x16.png" 16
    ic "16x16@2x.png" 32
    ic "32x32.png" 32
    ic "32x32@2x.png" 64
    ic "128x128.png" 128
    ic "128x128@2x.png" 256
    ic "256x256.png" 256
    ic "256x256@2x.png" 512
    ic "512x512.png" 512
    ic "512x512@2x.png" 1024
    iconutil -c icns "$ICONSET" -o "$APPDIR/Resources/${APP_NAME}.icns" 2>/dev/null || true
else
    # make-icons.sh 消费 logo-color.png 而不是生成它——旧回退永远不成立
    echo "警告: docs/assets/logo-color.png 缺失，跳过 .icns 生成（.app 无自定义图标）" >&2
fi
cat > "$APPDIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>ai.evolve-router.tray</string>
    <key>CFBundleExecutable</key><string>${APP_NAME}</string>
    <key>CFBundleIconFile</key><string>${APP_NAME}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VER#v}</string>
    <key>LSUIElement</key><true/>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$APPDIR/MacOS/evo-router"
codesign --force --sign - "$APPDIR/MacOS/${APP_NAME}"
codesign --force --sign - "$APP" 2>/dev/null || true

echo "[4/6] 归档产物（用户裁决：只发 .app zip + 校验和，单文件 tar 停发）..."
# 资产名标注平台（与 Homebrew cask 的 URL 模板一致）：
# EvolveRouter-v<ver>-macos-universal2.zip
ditto -c -k --keepParent "$APP" "$OUT/$APP_NAME-$VER-macos-universal2.zip"

echo "[5/6] 校验和（仅发布资产；不含中间产物 evo-router）..."
( cd "$OUT" && shasum -a 256 "$APP_NAME-$VER-macos-universal2.zip" > SHA256SUMS.txt )

echo "[6/6] 完成。清单："
ls -lh "$OUT"
