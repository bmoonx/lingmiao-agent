#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# install.sh — 从本仓库取回 lingmiao 预编译二进制（分块存放，无需 Releases）。
#
# 本仓库不放大文件：约 140 MB 的可执行文件（含内嵌嵌入模型）被切成多块小
# 文件放在 release/ 目录，每块都在 GitHub 单文件限制以下。本脚本负责
# 「校验 → 合并 → 解压 → 就位」，产出可直接运行的 ./lingmiao。
#
# 用法（两种，任选）：
#   1) 先克隆仓库再跑（推荐，无需额外网络）：
#        git clone https://github.com/bmoonx/lingmiao.git
#        cd lingmiao && ./install.sh
#
#   2) 不克隆，直接从 GitHub 拉分块（只取二进制）：
#        ./install.sh --from-github
#
# 选项：
#   --from-github     从 GitHub raw 逐块下载，而不是用本目录 release/
#   --out DIR         解压到 DIR（默认 ./dist）
#   --version VER     指定版本（默认读 release/VERSION）
#   --keep            保留下载/合并的中间文件
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="bmoonx/lingmiao"
RAW_BASE="https://raw.githubusercontent.com/${REPO}/main"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FROM_GITHUB=0
OUT="$SCRIPT_DIR/dist"
VERSION=""
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --from-github) FROM_GITHUB=1 ;;
    --out)         OUT="$2"; shift ;;
    --version)     VERSION="$2"; shift ;;
    --keep)        KEEP=1 ;;
    -h|--help)     sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "✗ 未知参数：$1（-h 看用法）" >&2; exit 2 ;;
  esac
  shift
done

case "$(uname -s)" in
  Linux)  OS=linux ;;
  *) echo "✗ 暂不支持的系统：$(uname -s)。当前仅提供 Linux x86_64 的预编译二进制（推荐 Ubuntu 24.04 LTS）。" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  *) echo "✗ 暂不支持的架构：$(uname -m)。当前仅提供 x86_64（amd64）。" >&2; exit 1 ;;
esac
PLAT="${OS}-${ARCH}"

# ── 系统兼容性：二进制动态链接 glibc，要求 glibc >= 2.39（Ubuntu 24.04 自带）──
if command -v ldd >/dev/null 2>&1; then
  HAVE_GLIBC="$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)"
  if [ -n "$HAVE_GLIBC" ] && [ "$(printf '%s\n' 2.39 "$HAVE_GLIBC" | sort -V | head -1)" != "2.39" ]; then
    echo "✗ 本机 glibc ${HAVE_GLIBC} 过低：该二进制要求 glibc >= 2.39。" >&2
    echo "  推荐在 Ubuntu 24.04 LTS 上运行（或在目标机自行构建）。" >&2
    exit 1
  fi
fi

SRC_DIR="$SCRIPT_DIR/release"
TMP="$(mktemp -d)"
trap '[ "$KEEP" = 1 ] || rm -rf "$TMP"' EXIT

# ── 1. 确定版本 ──────────────────────────────────────────────────────────────
if [ -z "$VERSION" ]; then
  if [ -f "$SRC_DIR/VERSION" ]; then
    VERSION="$(tr -d '[:space:]' < "$SRC_DIR/VERSION")"
  else
    echo "✗ 找不到 release/VERSION；用 --version 指定。" >&2; exit 1
  fi
fi
NAME="lingmiao-${VERSION}-${PLAT}"
TARBALL="${NAME}.tar.gz"
echo "▸ 目标：${NAME}"

# ── 2. 收集分块（本地 或 从 GitHub 下载）─────────────────────────────────────
if [ "$FROM_GITHUB" = 1 ]; then
  echo "▸ 从 GitHub 下载分块（${RAW_BASE}/release/）"
  i=0
  while :; do
    part="${TARBALL}.part-$(printf '%02d' "$i")"
    if curl -fsSL "${RAW_BASE}/release/${part}" -o "$TMP/${part}" 2>/dev/null; then
      echo "  · ${part}"
    else
      rm -f "$TMP/${part}"; break
    fi
    i=$((i+1))
  done
  [ "$i" -gt 0 ] || { echo "✗ 未下载到任何分块（网络或路径问题）。" >&2; exit 1; }
  curl -fsSL "${RAW_BASE}/release/SHA256SUMS" -o "$TMP/SHA256SUMS"
else
  echo "▸ 使用本目录 release/ 中的分块"
  [ -d "$SRC_DIR" ] || { echo "✗ 找不到 $SRC_DIR（若只想下载二进制，用 --from-github）" >&2; exit 1; }
  cp "$SRC_DIR/${TARBALL}.part-"* "$TMP"/
  cp "$SRC_DIR/SHA256SUMS" "$TMP"/ 2>/dev/null || true
fi

# ── 3. 校验分块（逐块 sha256）───────────────────────────────────────────────
if [ -f "$TMP/SHA256SUMS" ]; then
  echo "▸ 校验分块完整性"
  ( cd "$TMP" && sha256sum -c --ignore-missing SHA256SUMS >/dev/null ) \
    && echo "  · 分块校验通过" \
    || { echo "✗ 校验失败：下载不完整或文件损坏，请重试。" >&2; exit 1; }
fi

# ── 4. 合并 → 校验整包 → 解压 ───────────────────────────────────────────────
echo "▸ 合并分块"
cat "$TMP/${TARBALL}.part-"* > "$TMP/${TARBALL}"

if [ -f "$TMP/SHA256SUMS" ]; then
  want="$(awk -v n="$TARBALL" '$2==n{print $1}' "$TMP/SHA256SUMS")"
  if [ -n "$want" ]; then
    got="$(sha256sum "$TMP/${TARBALL}" | cut -d' ' -f1)"
    [ "$want" = "$got" ] || { echo "✗ 整包校验失败（期望 $want，实得 $got）" >&2; exit 1; }
    echo "  · 整包校验通过"
  fi
fi

mkdir -p "$OUT"
echo "▸ 解压到 $OUT"
tar -xzf "$TMP/${TARBALL}" -C "$OUT"
find "$OUT" -name lingmiao -type f -exec chmod 755 {} + 2>/dev/null || true
BIN="$(find "$OUT" -name lingmiao -type f | head -1)"

echo
echo "✓ 就绪：$BIN"
echo
echo "下一步："
echo "  1) 配好你的模型 key（仓库自带模板）："
echo "       mkdir -p ~/.lingmiao && cp .env.example ~/.lingmiao/.env   # 填 DEEPSEEK_API_KEY"
echo "  2) 运行："
echo "       \"$BIN\""
echo
echo "  （可选）加自己的模型来源：cp config.example.json config.json 后编辑。"
