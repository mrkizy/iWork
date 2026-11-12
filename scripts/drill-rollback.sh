#!/usr/bin/env bash
# 紧急路径演练的 T04 段（T04 方案 7.3；T07 方案 S6、第 15 节第 8 步接在后面加规则集版本的核对）。
#
# 在临时目录里做，不碰本机的 iWork 数据与自启注册项：
#   1. 起后台 A（开发构建的假工作：一个回合在跑）
#   2. ShutdownDaemon{cancel_running:false} → 被拒（PRECONDITION_FAILED），带回合数
#   3. ShutdownDaemon{cancel_running:true}  → 受理；后台走取消流程、以 0 退出
#   4. 换成另一个构建 B（macOS 换签名标识重签，Linux 末尾追加字节：映像身份必变）
#   5. 起 B → 状态 Ready，日志里就绪时的运行映像是 B
# 每一步打印命令与观察；任何一步不符就以 1 退出。退出码：0 通过；1 不符；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
command -v jq >/dev/null || { echo "drill-rollback: 需要 jq" >&2; exit 2; }

say() { printf '== %s\n' "$*"; }
bad() { printf 'drill-rollback: 不符：%s\n' "$*" >&2; exit 1; }

# 后台程序与调试工具都由本脚本自己构建（只 build example 不会产出后台程序，干净检出上会缺，1/F-6）
cargo build -q -p iwork-daemon --features dev --bin iwork-daemon --example lifecycle-tool || exit 2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/iwork-daemon"
TOOL="${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples/lifecycle-tool"
for f in "$BIN" "$TOOL"; do [ -x "$f" ] || { echo "drill-rollback: 构建之后找不到 $f" >&2; exit 2; }; done

WORK="$(cd "$(mktemp -d /tmp/iwd-XXXXXX)" && pwd -P)"
PID=""
cleanup() {
  [ -n "$PID" ] && kill -9 "$PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 143' TERM INT

export HOME="$WORK/home"
unset XDG_RUNTIME_DIR XDG_DATA_HOME XDG_CONFIG_HOME IWORK_DATA_DIR
mkdir -p "$HOME"
if [ "$(uname -s)" = Darwin ]; then DATA="$HOME/Library/Application Support/iWork"; else DATA="$HOME/.local/share/iwork"; fi
RUN="$DATA/run"
APP="$WORK/app"; mkdir -p "$APP"

variant() { # <源> <目标> <标记>：造一个映像身份不同的拷贝
  cp "$1" "$2.tmp"
  if [ "$(uname -s)" = Darwin ]; then codesign -f -s - -i "dev.iwork.daemon.$3" "$2.tmp" 2>/dev/null
  else printf 'iwork-drill-%s' "$3" >> "$2.tmp"; fi
  mv "$2.tmp" "$2"
}
image_of() {
  if [ "$(uname -s)" = Darwin ]; then echo "cdhash:$(codesign -dvvv "$1" 2>&1 | sed -n 's/^CDHash=//p' | head -1)"
  else echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; fi
}
phase() { jq -c '.phase' "$RUN/daemon.status.json" 2>/dev/null || echo null; }
wait_ready() { # <pid>
  for _ in $(seq 1 600); do
    if [ "$(jq -r '.pid' "$RUN/daemon.status.json" 2>/dev/null)" = "$1" ] && [ "$(phase)" = '"Ready"' ]; then return 0; fi
    sleep 0.1
  done
  bad "pid $1 没在 60s 内就绪：$(phase)"
}
last_ready_image() { grep '后台服务就绪' "$DATA/logs/daemon.log" | tail -1 | jq -r '.fields.image'; }

say "1. 装 A 并起 A（假工作：一个回合在跑）"
variant "$BIN" "$APP/iwork-daemon" A
IWORK_DEV_WORK="turns=1" "$APP/iwork-daemon" --dev-trust >/dev/null 2>"$WORK/a.err" &
PID=$!
wait_ready "$PID"
echo "A pid $PID 就绪，运行映像 $(last_ready_image)"

say "2. ShutdownDaemon{cancel_running:false}"
set +e; OUT="$("$TOOL" --run-dir "$RUN" shutdown)"; RC=$?; set -e
echo "${OUT}（退出码 ${RC}）"
[ "$RC" = 1 ] || bad "应被拒"
[ "$(echo "$OUT" | jq -r '.details.running_turns')" = 1 ] || bad "应带回合数 1"
kill -0 "$PID" || bad "被拒后后台应还在"

say "3. ShutdownDaemon{cancel_running:true}"
"$TOOL" --run-dir "$RUN" shutdown --cancel-running
set +e; wait "$PID"; RC=$?; set -e; PID=""
echo "A 退出码 ${RC}；状态 $(phase)"
[ "$RC" = 0 ] || bad "用户退出应以 0 退出"
grep -q 'dev work：cancel_all' "$DATA/logs/daemon.log" || bad "应走取消流程"
[ "$(phase)" = '{"Exited":{"code":"Stopped","reason":"UserQuit","message":null}}' ] || bad "状态应是 Exited{UserQuit}"

say "4. 换成另一个构建 B"
variant "$BIN" "$APP/iwork-daemon" B
B_IMAGE="$(image_of "$APP/iwork-daemon")"
echo "B 的映像 $B_IMAGE"

say "5. 起 B"
"$APP/iwork-daemon" --dev-trust >/dev/null 2>"$WORK/b.err" &
PID=$!
wait_ready "$PID"
[ "$(last_ready_image)" = "$B_IMAGE" ] || bad "就绪的不是 B：$(last_ready_image)"
echo "B pid $PID 就绪，运行映像 $(last_ready_image)"
kill -TERM "$PID"; wait "$PID" || true; PID=""

echo "drill-rollback: 通过"
