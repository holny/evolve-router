#!/bin/bash
# EvolveRouter 图标管线：logo-template.svg（全黑扁平）→ qlmanage 渲染 → alpha 提取 → 各尺寸图标
# 产物：assets/logo-alpha-1024.png（icns 源）+ Resources/tray-icon{,@2x}.png（菜单栏 template）
# 用法: apps/tray/make-icons.sh
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd ../.. && pwd)"
SVG="$ROOT/docs/assets/logo-template.svg"
OUT1024="assets/logo-alpha-1024.png"
RES="Sources/evolve-router-tray/Resources"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "[1/3] qlmanage 渲染 1024 ..."
qlmanage -t -s 1024 -o "$TMP" "$SVG" >/dev/null 2>&1
RAW="$TMP/$(basename "$SVG").png"
[[ -f "$RAW" ]] || { echo "qlmanage 渲染失败"; exit 1; }

echo "[2/3] alpha 提取（255−亮度）..."
swift alpha.swift "$RAW" "$OUT1024"

echo "[3/3] 派生菜单栏/资源尺寸 ..."
sips -z 44 44 "$OUT1024" --out "$RES/tray-icon@2x.png" >/dev/null
sips -z 22 22 "$OUT1024" --out "$RES/tray-icon.png" >/dev/null
echo "完成：$OUT1024 + $RES/tray-icon{,@2x}.png（build.sh --install 后生效）"
