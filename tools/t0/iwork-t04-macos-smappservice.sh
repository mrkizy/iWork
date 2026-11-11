#!/usr/bin/env bash
# iWork T04 N1（方案第 8 节）：SMAppService 在本项目的构建形态上能不能用；注册、注销对正在跑的进程做了什么；
# launchd 的 KeepAlive 对各种结束方式的重启语义；退路（~/Library/LaunchAgents + launchctl bootstrap/bootout）同样测一遍。
# 也顺带做 N3 第 3 项（launchd 管着的进程 exec 自己后仍是同一个任务）与 N5 A5（不做整包签名的 .app 上 SMAppService 可不可用）。
# 只在 macOS 跑，约 30 分钟（外面套 timeout 时给 5400 秒以上）。
#
# 会装、卸的东西（退出时一律清掉，并在 summary 末尾列出清理后的核对结果）：
#   - SMAppService 注册项 com.iwork.t04probe（agent，plist 在测试包里）
#   - ~/Library/LaunchAgents/com.iwork.t04probe.plist（只在退路一节与第 10 节存在）
#   - launchd 任务 gui/<uid>/com.iwork.t04probe
# 证书放临时钥匙串（不加进搜索列表、关掉自动上锁），跑完删除。不碰登录钥匙串。
# 全程不该弹任何框：不调 sfltool（要管理员授权）、签名前都先解锁临时钥匙串。
# 结果：tools/t0/out/t04-smappservice/
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-smappservice"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-sm.XXXXXX")" && pwd -P)  # 规范化（/private/var/…），日志里的路径才脱得掉
LABEL=com.iwork.t04probe
PLIST_NAME="${LABEL}.plist"
DOMAIN="gui/$(id -u)"
TARGET="${DOMAIN}/${LABEL}"
USER_PLIST="$HOME/Library/LaunchAgents/${PLIST_NAME}"
KC="$WORK/t04.keychain-db"
# 临时钥匙串：建好后关掉自动上锁（默认空闲 5 分钟、睡眠就锁，之后 codesign 会弹框找人要密码）；
# 每次签名前再解锁一次。整个过程不该出现要人输密码的框。
kc_codesign() { security unlock-keychain -p t "$KC"; codesign --keychain "$KC" "$@"; }
SIGNER="iWork T04 Signer"
LOG="$WORK/agent.log"
MODE="$WORK/mode"
CUR_APP=""

cleanup() {
  set +e
  if [ -n "$CUR_APP" ] && [ -x "$CUR_APP/Contents/MacOS/host" ]; then
    "$CUR_APP/Contents/MacOS/host" host unregister "$PLIST_NAME" >/dev/null 2>&1
  fi
  launchctl bootout "$TARGET" >/dev/null 2>&1
  rm -f "$USER_PLIST"
  pkill -f "$WORK/" >/dev/null 2>&1
  security delete-keychain "$KC" >/dev/null 2>&1
  # 有些原始输出是直接重定向写的，最后统一脱敏一遍
  [ -d "$OUT" ] && find "$OUT" -type f -exec sed -i '' -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g" {} +
  rm -rf "$WORK"
}
trap cleanup EXIT
# 被 timeout 或 Ctrl-C 结束时也走 EXIT（bash 对没设 trap 的 TERM 直接退出，不跑 EXIT trap）
trap 'exit 143' TERM INT

rm -rf "$OUT"; mkdir -p "$OUT"; : > "$LOG"
SUMMARY="$OUT/summary.txt"
note() { echo "$*" | tee -a "$SUMMARY"; }
redact() { sed -e "s#/private$WORK#<work>#g" -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g" -e "s#gui/$(id -u)#gui/<uid>#g"; }
t0() { date +%s; }
# 日志里时间戳 > $1 的 start 行
starts_after() { { awk -v t="$1" '$1 > t && /start/' "$LOG" || true; } | wc -l | tr -d ' '; }
first_start_after() { awk -v t="$1" '$1 > t && /start/ {print $1; exit}' "$LOG" || true; }
last_line_matching() { { grep "$1" "$LOG" || true; } | tail -1 | cut -d' ' -f1; }
job_state() { { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*state = //p' | head -1; }
job_pid() { { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*pid = //p' | head -1; }
job_exists() { launchctl print "$TARGET" >/dev/null 2>&1 && echo yes || echo no; }
wait_running() { for _ in $(seq 60); do [ "$(job_state)" = running ] && return 0; sleep 0.5; done; return 1; }

note "# T04 N1 macOS SMAppService / launchd 实测  $(date '+%F %T')"
note "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
note "开始前：$(job_exists | sed 's/yes/已有同名任务（异常）/;s/no/没有同名任务/')；$( [ -e "$USER_PLIST" ] && echo 已有同名 plist（异常） || echo 没有同名 plist)"
[ "$(job_exists)" = no ] && [ ! -e "$USER_PLIST" ] || { note "环境不干净，停"; exit 2; }

# ---------------------------------------------------------------- 探针与证书
cc -fobjc-arc -framework Foundation -framework ServiceManagement "$ROOT/tools/t0/iwork-t04-smprobe.m" -o "$WORK/smprobe"
security create-keychain -p t "$KC"; security unlock-keychain -p t "$KC"
security set-keychain-settings "$KC"
cat > "$WORK/cert.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$SIGNER
OU=IWORKTEST04
[v3]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$WORK/cert.cnf" -keyout "$WORK/c.key" -out "$WORK/c.pem" 2>/dev/null
openssl pkcs12 -export -legacy -inkey "$WORK/c.key" -in "$WORK/c.pem" -out "$WORK/c.p12" -passout pass:t
security import "$WORK/c.p12" -k "$KC" -P t -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k t "$KC" >/dev/null

agent_plist() { # agent_plist <KeepAlive 字典内容> <BundleProgram 或 Program 那一行>
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  $2
  <key>ProgramArguments</key><array><string>agent</string><string>--service</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict>$1</dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>EnvironmentVariables</key><dict>
    <key>T04_LOG</key><string>$LOG</string>
    <key>T04_MODE</key><string>$MODE</string>
  </dict>
  <key>AssociatedBundleIdentifiers</key><string>dev.iwork.t04probe</string>
</dict></plist>
EOF
}
KA_SUCCESS='<key>SuccessfulExit</key><false/>'
KA_CRASHED='<key>Crashed</key><true/>'

make_app() { # make_app <目录> <签名：linker|adhoc|cert|unsigned>
  local app="$1/T04Probe.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
  cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.t04probe</string>
  <key>CFBundleExecutable</key><string>host</string>
  <key>CFBundleName</key><string>T04Probe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
</dict></plist>
EOF
  cp "$WORK/smprobe" "$app/Contents/MacOS/host"; cp "$WORK/smprobe" "$app/Contents/MacOS/agent"
  agent_plist "$KA_SUCCESS" '<key>BundleProgram</key><string>Contents/MacOS/agent</string>' > "$app/Contents/Library/LaunchAgents/$PLIST_NAME"
  case "$2" in
    linker) ;; # cc 的产物本来就带链接器的 ad-hoc 签名；不做整包签名（等同 N5 的形态 A）
    adhoc) codesign -f -s - "$app/Contents/MacOS/agent" 2>/dev/null; codesign -f -s - "$app" 2>/dev/null ;;
    cert) kc_codesign -f -s "$SIGNER" -i dev.iwork.t04probe.agent "$app/Contents/MacOS/agent" 2>/dev/null
          kc_codesign -f -s "$SIGNER" "$app" 2>/dev/null ;;
    unsigned) codesign --remove-signature "$app/Contents/MacOS/host" "$app/Contents/MacOS/agent" ;;
  esac
  echo "$app"
}

sm() { # sm <app> <可执行文件名> <op>
  "$1/Contents/MacOS/$2" host "$3" "$PLIST_NAME" 2>&1 | redact || true
}

# ================================================================ 1–4 各种签名能不能注册
note ""
note "## 1–4 SMAppService.agent 的 register / status / unregister（host = 主程序；helper = 包里另一个可执行文件）"
echo sleep > "$MODE"
WORKING=""
for sig in unsigned linker adhoc cert; do
  d="$WORK/v-$sig"; mkdir -p "$d"; app=$(make_app "$d" "$sig"); CUR_APP="$app"
  : > "$LOG"
  for caller in host agent; do
    r=$(sm "$app" "$caller" register); s1=$(job_exists); sleep 3; st=$(job_state); started=$(grep -c start "$LOG" || true)
    sleep 1
    s=$(sm "$app" "$caller" status)
    u=$(sm "$app" "$caller" unregister); sleep 2; s2=$(job_exists)
    note "  $sig/${caller}：register→ ${r:-<无输出：程序起不来>}"
    note "      launchctl print 有任务=$s1 state=${st:-无} agent 启动次数=${started}；status→ $s"
    note "      unregister→ ${u}；之后有任务=${s2}；agent 收到 SIGTERM=$(grep -c 'got SIGTERM' "$LOG" || true)"
    if [ -z "$WORKING" ] && [ "$caller" = host ] && echo "$r" | grep -q 'ok=1'; then WORKING=$sig; fi
    : > "$LOG"
  done
  launchctl bootout "$TARGET" >/dev/null 2>&1 || true
done
note "  → 主程序调用能注册的第一种形态：${WORKING:-无}"

# 用于 5–7 的形态：优先 linker（= N5 形态 A，A5 的问题），不行就用能注册的那一种
SM_SIG=${WORKING:-}
if [ -n "$SM_SIG" ]; then
  d="$WORK/sm"; mkdir -p "$d"; APP=$(make_app "$d" "$SM_SIG"); CUR_APP="$APP"
fi

# ---------------------------------------------------------------- 公用：第 5、6、7 步（注册方法作为参数）
# 探针在 sleep 时每 0.2 秒重读模式文件，模式一变就由正在跑的那个实例自己行动；测量不用 kickstart -k 换实例。
do_register() { if [ "$1" = sm ]; then sm "$APP" host register >/dev/null; else launchctl bootstrap "$DOMAIN" "$USER_PLIST" 2>/dev/null || note "    （bootstrap rc=$?）"; fi; }
do_unregister() { if [ "$1" = sm ]; then sm "$APP" host unregister >/dev/null; else launchctl bootout "$TARGET" 2>/dev/null || true; fi; }
# 保证有一个 sleep 模式的实例在跑（必要时 kickstart，不带 -k）
ensure_sleeping() {
  echo sleep > "$MODE"
  for _ in 1 2 3; do
    wait_running && return 0
    launchctl kickstart "$TARGET" >/dev/null 2>&1 || true
  done
  return 0
}

step5() { # register 是否立刻起一个实例；手动起的实例握着锁时新实例停在等锁
  local how="$1"
  : > "$LOG"; echo "lock $WORK/lockfile" > "$MODE"
  echo "lock $WORK/lockfile" > "$WORK/mode.manual"
  T04_LOG="$LOG" T04_MODE="$WORK/mode.manual" "$APP/Contents/MacOS/agent" --service --manual & local manual=$!
  sleep 1
  local t; t=$(t0); do_register "$how"; sleep 4
  note "  5 [$how] register 后 4 秒内 launchd 起的实例数=$(starts_after "$t")；state=$(job_state)；launchd 实例在等锁=$(awk -v t="$t" '$1 > t && /waiting lock/' "$LOG" | wc -l | tr -d ' ')；已拿到锁=$(awk -v t="$t" '$1 > t && /got lock/' "$LOG" | wc -l | tr -d ' ')"
  local tk; tk=$(t0); kill -9 "$manual"; wait "$manual" 2>/dev/null || true; sleep 2
  note "    kill -9 手动实例（于 ${tk}）后：launchd 实例拿到锁于 $(last_line_matching 'got lock')"
}

step6() { # 各种结束方式会不会被再拉、隔多久
  local how="$1" ka="$2"
  for m in "exit 0" "exit 12" "exit 20" "exit 21" "signal 9" "signal 15" "signal 11" "abort"; do
    ensure_sleeping
    local t; t=$(t0); echo "$m" > "$MODE"
    sleep 25
    local ended relaunch n
    ended=$(awk -v t="$t" '$1 > t && /(exiting|raising|aborting)/ {print $1; exit}' "$LOG")
    relaunch=$(awk -v e="${ended:-9e18}" '$1 > e && / start / {print $1; exit}' "$LOG")
    n=$(starts_after "$t")
    local gap="-"; [ -n "$relaunch" ] && gap=$(awk -v a="$relaunch" -v b="$ended" 'BEGIN{printf "%.1f", a-b}')
    note "  6 [$how $ka] ${m}：25 秒内再拉起 $n 次；第一次再拉距结束 ${gap}s；last exit：$( { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*last exit code = //p' | head -1)"
  done
  ensure_sleeping
}

step7() { # 注销对正在跑的实例做了什么：先从实例自己，再从另一个进程
  local how="$1" self_mode="$2"
  ensure_sleeping
  local t; t=$(t0); echo "$self_mode" > "$MODE"; sleep 25
  local call ret term last
  call=$(awk -v t="$t" '$1 > t && /calling/ {print $1; exit}' "$LOG")
  ret=$(awk -v t="$t" '$1 > t && /returned/ {print $0; exit}' "$LOG" | cut -d' ' -f3-)
  term=$(awk -v t="$t" '$1 > t && /got SIGTERM/ {print $1; exit}' "$LOG")
  last=$(awk -v t="$t" '$1 > t {l=$0} END {print l}' "$LOG" | cut -d' ' -f1,3-)
  note "  7 [$how] 实例自己注销：调用于 ${call:-无}；返回：${ret:-没返回}；收到 SIGTERM 于 ${term:-没收到}；最后一行：${last}；之后任务还在=$(job_exists)；25 秒内再被拉起=$(starts_after "$t") 次"
  echo sleep > "$MODE"; do_register "$how"; wait_running || true
  local pid; pid=$(job_pid); t=$(t0); do_unregister "$how"; sleep 3
  term=$(awk -v t="$t" '$1 > t && /got SIGTERM/ {print $1; exit}' "$LOG")
  note "  7 [$how] 另一个进程注销：实例 pid ${pid:-?} 收到 SIGTERM 于 ${term:-没收到}（注销于 ${t}）；之后进程还在=$(kill -0 "${pid:-0}" 2>/dev/null && echo 是 || echo 否)；任务还在=$(job_exists)"
}

step_exec() { # N3 第 3 项：launchd 管着的进程 exec 自己之后仍被认作同一个任务
  local how="$1"
  ensure_sleeping
  local pid_before; pid_before=$(job_pid)
  local t; t=$(t0); echo exec > "$MODE"; sleep 3
  local pid_after; pid_after=$(job_pid)
  note "  N3-3 [$how] exec 自己：exec 前 launchctl 的 pid=${pid_before:-?}，exec 后=${pid_after:-?}，state=$(job_state)，带 --execd 的启动行=$(awk -v t="$t" '$1 > t && /--execd/' "$LOG" | wc -l | tr -d ' ')"
  echo sleep > "$MODE"
  local tk; tk=$(t0); kill -9 "${pid_after:-0}" 2>/dev/null || true; sleep 14
  note "    之后 kill -9 它：14 秒内被 launchd 再拉起=$(starts_after "$tk") 次（证明 exec 后仍归这个任务管）"
}

program_line() { { launchctl print "$TARGET" 2>/dev/null || true; } | grep -iE '^[[:space:]]*(program|bundle|path)' | head -3 | tr -s ' \t' ' ' | redact | tr '\n' ';'; }

# ================================================================ 5–7、9 走 SMAppService
if [ -n "$SM_SIG" ]; then
  note ""
  note "## 5–7、9 SMAppService（形态 ${SM_SIG}，plist 在包里，用 BundleProgram）"
  step5 sm
  step6 sm SuccessfulExit=false
  step_exec sm
  step7 sm "unregister $PLIST_NAME"
  note ""
  note "## 9 注册后把 .app 挪个位置"
  echo sleep > "$MODE"; sm "$APP" host register >/dev/null; wait_running || true
  before=$(program_line)
  mkdir -p "$WORK/moved"; mv "$d/T04Probe.app" "$WORK/moved/"; APP="$WORK/moved/T04Probe.app"; CUR_APP="$APP"
  sleep 2
  note "  挪之前 launchctl：${before}"
  note "  挪之后（从新位置调）status：$(sm "$APP" host status)；launchctl：$(program_line)"
  pid=$(job_pid); kill -9 "${pid:-0}" 2>/dev/null || true; sleep 14
  note "  挪之后 kill -9 实例，14 秒内：state=$(job_state)；最后一行：$(tail -1 "$LOG" | cut -d' ' -f3-)"
  note "  从新位置 unregister：$(sm "$APP" host unregister)；任务还在=$(job_exists)"
  launchctl bootout "$TARGET" >/dev/null 2>&1 || true
  CUR_APP=""
fi

# ================================================================ 10 BundleProgram 与 Program：plist 放哪、怎么加载
note ""
note "## 10 LaunchAgent plist：BundleProgram 还是 Program（T09 通知探针报 BundleProgram + bootstrap 得 rc=5）"
note "  每格：写 plist → 等 3 秒看系统有没有自己加载 → launchctl bootstrap 的 rc → 5 秒后 state → bootout、删 plist"
for sig in linker adhoc cert; do
  d10="$WORK/p10-$sig"; mkdir -p "$d10"; app10=$(make_app "$d10" "$sig")
  for key in BundleProgram Program; do
    for where in user work; do
      if [ $key = BundleProgram ]; then line='<key>BundleProgram</key><string>Contents/MacOS/agent</string>'
      else line="<key>Program</key><string>$app10/Contents/MacOS/agent</string>"; fi
      if [ $where = user ]; then pl="$USER_PLIST"; else pl="$WORK/$PLIST_NAME"; fi
      echo sleep > "$MODE"; : > "$LOG"
      # 先确认干净：上一节 SMAppService 注销后 smd 可能还会再提交一次同名任务
      launchctl bootout "$TARGET" >/dev/null 2>&1 || true; sleep 2
      pre=$(job_exists)
      agent_plist "$KA_SUCCESS" "$line" > "$pl"
      sleep 3
      auto=$(job_exists)
      err=$(launchctl bootstrap "$DOMAIN" "$pl" 2>&1) && rc=0 || rc=$?
      sleep 5
      note "  $sig / $key / $( [ $where = user ] && echo '~/Library/LaunchAgents' || echo '临时目录')：写入前已加载=${pre}；写入后自己加载=${auto}；bootstrap rc=${rc}$( [ -n "$err" ] && echo "（$(echo "$err" | tr '\n' ' ' | redact)）")；state=$(job_state)；探针启动=$(grep -c ' start ' "$LOG" || true)"
      launchctl bootout "$TARGET" >/dev/null 2>&1 || true
      rm -f "$pl"; sleep 1
    done
  done
done

# ================================================================ 8 退路：~/Library/LaunchAgents + bootstrap / bootout
note ""
note "## 8 退路：~/Library/LaunchAgents/${PLIST_NAME} + launchctl bootstrap / bootout（Program 指向包里的 agent）"
d="$WORK/fb"; mkdir -p "$d"; APP=$(make_app "$d" linker); CUR_APP=""
for ka in SuccessfulExit Crashed; do
  if [ $ka = SuccessfulExit ]; then kad="$KA_SUCCESS"; else kad="$KA_CRASHED"; fi
  agent_plist "$kad" "<key>Program</key><string>$APP/Contents/MacOS/agent</string>" > "$USER_PLIST"
  echo sleep > "$MODE"
  sleep 3
  note "  [$ka] 写入 plist 3 秒后系统自己加载了吗：$(job_exists)"
  if [ $ka = SuccessfulExit ]; then
    step5 launchd
  else
    launchctl bootstrap "$DOMAIN" "$USER_PLIST" 2>/dev/null || note "    （bootstrap rc=$?）"; wait_running || true
  fi
  step6 launchd "KeepAlive={$ka}"
  if [ $ka = SuccessfulExit ]; then
    step_exec launchd
    step7 launchd "cmd /bin/launchctl bootout $TARGET"
  fi
  launchctl bootout "$TARGET" >/dev/null 2>&1 || true
  rm -f "$USER_PLIST"
done

# ---------------------------------------------------------------- 清理与核对
cp "$LOG" "$OUT/agent-last.log" 2>/dev/null || true
cleanup_check() {
  [ -n "$CUR_APP" ] && "$CUR_APP/Contents/MacOS/host" host unregister "$PLIST_NAME" >/dev/null 2>&1 || true
  launchctl bootout "$TARGET" >/dev/null 2>&1 || true
  rm -f "$USER_PLIST"
  sleep 1
  note ""
  note "## 清理核对"
  note "  launchctl print ${LABEL}：$(job_exists | sed 's/yes/仍存在（未清干净）/;s/no/不存在/')"
  note "  ~/Library/LaunchAgents/${PLIST_NAME}：$( [ -e "$USER_PLIST" ] && echo 仍存在 || echo 不存在)"
  note "  探针进程：$(pgrep -f "$WORK/" | wc -l | tr -d ' ') 个"
  # 不读 BTM 数据库：sfltool dumpbtm 要管理员授权，会弹 SecurityAgent 框（2026-11-11 实测）
}
cleanup_check
note "完成。"
