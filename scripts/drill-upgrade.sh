#!/usr/bin/env bash
# 升级演练（T04 方案 7.3；T03 Q4 交来）。在临时目录里做，不碰本机的 iWork 数据与自启注册项。
#
# 同主版本（带清单的包）：
#   1. 装 A（后台 + 客户端 + 清单）并起 A
#   2. 分批换成 B：先放半个后台、停 5s（期间断言 A 没动）、写完后台、换客户端、最后换清单 → A 自行重启成 B
#   3. 装回 A → 同样的路径回到 A
# 跨主版本：
#   4. 停后台；库的协议主版本改成新的 → 起后台：拒绝启动（12）、状态带两个主版本号与备份路径、备份在、库文件不变
#   5. 主版本改回去 → 正常启动、事件数不变
# 包的形态互换、整包签名的包资源最后到位：见真进程用例 `install::bundle::*`（macOS）。
#
# 后台带 IWORK_DEV_INSTALL_FAST=10：安装检查的周期与冷却都除以 10（3s）。退出码：0 通过；1 不符；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
for c in jq sqlite3; do command -v "$c" >/dev/null || { echo "drill-upgrade: 需要 $c" >&2; exit 2; }; done

say() { printf '== %s\n' "$*"; }
bad() {
  printf 'drill-upgrade: 不符：%s\n' "$*" >&2
  [ -f "${DATA:-/nonexistent}/logs/daemon.log" ] && { echo "-- 日志尾部" >&2; tail -8 "$DATA/logs/daemon.log" | cut -c1-300 >&2; }
  exit 1
}

cargo build -q -p iwork-daemon --features dev --bin iwork-daemon || exit 2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/iwork-daemon"
[ -x "$BIN" ] || { echo "drill-upgrade: 构建之后找不到 $BIN" >&2; exit 2; }

WORK="$(cd "$(mktemp -d /tmp/iwu-XXXXXX)" && pwd -P)"
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

variant() { # <源> <目标> <标记>
  cp "$1" "$2.tmp"
  if [ "$(uname -s)" = Darwin ]; then codesign -f -s - -i "dev.iwork.$(basename "$2").$3" "$2.tmp" 2>/dev/null
  else printf 'iwork-drill-%s' "$3" >> "$2.tmp"; fi
  mv "$2.tmp" "$2"
}
image_of() {
  if [ "$(uname -s)" = Darwin ]; then echo "cdhash:$(codesign -dvvv "$1" 2>&1 | sed -n 's/^CDHash=//p' | head -1)"
  else echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; fi
}
# 一版的完整包放在 $WORK/pkg-<标记>/（后台、客户端、清单），安装时逐个改名进 $APP
build_pkg() {
  local d="$WORK/pkg-$1"; mkdir -p "$d"
  variant "$BIN" "$d/iwork-daemon" "$1"
  variant "$BIN" "$d/iwork-client" "$1"
  scripts/write-install-manifest.sh "$d" >/dev/null
}
put() { cp "$WORK/pkg-$1/$2" "$APP/.$2.in"; mv "$APP/.$2.in" "$APP/$2"; }
phase() { jq -c '.phase' "$RUN/daemon.status.json" 2>/dev/null || echo null; }
readies() { grep -c '后台服务就绪' "$DATA/logs/daemon.log" 2>/dev/null || echo 0; }
last_ready_image() { grep '后台服务就绪' "$DATA/logs/daemon.log" | tail -1 | jq -r '.fields.image'; }
wait_ready() { # <pid> <就绪次数>
  for _ in $(seq 1 900); do
    if [ "$(jq -r '.pid' "$RUN/daemon.status.json" 2>/dev/null)" = "$1" ] && [ "$(phase)" = '"Ready"' ] && [ "$(readies)" -ge "$2" ]; then return 0; fi
    sleep 0.1
  done
  bad "pid $1 没在 90s 内第 $2 次就绪：$(phase)"
}
events() { sqlite3 "$DATA/iwork.db" 'SELECT COUNT(*) FROM events'; }

build_pkg A
build_pkg B
A_IMAGE="$(image_of "$WORK/pkg-A/iwork-daemon")"
B_IMAGE="$(image_of "$WORK/pkg-B/iwork-daemon")"

say "1. 装 A 并起 A"
for f in iwork-daemon iwork-client iwork-manifest.json; do put A "$f"; done
IWORK_DEV_INSTALL_FAST=10 "$APP/iwork-daemon" --dev-trust >/dev/null 2>"$WORK/daemon.err" &
PID=$!
wait_ready "$PID" 1
[ "$(last_ready_image)" = "$A_IMAGE" ] || bad "就绪的不是 A"
echo "A pid $PID 就绪：$(last_ready_image)"

say "2. 分批换成 B"
head -c "$(( $(wc -c < "$WORK/pkg-B/iwork-daemon") / 2 ))" "$WORK/pkg-B/iwork-daemon" > "$APP/.half"
mv "$APP/.half" "$APP/iwork-daemon"
echo "  放了半个后台，等进「安装中」后再停 5s"
for _ in $(seq 1 200); do grep -q '安装中' "$DATA/logs/daemon.log" && break; sleep 0.1; done
grep -q '安装中' "$DATA/logs/daemon.log" || bad "20s 内没进「安装中」"
sleep 5
[ "$(readies)" = 1 ] && kill -0 "$PID" || bad "半个后台期间 A 不该动"
echo "  A 没动（就绪 1 次，pid $PID 还在），日志里有「安装中」"
put B iwork-daemon; sleep 1; [ "$(readies)" = 1 ] || bad "清单还没换，不该动"
put B iwork-client; sleep 1; [ "$(readies)" = 1 ] || bad "清单还没换，不该动"
put B iwork-manifest.json
wait_ready "$PID" 2
[ "$(last_ready_image)" = "$B_IMAGE" ] || bad "重启成的不是 B：$(last_ready_image)"
echo "  重启成 B（同一个 pid ${PID}）：$(last_ready_image)"

say "3. 装回 A"
for f in iwork-daemon iwork-client iwork-manifest.json; do put A "$f"; done
wait_ready "$PID" 3
[ "$(last_ready_image)" = "$A_IMAGE" ] || bad "回到的不是 A"
echo "  回到 A：$(last_ready_image)"

say "4. 跨主版本：库的协议主版本改成新的"
kill -TERM "$PID"; wait "$PID" || true; PID=""
BEFORE_EVENTS="$(events)"
MAJOR="$(sqlite3 "$DATA/iwork.db" "SELECT value FROM meta WHERE key='protocol_major'")"
sqlite3 "$DATA/iwork.db" "UPDATE meta SET value='$((MAJOR + 1))' WHERE key='protocol_major'"
DB_HASH="$(shasum "$DATA/iwork.db" | cut -d' ' -f1)"
BACKUPS="$(ls "$DATA/backups" | wc -l | tr -d ' ')"
set +e; "$APP/iwork-daemon" --dev-trust >/dev/null 2>"$WORK/refuse.err"; RC=$?; set -e
echo "  退出码 ${RC}；状态 $(phase)"
[ "$RC" = 12 ] || bad "应以 12 拒绝启动"
jq -e --argjson db "$((MAJOR + 1))" --argjson p "$MAJOR" \
  '.phase.Refused.ProtocolMajorMismatch | .database == $db and .program == $p and (.backup | length > 0)' \
  "$RUN/daemon.status.json" >/dev/null || bad "状态里应有两个主版本号与备份路径"
[ "$(ls "$DATA/backups" | wc -l | tr -d ' ')" -gt "$BACKUPS" ] || bad "应多一份备份"
[ "$(shasum "$DATA/iwork.db" | cut -d' ' -f1)" = "$DB_HASH" ] || bad "原库不该变"

say "5. 主版本改回去，正常启动"
sqlite3 "$DATA/iwork.db" "UPDATE meta SET value='$MAJOR' WHERE key='protocol_major'"
"$APP/iwork-daemon" --dev-trust >/dev/null 2>"$WORK/again.err" &
PID=$!
wait_ready "$PID" 4
[ "$(events)" = "$BEFORE_EVENTS" ] || bad "事件数变了"
echo "  就绪，事件数 $(events) 不变"
kill -TERM "$PID"; wait "$PID" || true; PID=""

echo "drill-upgrade: 通过"
