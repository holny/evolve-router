#!/bin/bash
# EvolveRouter macOS universal2 发布打包：双架构 Rust 单文件 + universal2 合并 + 双芯 .app + 校验和
# 用法: scripts/release-macos.sh <version>  （产物落到 apps/tray/build/release/）
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)/.."
APP_NAME="EvolveRouter"
VER="${1:?用法: release-macos.sh <version>，例 v0.1.1}"
OUT="apps/tray/build/release"
rm -rf "$OUT" && mkdir -p "$OUT"

echo "[1/6] cargo 双架构 release 构建..."
for T in aarch64-apple-darwin x86_64-apple-darwin; do
    if [[ -f "target/$T/release/evo-router" ]]; then
        echo "  $T 已存在，跳过"
    else
        echo "  building $T ..."
        cargo build -p evolve-server --release --target "$T"
    fi
done

echo "[2/6] lipo universal 单文件..."
lipo -create target/aarch64-apple-darwin/release/evo-router target/x86_64-apple-darwin/release/evo-router -output "$OUT/evo-router"

echo "[3/6] Swift 壳双芯构建 + 组装 .app..."
( cd apps/tray && swift build -c release --triple arm64-apple-macosx >/dev/null 2>&1 || true )
( cd apps/tray && swift build -c release --triple x86_64-apple-macosx )
lipo -create apps/tray/.build/arm64-apple-macosx/release/evolve-router-tray apps/tray/.build/x86_64-apple-macosx/release/evolve-router-tray -output apps/tray/build/evolve-router-tray-universal
SWIFT_UNI="apps/tray/build/evolve-router-tray-universal"
APP="apps/tray/build/${APP_NAME}.app"
APPDIR="$APP/Contents"
rm -rf "$APP" && mkdir -p "$APPDIR/MacOS" "$APPDIR/Resources"
cp "$SWIFT_UNI" "$APPDIR/MacOS/${APP_NAME}"
cp "$OUT/evo-router" "$APPDIR/MacOS/evo-router"
chmod +x "$APPDIR/MacOS/${APP_NAME}" "$APPDIR/MacOS/evo-router"
[[ -f apps/tray/assets/logo-alpha-1024.png ]] || bash apps/tray/make-icons.sh >/dev/null
if [[ -f apps/tray/assets/logo-alpha-1024.png ]]; then
    ICONSET="apps/tray/build/${APP_NAME}.iconset"
    rm -rf "$ICONSET" && mkdir -p "$ICONSET"
    for s in 16 32 64 128 256 512 1024; do
        sips -z $s $s apps/tray/assets/logo-alpha-1024.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1 || true
    done
    iconutil -c icns "$ICONSET" -o "$APPDIR/Resources/${APP_NAME}.icns" 2>/dev/null || true
fi
cat > "$APPDIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>ai.evolve.router.tray</string>
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

echo "[4/6] 归档产物..."
cp target/aarch64-apple-darwin/release/evo-router "$OUT/evo-router-aarch64"
cp target/x86_64-apple-darwin/release/evo-router "$OUT/evo-router-x86_64"
(
  cd "$OUT"
  tar -czf "evo-router-$VER-aarch64-apple-darwin.tar.gz" evo-router-aarch64
  tar -czf "evo-router-$VER-x86_64-apple-darwin.tar.gz" evo-router-x86_64
  tar -czf "evo-router-$VER-universal2-apple-darwin.tar.gz" evo-router
  rm -f evo-router-aarch64 evo-router-x86_64
)
ditto -c -k --keepParent "$APP" "$OUT/$APP_NAME-$VER.zip"

echo "[5/6] 校验和..."
( cd "$OUT" && shasum -a 256 * > SHA256SUMS.txt )

echo "[6/6] 完成。清单："
ls -lh "$OUT"
