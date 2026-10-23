#!/bin/bash
# EvolveRouter 网关：编译 → 部署 → 重启 一键脚本
# 用法: ./scripts/deploy.sh          # 编译+部署+重启
#       ./scripts/deploy.sh --no-build   # 只重启（不编译）
set -euo pipefail

BIN_SRC="target/release/evo-router"
BIN_DST="$HOME/.local/bin/evo-router"
LABEL="ai.evolve-router.gateway"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
# 健康检查端口：LaunchAgent 内嵌安装期 --port（service.rs write_plist），
# 非默认端口部署时用 PORT=xxxx ./scripts/deploy.sh 对齐
PORT="${PORT:-8787}"

if [[ "${1:-}" != "--no-build" ]]; then
    echo "[1/4] cargo build --release ..."
    build_start=$(date +%s)
    cargo build --release
    # 防呆：确认二进制真的是刚构建的（SIGPIPE 掐死构建的教训）。
    # 以构建起始时刻为基准——无变更的 no-op 构建不重链是正常情形，不再误报
    # （review 2026-10-23：旧实现与当前时间比，no-op 构建必炸）
    src_mtime=$(stat -f %m "$BIN_SRC")
    if (( src_mtime + 5 < build_start )); then
        echo "警告: $BIN_SRC 在本次构建中未被重链（无变更或构建被截断），继续部署" >&2
    fi
fi

echo "[2/4] 部署到 $BIN_DST ..."
# 原地 cp 覆盖已签名二进制 → macOS 代码签名失效 → exec 被 SIGKILL。
# 必须 rm（换 inode）+ codesign 临时重签。
# review 修复：LaunchAgent 启动会做 Launch Constraint 签名校验——
# quarantine 属性与签名失效都会被 SIGKILL（Code Signature Invalid）。
# 先清属性再重签；注意作用对象是新落盘的文件（旧实现 xattr 在 rm 之前，
# 清的是马上要删的旧 inode，等于没清——review 2026-10-23）
rm -f "$BIN_DST"
cp "$BIN_SRC" "$BIN_DST"
xattr -cr "$BIN_DST" 2>/dev/null || true
codesign --force --sign - "$BIN_DST" 2>/dev/null || true

echo "[3/4] 重启 LaunchAgent ..."
# 前置守卫：agent 从未安装时 kickstart 必失败（review 2026-10-23）——
# 给出可执行的修复指引而不是裸报错
if [[ ! -f "$PLIST" ]]; then
    echo "错误: LaunchAgent 未安装（$PLIST 缺失）。先运行: $BIN_DST service install" >&2
    exit 1
fi
launchctl kickstart -k "gui/$(id -u)/$LABEL"

echo "[4/4] 健康检查 ..."
for _ in $(seq 1 25); do
    sleep 1
    code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/healthz" || true)
    if [[ "$code" == "200" ]]; then
        echo "完成: 网关已就绪 (http://127.0.0.1:$PORT)"
        exit 0
    fi
done
echo "警告: 健康检查未通过（25s 内无 200），查看 ~/.evo-router/logs/gateway.log" >&2
exit 1
