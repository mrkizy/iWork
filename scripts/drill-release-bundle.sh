#!/usr/bin/env bash
# 真包演练（T04p 方案第 9 节）：拿已经打好的发布包做装、起、换、查、收尾。只在 macOS、只在演练专用的账号里跑（PD-3）。
#
# 用法：scripts/drill-release-bundle.sh --packages <目录> [--out <输出目录>]
#   <目录> 里放：a-unsigned/iWork.app、a-signed/iWork.app、b-unsigned/iWork.app、b-signed/iWork.app（同一份源码、只差
#   版本号的两版，各两种形态；9.4）；check-install（核对程序）；ipcprobe（N5 的握手探针 tools/t0/iwork-t04-ipcprobe.rs，
#   不开 dev、不签名，自检第 2 件用它被拒一次）。
#   <输出目录> 不给就是 <目录>/out-<时间>。记录写在 <输出目录>/record.txt，终端输出另存 terminal.txt。
# 退出码：0 演练跑完（各格的 PASS/FAIL/INCONCLUSIVE 在记录里）；1 演练失败（取证不成立、环境没管住）；
#         3 前置检查没过（什么都没动）；4 日志配置没放行要用的记录（9.2 自检）；2 用法错。
#
# 四条硬规定（9.1），脚本照做：
#   1. 全程不开登录自启。脚本不发这个命令。
#   2. 开始前、结束后各核一次自启没注册（只看 launchctl print 的退出码，输出丢掉）；开始前就注册着 → 停，不代为注销。
#   3. 装包只用留底后的整包替换（install_package）。
#   4. 取证不读、不存任何进程或 launchd 的整份环境：只用脚本自己给出去的参数、lsof 看到的路径、自建的哨兵。
#      脚本里没有 ps 带 e/-E、不带变量名的 env、printenv、export -p、launchctl export、set -x；launchctl getenv 只问
#      IWORK_DATA_DIR，记录里只写三态。
# 数据根 D 只算一次（9.2）：起进程时显式钉住 IWORK_DATA_DIR=D，起来后用 lsof 核实后台打开的就在 D 下。
set -euo pipefail

LOG_SPEC='info,iwork_ipc=debug'   # 全演练唯一的日志配置（R13）：开头的 info 不能省，见 9.2「日志配置与自检」
LABEL=com.iwork.daemon
SETTLE="${IWORK_DRILL_SETTLE_SECS:-15}"   # 查进程两次之间（launchd 管着的后台退出约 10 秒后会被重拉）
WAIT="${IWORK_DRILL_WAIT_SECS:-75}"       # 等一条记录、一个进程的上限
STOP_WAIT="${IWORK_DRILL_STOP_SECS:-60}"  # 收尾时等后台走完停机的上限
COOLDOWN="${IWORK_DRILL_COOLDOWN_SECS:-31}"  # 回退窗口：B 版后台就绪后等过它自己的启动冷却（30 秒）再回退

die_usage() { echo "用法：$0 --packages <目录> [--out <输出目录>]" >&2; exit 2; }
PKG="" OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --packages) PKG="${2:-}"; shift 2 || die_usage ;;
    --out) OUT="${2:-}"; shift 2 || die_usage ;;
    *) die_usage ;;
  esac
done
[ -n "$PKG" ] && [ -d "$PKG" ] || die_usage
[ "$(uname -s)" = Darwin ] || { echo "drill-release-bundle：只在 macOS 上跑" >&2; exit 2; }
for c in jq perl; do command -v "$c" >/dev/null || { echo "drill-release-bundle：需要 $c" >&2; exit 2; }; done
PKG="$(cd "$PKG" && pwd -P)"
# 路径归属：删、写、改名都经过这一处（只碰这一次建的、不走链接、不跨文件系统改名、按目录边界判归属）
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=lib/owned-paths.sh
source "$ROOT/scripts/lib/owned-paths.sh"
# shellcheck source=lib/drill-window.sh
source "$ROOT/scripts/lib/drill-window.sh"
# 路径归属不成立：前置检查阶段（什么都没动）退出 3，演练开始后算演练失败、退出 1（都走记录，有记录的话）
OWNED_FAIL_RC=3
owned_paths_fail() {
  if [ -n "${RECORD:-}" ] && [ -f "$RECORD" ]; then note "路径归属不成立：$*"; else echo "drill-release-bundle：$*" >&2; fi
  exit "$OWNED_FAIL_RC"
}
# 输出目录只独占新建：已有的（哪怕只是一个链接）一律不用，免得截断、覆盖别处的文件
OUT="${OUT:-$PKG/out-$(date +%Y%m%dT%H%M%S)}"
owned_mkdir "$OUT"
OUT="$(canonical "$OUT")"
RECORD="$OUT/record.txt"
: > "$RECORD"
exec > >(tee -a "$OUT/terminal.txt") 2>&1

UIDN="$(id -u)"
D="$HOME/Library/Application Support/iWork"
APPS="$HOME/Applications"
DEST="$APPS/iWork.app"
SOCK="$D/run/d.sock"
LOG_FILE="$D/logs/daemon.log"
EVENTS="$OUT/events.txt"

note() { printf '%s\n' "$*" >> "$RECORD"; printf '== %s\n' "$*"; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }

# ---------------------------------------------------------------- 哨兵与虚构凭据标记（9.2 的两个反例）
SENT="$OUT/sentinel"
owned_mkdir "$SENT"
echo marker > "$SENT/marker"
tree_digest() {
  (cd "$1" && find . | LC_ALL=C sort | while IFS= read -r f; do
    if [ -f "$f" ]; then printf '%s %s\n' "$f" "$(shasum -a 256 < "$f" | cut -d' ' -f1)"; else printf '%s\n' "$f"; fi
  done)
}
SENT_BEFORE="$(tree_digest "$SENT")"
# 故意扮成「终端里留着一个别的数据目录」：机制没管住的话，被写的是这个哨兵，不是谁的真数据
export IWORK_DATA_DIR="$SENT"
FAKE_CRED="drillmark$(head -c 20 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export IWORK_DRILL_FAKE_CREDENTIAL="$FAKE_CRED"

# ---------------------------------------------------------------- 只读的查询
autostart_state() {
  if launchctl print "gui/$UIDN/$LABEL" >/dev/null 2>&1 || [ -e "$HOME/Library/LaunchAgents/$LABEL.plist" ]; then
    echo 注册着
  else
    echo 没注册
  fi
}
# 只认本用户的进程：macOS 的 pgrep 不带 -u 会匹配所有用户的同名进程（平时账号里的开发后台、测试进程），
# 前置检查会误停，演练中途会选中、去杀别人的进程。找进程只有这一处
pids_of() { pgrep -x -u "$UIDN" "$1" 2>/dev/null || true; }
any_iwork_running() { [ -n "$(pids_of iwork-daemon)$(pids_of iwork-client)" ]; }
# launchd 用户环境里的 IWORK_DATA_DIR：只问这一个名字，只回三态，不回值
launchd_data_dir_state() {
  local v
  v="$(launchctl getenv IWORK_DATA_DIR 2>/dev/null || true)"
  if [ -z "$v" ]; then echo 空; elif [ "$v" = "$D" ]; then echo 等于D; else echo 别的值; fi
}

# ---------------------------------------------------------------- 前置检查（9.2；不满足就停，什么都不动）
stop_before() { note "前置检查没过：$*。什么都没动"; exit 3; }
note "演练开始；数据根 D=${D}；装到 ${DEST}；日志配置 IWORK_LOG=$LOG_SPEC"
START_AUTOSTART="$(autostart_state)"
note "开始前的自启：$START_AUTOSTART"
[ "$START_AUTOSTART" = 没注册 ] || stop_before "开始前 $LABEL 就注册着（不是演练留下的，请人处理）"
any_iwork_running && stop_before "有 iwork-daemon 或 iwork-client 在跑"
sleep "$SETTLE"
any_iwork_running && stop_before "隔 ${SETTLE} 秒再查，有 iwork-daemon 或 iwork-client 在跑（可能被 launchd 重拉）"
DATA_DIR_STATE="$(launchd_data_dir_state)"
note "launchd 用户环境里的 IWORK_DATA_DIR：$DATA_DIR_STATE"
[ "$DATA_DIR_STATE" = 别的值 ] && stop_before "launchd 用户环境里的 IWORK_DATA_DIR 指向别处"
# 演练要放东西、事后要删或改名的每个位置，开始时都必须空着（或是空的真目录）：不是这一次建的，脚本一概不碰
if _exists "$D"; then
  real_dir "$D" || stop_before "数据根 D 不是真目录（是链接或文件）"
  is_empty_dir "$D" || stop_before "数据根 D 不是空的（或读不了内容）"
fi
if _exists "$APPS"; then real_dir "$APPS" || stop_before "${APPS} 不是真目录（是链接或文件）"; fi
for p in "$DEST" "${DEST%.app}.prev.app" "${DEST%.app}.incoming.app" "${DEST%.app}.failed.app"; do
  _exists "$p" && stop_before "${p} 已经有东西（不是这一次建的）"
done
for p in a-unsigned a-signed b-unsigned b-signed; do
  "$PKG/check-install" "$PKG/$p/iWork.app/Contents/MacOS" >> "$OUT/check-install-$p.txt" 2>&1 \
    || stop_before "包 $p 过不了 check-install（见 check-install-$p.txt）"
done
note "前置检查都过了"
OWNED_FAIL_RC=1

# ---------------------------------------------------------------- 收尾（成功、失败、被中断都执行）
FAILURES=()
fail_drill() { note "演练失败：$*"; exit 1; }
cleanup() {
  local rc=$? p i
  set +e
  for p in $(pids_of iwork-client); do kill -TERM "$p"; done
  for i in $(seq 1 50); do [ -z "$(pids_of iwork-client)" ] && break; sleep 0.2; done
  for p in $(pids_of iwork-daemon); do kill -TERM "$p"; done
  for i in $(seq 1 $((STOP_WAIT * 5))); do [ -z "$(pids_of iwork-daemon)" ] && break; sleep 0.2; done
  for p in $(pids_of iwork-daemon) $(pids_of iwork-client); do
    kill -KILL "$p"; note "收尾：进程 $p 到点还在，SIGKILL"
  done
  sleep "$SETTLE"
  [ -n "$(pids_of iwork-daemon)$(pids_of iwork-client)" ] && note "收尾：隔 ${SETTLE} 秒还有 iWork 进程"
  local end_state
  end_state="$(autostart_state)"
  note "结束时的自启：$end_state"
  if [ "$end_state" = 注册着 ]; then
    launchctl bootout "gui/$UIDN/$LABEL" >/dev/null 2>&1
    rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
    note "收尾：注销了演练中出现的自启注册（之后：$(autostart_state)）"
    rc=1
  fi
  # 只删这一次登记过的（前置检查保证开始时这些位置都空着）
  for p in "$DEST" "${DEST%.app}.prev.app" "${DEST%.app}.incoming.app" "${DEST%.app}.failed.app"; do
    _exists "$p" && { is_owned "$p" && owned_rm "$p" || note "收尾：${p} 不是这一次建的，没删"; }
  done
  # 数据根：开始时不存在或是空的真目录，之后的内容都是演练的后台写的 → 归演练，改名留证
  if _exists "$D"; then
    own "$D"
    local kept="$D.drill-$(date +%Y%m%dT%H%M%S)"
    ( owned_mv "$D" "$kept" ) && note "收尾：数据根留作证据：${kept}" || note "收尾：数据根没能改名留证"
  fi
  if [ "$(tree_digest "$SENT")" != "$SENT_BEFORE" ]; then
    note "哨兵目录被改过（数据根没管住）"; rc=1
  fi
  # 和 verify_root 一样按目录边界判（不找子串）
  local f path
  for f in "$OUT"/lsof-*.txt; do
    [ -f "$f" ] || continue
    while IFS= read -r path; do
      [ -n "$path" ] && under_dir "$path" "$SENT" && { note "lsof 看到了哨兵目录下的路径：$path"; rc=1; }
    done < "$f"
  done
  if grep -rqF "$FAKE_CRED" "$OUT"; then
    note "记录里搜到了虚构凭据标记（IWORK_DRILL_FAKE_CREDENTIAL），这份记录要按 S9 处理"; rc=1
  fi
  note "结束：退出码 ${rc}；动过的：${DEST}（已删）、数据根（已改名留证）；自启：$end_state"
  exit "$rc"
}
trap cleanup EXIT

# ---------------------------------------------------------------- 动作
# 9.3 唯一的装法：新包先完整复制到旁边，旧包整个改名留底，新包整个改名到位
install_package() {
  local new="$PKG/$1/iWork.app" incoming="${DEST%.app}.incoming.app" prev="${DEST%.app}.prev.app"
  _exists "$APPS" || owned_mkdir "$APPS"
  _exists "$incoming" && owned_rm "$incoming"
  dest_free "$incoming"
  ditto "$new" "$incoming"
  own "$incoming"
  if _exists "$DEST"; then
    _exists "$prev" && owned_rm "$prev"
    owned_mv "$DEST" "$prev"
  fi
  owned_mv "$incoming" "$DEST"
  note "装上 ${1}（整包替换）"
}
# U2、U4 故意用的错误装法：把包合并盖到装上的那份上。只盖这一次装的（登记过、是真目录、里面没有链接）
overlay_package() {
  is_owned "$DEST" && real_dir "$DEST" || fail_drill "${DEST} 不是这一次装的，不往上盖"
  no_links_in "$DEST"
  ditto "$PKG/$1/iWork.app" "$DEST"
}
roll_back() {
  local failed="${DEST%.app}.failed.app"
  _exists "$failed" && owned_rm "$failed"
  owned_mv "$DEST" "$failed"
  owned_mv "${DEST%.app}.prev.app" "$DEST"
  note "回退：留底的包换回来了"
}

wait_pid() {
  local name=$1 i p
  for i in $(seq 1 $((WAIT * 5))); do
    p="$(pids_of "$name" | head -1)"
    [ -n "$p" ] && { echo "$p"; return 0; }
    sleep 0.2
  done
  return 1
}

# 起客户端：经系统的启动服务（和双击同一条路），显式钉住数据根与日志配置；虚构凭据标记只记名字
start_client() {
  note "起客户端：open -n --env IWORK_DATA_DIR=$D --env IWORK_LOG=$LOG_SPEC --env IWORK_DRILL_FAKE_CREDENTIAL=<标记> $DEST"
  open -n --env "IWORK_DATA_DIR=$D" --env "IWORK_LOG=$LOG_SPEC" --env "IWORK_DRILL_FAKE_CREDENTIAL=$FAKE_CRED" "$DEST"
  CLIENT_PID="$(wait_pid iwork-client)" || fail_drill "${WAIT} 秒内客户端没起来"
  note "客户端进程 $CLIENT_PID"
}
quit_client() {
  local p i
  for p in $(pids_of iwork-client); do kill -TERM "$p"; done
  for i in $(seq 1 50); do [ -z "$(pids_of iwork-client)" ] && return 0; sleep 0.2; done
  fail_drill "客户端 10 秒内没退出"
}

# 核实后台实际用的是 D：它打开的库、锁、日志都要在 D 下，哨兵下一个都不能有
verify_root() {
  local pid=$1 label=$2 f="$OUT/lsof-$2.txt"
  lsof -p "$pid" -Fn 2>/dev/null | sed -n 's/^n//p' > "$f" || true
  local relevant outside
  relevant="$(grep -E 'iwork\.db|daemon\.lock|/logs/' "$f" || true)"
  [ -n "$relevant" ] || fail_drill "lsof 没看到后台 $pid 打开库、锁或日志（${label}）"
  # 按规范化后的目录边界判（不找子串：/unexpected<D>/iwork.db 不在 D 下；经过链接的路径按真实路径判）
  outside=""
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    under_dir "$path" "$D" || outside="${outside}${path} "
    under_dir "$path" "$SENT" && fail_drill "后台 $pid 打开了哨兵目录下的文件（${label}）：$path"
  done <<< "$relevant"
  [ -z "$outside" ] || fail_drill "后台 $pid 打开了数据根之外的文件（${label}）：$outside"
  note "数据根核实（${label}）：后台 $pid 打开的库、锁、日志都在 D 下"
}

# 只看演练自己起的进程的参数，不带 e
check_daemon_args() {
  local pid=$1
  [ "$(ps -o comm= -p "$pid")" = "$DEST/Contents/MacOS/iwork-daemon" ] \
    || fail_drill "后台 $pid 不是装上的包里的程序：$(ps -o comm= -p "$pid")"
  ps -o args= -p "$pid" | grep -q -- --dev-trust && fail_drill "后台 $pid 带着 --dev-trust"
  note "后台 $pid 是包里的程序，参数里没有 --dev-trust"
}

# ---------------------------------------------------------------- 日志里的固定事件（只摘这些行，不整份复制）
# 一条记录 = 后台日志里的一行 JSON；按 jq 条件挑，并且 ts 不早于给的毫秒数
events_since() {
  local since=$1 filter=$2
  [ -f "$LOG_FILE" ] || return 0
  jq -c -R --argjson since "$since" "fromjson? | select(.ts >= \$since) | select($filter)" "$LOG_FILE" 2>/dev/null || true
}
wait_event() {
  local what=$1 since=$2 filter=$3 i line
  for i in $(seq 1 $((WAIT * 5))); do
    line="$(events_since "$since" "$filter" | head -1)"
    if [ -n "$line" ]; then printf '%s %s\n' "$what" "$line" >> "$EVENTS"; echo "$line"; return 0; fi
    sleep 0.2
  done
  return 1
}
F_CONNECTED='.target == "iwork_ipc" and .fields.message == "连接已建立" and .fields.pid == $pid'
F_REJECTED='.target == "iwork.ipc.audit" and .fields.event == "peer_rejected" and .fields.role == "server"'
F_RESTART='.fields.message == "磁盘上装齐了另一版，自行重启"'
F_REJECT_CHECK='.fields.message == "有客户端被拒，立刻做一次安装检查"'
F_READY='.fields.message == "后台服务就绪"'
F_INSTALLING='.fields.message == "安装中：磁盘上的后台不是我，也还没有装齐的包"'
F_DROPPED='(.fields.message // "") | test("丢了")'
connected_filter() { echo "${F_CONNECTED//\$pid/$1}"; }
# wait_event 交回的那一行日志的时刻
ts_of() { jq -r '.ts' <<<"$1"; }
# 等 <起点> 之后的「自行重启」，再等这一条之后的「就绪」。「就绪」按这次重启的时刻找，不按起点找：前一次重启的
# 「就绪」可能晚于起点到（上一格只等到重启就往下走了），按起点找会把它当成这一次的，进程还没换成新版就去比 CDHash
wait_restart_ready() { # <名字> <起点>
  local restart
  restart="$(wait_event "$1 自行重启" "$2" "$F_RESTART")" || return 1
  wait_event "$1 就绪" "$(ts_of "$restart")" "$F_READY" >/dev/null
}
stop_log() { note "日志配置没放行第 $1 类（$2）：后面的格一格都不做，不记成 FAIL"; exit 4; }

cdhash_of_pid() { codesign -dvvv "$1" 2>&1 | sed -n 's/^CDHash=//p' | head -1; }
cdhash_of_file() { codesign -dvvv "$1" 2>&1 | sed -n 's/^CDHash=//p' | head -1; }
confirm() {
  local answer
  printf '?? %s（看清后输入 y 回车；不对输入 n）：' "$1"
  read -r answer || answer=n
  note "人工确认：$1 → $answer"
  [ "$answer" = y ]
}

# ---------------------------------------------------------------- 自检 1 与验收 2（形态 A）
install_package a-unsigned
T0="$(now_ms)"
start_client
DAEMON_PID="$(wait_pid iwork-daemon)" || fail_drill "${WAIT} 秒内后台没被拉起"
note "后台进程 $DAEMON_PID"
wait_event "①连接已建立" "$T0" "$(connected_filter "$CLIENT_PID")" >/dev/null || stop_log 1 "连接已建立"
verify_root "$DAEMON_PID" a-unsigned
check_daemon_args "$DAEMON_PID"
wait_event "manifest_loaded" "$T0" '.target == "iwork.ipc.audit" and .fields.event == "manifest_loaded" and .fields.self_listed == true' >/dev/null \
  || FAILURES+=("验收 2：没有 manifest_loaded self_listed=true")
[ -z "$(events_since "$T0" "$F_REJECTED")" ] || FAILURES+=("验收 2：有 peer_rejected")
[ -z "$(events_since "$T0" '.fields.event == "peer_accepted_without_signature_check"')" ] || FAILURES+=("验收 2：有开发信任的记录")
confirm "客户端窗口进到了首页，不是诊断页" || FAILURES+=("验收 2：窗口不是首页")

# 自检 2：没登记在清单上的探针连一次，期望被拒（③）
T1="$(now_ms)"
"$PKG/ipcprobe" hello "$SOCK" >> "$OUT/ipcprobe.txt" 2>&1 || true
wait_event "peer_rejected（探针）" "$T1" "$F_REJECTED" >/dev/null || stop_log 3 "peer_rejected"
wait_event "③有客户端被拒" "$T1" "$F_REJECT_CHECK" >/dev/null || stop_log 3 "有客户端被拒，立刻做一次安装检查"

# 自检 3 = 8.2 的 U1：关客户端，整包替换成 B 版同形态，后台自己重启（②）
quit_client
INODE_BEFORE="$(stat -f %i "$DEST/Contents/MacOS/iwork-daemon")"
OLD_HASH="$(cdhash_of_pid "$DAEMON_PID")"
T2="$(now_ms)"
install_package b-unsigned
R2="$(wait_event "②自行重启（U1）" "$T2" "$F_RESTART")" || stop_log 2 "磁盘上装齐了另一版，自行重启"
wait_event "U1 重启后就绪" "$(ts_of "$R2")" "$F_READY" >/dev/null || FAILURES+=("U1：重启后没就绪")
NEW_HASH="$(cdhash_of_pid "$DAEMON_PID")"
if kill -0 "$DAEMON_PID" 2>/dev/null && [ "$NEW_HASH" = "$(cdhash_of_file "$DEST/Contents/MacOS/iwork-daemon")" ] && [ "$NEW_HASH" != "$OLD_HASH" ]; then
  note "U1 PASS：同一个 pid $DAEMON_PID 重启成 B 版（inode 前 ${INODE_BEFORE}、后 $(stat -f %i "$DEST/Contents/MacOS/iwork-daemon")）"
else
  note "U1 FAIL：pid $DAEMON_PID 没有自己重启成 B 版；按方案 8.2，验收 3 不做，回 architect"
  exit 1
fi
note "日志自检三类都看到了"

# ---------------------------------------------------------------- 8.2 的 U2–U4（只记录 / 救法）
T3="$(now_ms)"
I_BEFORE="$(stat -f %i "$DEST/Contents/MacOS/iwork-daemon")"
overlay_package a-unsigned
note "U2：ditto 直接盖到正在运行的包上（同形态 A 版）；inode 前 ${I_BEFORE}、后 $(stat -f %i "$DEST/Contents/MacOS/iwork-daemon")"
if wait_event "U2 自行重启" "$T3" "$F_RESTART" >/dev/null; then
  note "U2：后台 $(pids_of iwork-daemon) 自己重启了（原 pid $DAEMON_PID 还在：$(kill -0 "$DAEMON_PID" 2>/dev/null && echo 是 || echo 否)）"
else
  note "U2：${WAIT} 秒内没看到自行重启；后台进程：$(pids_of iwork-daemon | tr '\n' ' ')"
fi
DAEMON_PID="$(pids_of iwork-daemon | head -1)"
if confirm "U3：请在 Finder 里把 $PKG/b-unsigned/iWork.app 拖进 ${APPS}、选「替换」；做完输入 y，不做输入 n"; then
  T4="$(now_ms)"
  wait_event "U3 自行重启" "$T4" "$F_RESTART" >/dev/null && note "U3：后台自己重启了" || note "U3：${WAIT} 秒内没看到自行重启"
else
  note "U3 INCONCLUSIVE：没有人手做"
fi
# U4：合并覆盖着换形态（旧清单会留下）→ 预期两种形态都判不齐；再整包替换救回来
overlay_package b-signed
if "$PKG/check-install" "$DEST/Contents/MacOS" >> "$OUT/check-install-u4.txt" 2>&1; then
  note "U4：合并覆盖之后 check-install 竟然过了（和 N5 E2 不符），记下"
else
  note "U4：合并覆盖之后判不齐（预期）"
fi
T5="$(now_ms)"
install_package b-signed
# 等到这次重启的「就绪」再往下走：不然它的「就绪」会落进 9.7 第一轮
if wait_restart_ready "U4 救回后" "$T5"; then note "U4 PASS：整包替换后重启成 B 版形态 B"; else note "U4 FAIL：整包替换后 ${WAIT} 秒内没重启并就绪"; fi
DAEMON_PID="$(pids_of iwork-daemon | head -1)"

# ---------------------------------------------------------------- 9.6 形态 B 与 9.7 的另两遍
upgrade_round() { # <从> <到>：后台跑着 <从>、客户端关着；整包替换成 <到>，后台自己重启
  local from=$1 to=$2 t pid
  pid="$(pids_of iwork-daemon | head -1)"
  t="$(now_ms)"
  install_package "$to"
  if wait_restart_ready "9.7 ${from}→$to" "$t" \
     && [ "$(cdhash_of_pid "$pid")" = "$(cdhash_of_file "$DEST/Contents/MacOS/iwork-daemon")" ]; then
    local installing
    installing="$(events_since "$t" "$F_INSTALLING" | wc -l | tr -d ' ')"
    note "9.7 ${from}→$to PASS：pid $pid 重启成新版；「安装中」$installing 条"
  else
    note "9.7 ${from}→$to FAIL"
    FAILURES+=("9.7 ${from}→$to")
  fi
}
upgrade_round b-signed a-signed
T6="$(now_ms)"
start_client
wait_event "9.6 形态 B 连接" "$T6" "$(connected_filter "$CLIENT_PID")" >/dev/null || FAILURES+=("9.6 形态 B：客户端没连上")
[ -z "$(events_since "$T6" '.fields.event == "manifest_loaded"')" ] || FAILURES+=("9.6 形态 B：有证书的构建不该读清单")
confirm "形态 B：客户端窗口进到了首页" || FAILURES+=("9.6 形态 B：窗口不是首页")
quit_client
upgrade_round a-signed b-signed
upgrade_round b-signed b-unsigned
upgrade_round b-unsigned a-signed   # 换形态：形态 A → 形态 B

# ---------------------------------------------------------------- 9.8 回退窗口
# 一格的证据：t1 之后后台日志里相关的记录，一行「<日志行号> <ts> <标签>」，交给 window_verdict 判（标签见 lib/drill-window.sh）
window_rows() {
  local since=$1 pid=$2
  [ -f "$LOG_FILE" ] || return 0
  jq -R -r --argjson since "$since" "fromjson? | select(.ts >= \$since)
    | (if ($(connected_filter "$pid")) then \"connected\"
       elif ($F_REJECTED and .fields.pid == $pid) then \"rejected\"
       elif ($F_RESTART) then \"restart\"
       elif ($F_REJECT_CHECK) then \"reject_check\"
       elif ($F_DROPPED) then \"dropped\"
       else empty end) as \$tag
    | \"\\(input_line_number) \\(.ts) \\(\$tag)\"" "$LOG_FILE" 2>/dev/null || true
}
# t4 之后跑着的后台：还是回退前那个进程（原地重启），而且跑的是 A 版 → A；进程换了或映像不是 A → other；取不到 → unknown
window_image() {
  local before=$1 a=$2 now hash
  now="$(pids_of iwork-daemon | head -1)"
  [ -n "$now" ] || { echo unknown; return; }
  [ "$now" = "$before" ] || { echo other; return; }
  hash="$(cdhash_of_pid "$now")"
  [ -n "$hash" ] || { echo unknown; return; }
  [ "$hash" = "$(cdhash_of_file "$PKG/$a/iWork.app/Contents/MacOS/iwork-daemon")" ] && echo A || echo other
}
window_cell() { # <格> <A 版包> <B 版包> <期望 t2：connected|rejected>
  local cell=$1 a=$2 b=$3 expect=$4 attempt
  for attempt in 1 2 3; do
    install_package "$a"; sleep 1
    local tb ready=no pid
    tb="$(now_ms)"
    install_package "$b"
    # 回退前 B 版后台必须真的就绪、跑的是 B 版；不是就记进证据（这一次 INCONCLUSIVE），不吞掉
    if wait_event "$cell B 版就绪" "$tb" "$F_READY" >/dev/null; then
      pid="$(pids_of iwork-daemon | head -1)"
      [ -n "$pid" ] && [ "$(cdhash_of_pid "$pid")" = "$(cdhash_of_file "$PKG/$b/iWork.app/Contents/MacOS/iwork-daemon")" ] && ready=yes
    fi
    pid="$(pids_of iwork-daemon | head -1)"
    sleep "$COOLDOWN"
    roll_back
    local t1 rows="$OUT/window-$cell-$attempt.txt" i image verdict
    t1="$(now_ms)"
    start_client
    # 等到「重启之后又连上」出现或到点；等不等得到都往下走，缺什么由 window_verdict 判成 INCONCLUSIVE
    for i in $(seq 1 $((WAIT * 5))); do
      window_rows "$t1" "$CLIENT_PID" > "$rows"
      awk '$3 == "restart" { r = 1 } r && $3 == "connected" { f = 1 } END { exit !f }' "$rows" && break
      sleep 0.2
    done
    image="$(window_image "$pid" "$a")"
    quit_client
    window_rows "$t1" "$CLIENT_PID" > "$rows"
    verdict="$(window_verdict "$expect" "$t1" $((WAIT * 1000)) "$ready" "$image" < "$rows")"
    case "$verdict" in
      INCONCLUSIVE*) note "$cell 第 $attempt 次 ${verdict}"; continue ;;
      PASS*) confirm "${cell}：窗口里的表现和方案 9.8 写的一样" || verdict="FAIL 人工确认窗口表现不对；${verdict#PASS }" ;;
    esac
    note "$cell ${verdict}"
    case "$verdict" in FAIL*) FAILURES+=("$cell ${verdict}") ;; esac
    return 0
  done
  note "$cell 三次都 INCONCLUSIVE，交回 architect"
}
window_cell W1 a-signed b-signed connected
window_cell W2 a-unsigned b-unsigned rejected

if [ ${#FAILURES[@]} -gt 0 ]; then
  for f in "${FAILURES[@]}"; do note "FAIL：$f"; done
fi
note "演练跑完"
