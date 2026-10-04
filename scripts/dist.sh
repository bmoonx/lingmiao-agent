#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# dist.sh — 生成「单二进制分发包」资产，供上传到 GitHub Release。
#
# 产物：target/release/lingmiao 打成
#   <out-dir>/lingmiao-<version>-<plat>.tar.gz
#   <out-dir>/lingmiao-<version>-<plat>.tar.gz.sha256
# 由仓库根目录的 install.sh 从 Release 下载、校验、解压。
#
# 用法：
#   scripts/dist.sh <version> [out-dir]
#   scripts/dist.sh 0.12.44
#
# 前置：先 `cargo build --release`（本脚本不负责构建，只负责打包分发）。
#
# 上传（需凭据，择一）：
#   gh release create v<version> <out-dir>/*.tar.gz <out-dir>/*.sha256 --title "v<version>"
#   # 或 GitHub API: POST /repos/<owner>/<repo>/releases + upload 资产
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

VER="${1:?用法: dist.sh <version> [out-dir]}"
OUT="${2:-$PWD/dist}"
PLAT="linux-x86_64"
NAME="lingmiao-${VER}-${PLAT}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/target/release/lingmiao"

[ -x "$SRC" ] || { echo "✗ 未找到 $SRC，请先 cargo build --release"; exit 1; }

echo "▸ 打包 $NAME"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/$NAME"
cp "$SRC" "$TMP/$NAME/lingmiao"
chmod 755 "$TMP/$NAME/lingmiao"
tar -C "$TMP" -czf "$TMP/$NAME.tar.gz" "$NAME"
chmod 644 "$TMP/$NAME.tar.gz"

mkdir -p "$OUT"
cp "$TMP/$NAME.tar.gz" "$OUT/"
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

echo "▸ 完成，产物在 $OUT"
ls -la "$OUT/$NAME.tar.gz" "$OUT/$NAME.tar.gz.sha256"
