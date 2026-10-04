#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# install.sh — 从 GitHub Releases 取回 lingmiao 预编译二进制。
#
# 二进制（约 140 MB，含内嵌嵌入模型）随 GitHub Release 一起发布，本脚本负责
# 「下载 → 校验 → 解压 → 就位」，产出可直接运行的 ./dist/lingmiao-<版本>-<平台>/lingmiao。
#
# 用法（两种，任选）：
#   1) 先克隆仓库再跑：
#        git clone https://github.com/bmoonx/lingmiao-agent.git
#        cd lingmiao-agent && ./install.sh
#
#   2) 不克隆，直接跑：
#        curl -fsSL https://raw.githubusercontent.com/bmoonx/lingmiao-agent/main/install.sh | bash
#
# 选项：
#   --version VER     指定版本（默认 latest）
#   --out DIR         解压到 DIR（默认 ./dist）
#   --keep            保留下载的中间文件
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="bmoonx/lingmiao-agent"
REL_BASE="https://github.com/${REPO}/releases"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="$SCRIPT_DIR/dist"
VERSION="latest"
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift ;;
    --out)     OUT="$2"; shift ;;
    --keep)    KEEP=1 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
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

TMP="$(mktemp -d)"
trap '[ "$KEEP" = 1 ] || rm -rf "$TMP"' EXIT

# ── 1. 解析 tag（latest → 具体 tag）─────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  echo "▸ 解析最新 Release …"
  loc="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "${REL_BASE}/latest" || true)"
  VERSION="${loc##*/}"
  case "$VERSION" in v[0-9]*) : ;; *) echo "✗ 无法解析 latest 版本（得到 \"$VERSION\"）；用 --version 指定。" >&2; exit 1 ;; esac
fi
case "$VERSION" in v*) : ;; *) VERSION="v${VERSION}" ;; esac
VER="${VERSION#v}"
NAME="lingmiao-${VER}-${PLAT}"
TARBALL="${NAME}.tar.gz"
echo "▸ 目标：${NAME}"

DL_BASE="${REL_BASE}/download/${VERSION}"

# ── 2. 下载二进制 + 校验和 ──────────────────────────────────────────────────
echo "▸ 下载 ${TARBALL}"
curl -fsSL "${DL_BASE}/${TARBALL}" -o "$TMP/${TARBALL}"
if curl -fsSL "${DL_BASE}/${TARBALL}.sha256" -o "$TMP/${TARBALL}.sha256" 2>/dev/null; then
  echo "▸ 校验完整性"
  want="$(cut -d' ' -f1 < "$TMP/${TARBALL}.sha256")"
  got="$(sha256sum "$TMP/${TARBALL}" | cut -d' ' -f1)"
  [ "$want" = "$got" ] || { echo "✗ 校验失败（期望 $want，实得 $got）" >&2; exit 1; }
  echo "  · 校验通过"
fi

# ── 3. 解压 ─────────────────────────────────────────────────────────────────
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
