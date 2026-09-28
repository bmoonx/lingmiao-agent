#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# lingmiao 启动脚本（测试用）—— 运行「灵妙 (lingmiao)」TUI
#
# 用法:
#   ./run.sh                 # debug 构建并运行 TUI（二进制缺失时自动 build）
#   ./run.sh --release       # 用 release 构建（更快、二进制更小）
#   ./run.sh --build         # 先强制重新构建再运行
#   ./run.sh --check         # 只做环境自检（校验 key / 配置 / 二进制），不启动 TUI
#
# 可调环境变量（均为可选）:
#   LINGMIAO_ENV=<file>        指定 API key 文件（默认: ~/.lingmiao/.env → <root>/.env）
#   LINGMIAO_CONFIG_DIR=<dir>  使用外部配置目录（须含完整管线；不兼容时程序自动回退内嵌默认）
#   LINGMIAO_CWD=<dir>         运行目录，.memory/ 记忆目录落点（默认: 仓库根）
#   LINGMIAO_BIN=<path>        直接指定要运行的 lingmiao 二进制（跳过构建）
#
# 说明:
#   - API key 从 .env 读取并导出为进程环境变量（已存在的同名变量优先，不覆盖）。
#   - 程序自身 .env 解析优先级: ~/.lingmiao/.env → $LINGMIAO_HOME/.env → exe 目录/.env
#     → cwd 上溯 ≤5 级 → ~/.claude/settings.json。本脚本负责把 key 可靠地注入。
#   - 本机若有 LINGMIAO_CONFIG_DIR 指向旧的工程配置，本脚本默认清掉，
#     让程序用内嵌默认（3 stages / 20 prompts）。要用外部配置请显式设 LINGMIAO_CONFIG_DIR。
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PROFILE="debug"
FORCE_BUILD=0
CHECK_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE="release" ;;
    --build)   FORCE_BUILD=1 ;;
    --check)   CHECK_ONLY=1 ;;
    -h|--help) sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run.sh: 未知参数 '$arg'（-h 查看用法）" >&2; exit 2 ;;
  esac
done

# ── 1. 加载 API key ───────────────────────────────────────────────────────────
# 解析 `KEY=VALUE` 行（跳过注释/空行）；已存在的环境变量优先，不覆盖。
load_env_file() {
  local f="$1" line k v
  [[ -f "$f" ]] || return 1
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ -z "${line//[[:space:]]/}" ]] && continue   # 空行
    [[ "$line" == \#* ]] && continue               # 注释
    [[ "$line" != *=* ]] && continue               # 非 KV 行
    k="${line%%=*}"; v="${line#*=}"
    k="$(printf '%s' "$k" | xargs)"                # trim
    v="$(printf '%s' "$v" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"   # 去成对引号
    [[ -z "$k" ]] && continue
    if [[ -z "${!k:-}" ]]; then export "$k=$v"; fi
  done < "$f"
}

ENV_FILE="${LINGMIAO_ENV:-}"
if [[ -z "$ENV_FILE" ]]; then
  for c in "$HOME/.lingmiao/.env" "$ROOT/.env"; do
    [[ -f "$c" ]] && ENV_FILE="$c" && break
  done
fi

KEY_SOURCE="(未找到 .env — 依赖外部已导出的环境变量)"
if [[ -n "$ENV_FILE" && -f "$ENV_FILE" ]]; then
  load_env_file "$ENV_FILE"
  KEY_SOURCE="$ENV_FILE"
fi

# ── 2. 配置目录：默认走内嵌，LINGMIAO_CONFIG_DIR 显式指定外部 ──────────────────
if [[ -n "${LINGMIAO_CONFIG_DIR:-}" ]]; then
  export LINGMIAO_CONFIG_DIR
else
  unset LINGMIAO_CONFIG_DIR 2>/dev/null || true
fi

# ── 3. 运行目录（.memory/ 记忆目录落点）──────────────────────────────────────
RUN_DIR="${LINGMIAO_CWD:-$ROOT}"
cd "$RUN_DIR"

# ── 4. 定位 / 构建二进制 ─────────────────────────────────────────────────────
BIN="${LINGMIAO_BIN:-$ROOT/target/$PROFILE/lingmiao}"
if (( FORCE_BUILD )) || [[ ! -x "$BIN" ]]; then
  echo "run.sh: 构建中（$PROFILE）..." >&2
  ( cd "$ROOT" && cargo build $([[ "$PROFILE" == release ]] && echo "--release") )
fi

# ── 5. 自检 ───────────────────────────────────────────────────────────────────
PROVIDER="${LLM_PROVIDER:-deepseek}"
case "$PROVIDER" in
  kimi) KEY_VAR="KIMI_API_KEY"; MODEL_VAR="KIMI_MODEL" ;;
  claude|anthropic) KEY_VAR="CLAUDE_API_KEY"; MODEL_VAR="CLAUDE_MODEL" ;;
  *) KEY_VAR="DEEPSEEK_API_KEY"; MODEL_VAR="DEEPSEEK_MODEL" ;;
esac
KEY_VALUE="${!KEY_VAR:-}"
if [[ -n "$KEY_VALUE" ]]; then
  KEY_STATE="已设置 (len=${#KEY_VALUE})"
else
  KEY_STATE="未设置 ⚠"
fi

echo "──────────────────────────────────────────────"
echo " lingmiao 启动自检（灵妙 / lingmiao）"
echo "──────────────────────────────────────────────"
echo " 仓库根    : $ROOT"
echo " 运行目录  : $RUN_DIR"
echo " 二进制    : $BIN  $( [[ -x "$BIN" ]] && echo '(存在)' || echo '(缺失)' )"
echo " key 来源  : $KEY_SOURCE"
echo " provider  : $PROVIDER  (key: $KEY_VAR = $KEY_STATE)"
echo " model     : ${!MODEL_VAR:-<默认>}"
echo " 配置目录  : ${LINGMIAO_CONFIG_DIR:-<内嵌默认>}"
echo " DISPLAY   : ${DISPLAY:-<未设置>}"
echo "──────────────────────────────────────────────"

if [[ -z "${!KEY_VAR:-}" ]]; then
  echo "run.sh: 缺少 $KEY_VAR，程序无法启动。" >&2
  echo "        请设置 LINGMIAO_ENV=/path/to/.env 或 export $KEY_VAR=..." >&2
  exit 1
fi

if (( CHECK_ONLY )); then
  # 非 TTY 下程序会打印配置计数后退出，用它验证环境是否被正确加载。
  echo "run.sh: 环境自检（不启动 TUI）..."
  "$BIN" </dev/null || true
  exit 0
fi

if [[ ! -t 0 || ! -t 1 ]]; then
  echo "run.sh: 警告 — 当前不是交互式终端，TUI 需要真实 tty。" >&2
  echo "        如需在 Xvfb 上跑，请用: DISPLAY=:99 xterm -e '$0'" >&2
fi

echo "run.sh: 启动 TUI（连按两次 Ctrl+C 退出；菜单栏窗口关闭按钮亦可）..."
exec "$BIN"
