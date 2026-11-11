#!/usr/bin/env bash
# iWork T04 N1（方案第 8 节）：macOS 登录自启走 LaunchAgent（~/Library/LaunchAgents 的 plist + launchctl bootstrap / bootout）。
# 测：bootstrap 是否立刻起一个实例、等锁接班；KeepAlive 对各种结束方式的重启语义（SuccessfulExit=false，对照 Crashed=true）；
# 实例自己 bootout / 别的进程 bootout 时发生什么；exec 自己后仍归同一个任务（N3 第 3 项）；plist 里 BundleProgram 与 Program。
# SMAppService 那部分的实测已删掉（产品负责人 2026-11-11 定：macOS 只走 LaunchAgent），当时的结论留在 t0-findings 7.2，
# 原始输出在 tools/t0/out/t04-smappservice/。
# 只在 macOS 跑，约 20 分钟（外面套 timeout 时给 3600 秒以上）。
#
# 会装、卸的东西（退出时一律清掉，summary 末尾列出清理后的核对结果）：
#   - ~/Library/LaunchAgents/com.iwork.t04probe.plist 与 launchd 任务 gui/<uid>/com.iwork.t04probe
#   - 测试包 ~/Library/Caches/dev.iwork.t04probe/T04Probe.app（固定路径，跑完删掉）
# 会留下的：macOS 会在「系统设置 › 通用 › 登录项」的「允许在后台」里记一条后台项目（T04Probe / com.iwork.t04probe），
# 首次出现时系统发一条「已添加后台项目」通知。label 与包路径都是固定的，重跑复用同一条记录、不再新增；可以在那里手动关掉。
# 证书放临时钥匙串（不加进搜索列表、关掉自动上锁），跑完删除。不碰登录钥匙串。
# 全程不该弹任何框：不调 SMAppService、不调 sfltool（要管理员授权）、签名前都先解锁临时钥匙串。
# 结果：tools/t0/out/t04-launchagent/
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-launchagent"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-la.XXXXXX")" && pwd -P)  # 规范化（/private/var/…），日志里的路径才脱得掉
LABEL=com.iwork.t04probe
PLIST_NAME="${LABEL}.plist"
DOMAIN="gui/$(id -u)"
TARGET="${DOMAIN}/${LABEL}"
USER_PLIST="$HOME/Library/LaunchAgents/${PLIST_NAME}"
APP_HOME="$HOME/Library/Caches/dev.iwork.t04probe"
APP="$APP_HOME/T04Probe.app"
KC="$WORK/t04.keychain-db"
# 临时钥匙串：建好后关掉自动上锁（默认空闲 5 分钟、睡眠就锁，之后 codesign 会弹框找人要密码）；
# 每次签名前再解锁一次。整个过程不该出现要人输密码的框。
kc_codesign() { security unlock-keychain -p t "$KC"; codesign --keychain "$KC" "$@"; }
SIGNER="iWork T04 Signer"
LOG="$WORK/agent.log"
MODE="$WORK/mode"
MANUAL_PID=""

# 删除一律用 ${VAR:?}：变量为空时中止，不会删到别处
cleanup() {
  set +e
  launchctl bootout "$TARGET" >/dev/null 2>&1
  rm -f -- "${USER_PLIST:?}" "${WORK:?}/${PLIST_NAME:?}"
  [ -n "$MANUAL_PID" ] && kill -9 "$MANUAL_PID" >/dev/null 2>&1
  rm -rf -- "${APP_HOME:?}"
  security delete-keychain "$KC" >/dev/null 2>&1
  # 有些原始输出是直接重定向写的，最后统一脱敏一遍
  [ -d "$OUT" ] && find "$OUT" -type f -exec sed -i '' -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g" {} +
  rm -rf -- "${WORK:?}"
}
trap cleanup EXIT  # bash 3.2 收到 TERM、INT 也会跑 EXIT trap（实测），被 timeout 掐断时照样清理

rm -rf -- "${OUT:?}"; mkdir -p "$OUT"; : > "$LOG"
SUMMARY="$OUT/summary.txt"
note() { echo "$*" | tee -a "$SUMMARY"; }
redact() { sed -e "s#/private$WORK#<work>#g" -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g" -e "s#gui/$(id -u)#gui/<uid>#g"; }
t0() { date +%s; }
# 日志里时间戳 > $1 的 start 行
starts_after() { { awk -v t="$1" '$1 > t && /start/' "$LOG" || true; } | wc -l | tr -d ' '; }
last_line_matching() { { grep "$1" "$LOG" || true; } | tail -1 | cut -d' ' -f1; }
job_state() { { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*state = //p' | head -1; }
job_pid() { { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*pid = //p' | head -1; }
job_exists() { launchctl print "$TARGET" >/dev/null 2>&1 && echo yes || echo no; }
wait_running() { for _ in $(seq 60); do [ "$(job_state)" = running ] && return 0; sleep 0.5; done; return 1; }

note "# T04 N1 macOS LaunchAgent 实测  $(date '+%F %T')"
note "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
note "开始前：$(job_exists | sed 's/yes/已有同名任务（异常）/;s/no/没有同名任务/')；$( [ -e "$USER_PLIST" ] && echo 已有同名 plist（异常） || echo 没有同名 plist)"
[ "$(job_exists)" = no ] && [ ! -e "$USER_PLIST" ] || { note "环境不干净，停"; exit 2; }

# ---------------------------------------------------------------- 探针与证书
cc -fobjc-arc -framework Foundation "$ROOT/tools/t0/iwork-t04-launchprobe.m" -o "$WORK/launchprobe"
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
</dict></plist>
EOF
}
KA_SUCCESS='<key>SuccessfulExit</key><false/>'
KA_CRASHED='<key>Crashed</key><true/>'
PROGRAM_LINE="<key>Program</key><string>$APP/Contents/MacOS/agent</string>"

make_app() { # make_app <签名：linker|adhoc|cert>：在固定路径重建测试包
  rm -rf -- "${APP:?}"
  mkdir -p "$APP/Contents/MacOS"
  cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.t04probe</string>
  <key>CFBundleExecutable</key><string>agent</string>
  <key>CFBundleName</key><string>T04Probe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
</dict></plist>
EOF
  cp "$WORK/launchprobe" "$APP/Contents/MacOS/agent"
  case "$1" in
    linker) ;; # cc 的产物本来就带链接器的 ad-hoc 签名；不做整包签名（等同 N5 的形态 A）
    adhoc) codesign -f -s - "$APP" 2>/dev/null ;;
    cert) kc_codesign -f -s "$SIGNER" "$APP" 2>/dev/null ;;
  esac
}

# ---------------------------------------------------------------- 公用：第 5、6、7 步
# 探针在 sleep 时每 0.2 秒重读模式文件，模式一变就由正在跑的那个实例自己行动；测量不用 kickstart -k 换实例。
do_register() { launchctl bootstrap "$DOMAIN" "$USER_PLIST" 2>/dev/null || note "    （bootstrap rc=$?）"; }
do_unregister() { launchctl bootout "$TARGET" 2>/dev/null || true; }
# 保证有一个 sleep 模式的实例在跑（必要时 kickstart，不带 -k）
ensure_sleeping() {
  echo sleep > "$MODE"
  for _ in 1 2 3; do
    wait_running && return 0
    launchctl kickstart "$TARGET" >/dev/null 2>&1 || true
  done
  return 0
}

step5() { # bootstrap 是否立刻起一个实例；手动起的实例握着锁时新实例停在等锁
  : > "$LOG"; echo "lock $WORK/lockfile" > "$MODE"
  echo "lock $WORK/lockfile" > "$WORK/mode.manual"
  T04_LOG="$LOG" T04_MODE="$WORK/mode.manual" "$APP/Contents/MacOS/agent" --service --manual & MANUAL_PID=$!
  sleep 1
  local t; t=$(t0); do_register; sleep 4
  note "  5 bootstrap 后 4 秒内 launchd 起的实例数=$(starts_after "$t")；state=$(job_state)；launchd 实例在等锁=$(awk -v t="$t" '$1 > t && /waiting lock/' "$LOG" | wc -l | tr -d ' ')；已拿到锁=$(awk -v t="$t" '$1 > t && /got lock/' "$LOG" | wc -l | tr -d ' ')"
  local tk; tk=$(t0); kill -9 "$MANUAL_PID"; wait "$MANUAL_PID" 2>/dev/null || true; MANUAL_PID=""; sleep 2
  note "    kill -9 手动实例（于 ${tk}）后：launchd 实例拿到锁于 $(last_line_matching 'got lock')"
}

step6() { # 各种结束方式会不会被再拉、隔多久
  local ka="$1"
  for m in "exit 0" "exit 12" "exit 20" "exit 21" "signal 9" "signal 15" "signal 11" "abort"; do
    ensure_sleeping
    local t; t=$(t0); echo "$m" > "$MODE"
    sleep 25
    local ended relaunch n
    ended=$(awk -v t="$t" '$1 > t && /(exiting|raising|aborting)/ {print $1; exit}' "$LOG")
    relaunch=$(awk -v e="${ended:-9e18}" '$1 > e && / start / {print $1; exit}' "$LOG")
    n=$(starts_after "$t")
    local gap="-"; [ -n "$relaunch" ] && gap=$(awk -v a="$relaunch" -v b="$ended" 'BEGIN{printf "%.1f", a-b}')
    note "  6 [${ka}] ${m}：25 秒内起了 $n 次；结束后第一次再拉隔 ${gap}s；last exit：$( { launchctl print "$TARGET" 2>/dev/null || true; } | sed -n 's/^[[:space:]]*last exit code = //p' | head -1)"
  done
  ensure_sleeping
}

step7() { # bootout 对正在跑的实例做了什么：先从实例自己，再从另一个进程
  ensure_sleeping
  local t; t=$(t0); echo "cmd /bin/launchctl bootout $TARGET" > "$MODE"; sleep 25
  local call ret term last
  call=$(awk -v t="$t" '$1 > t && /calling/ {print $1; exit}' "$LOG")
  ret=$(awk -v t="$t" '$1 > t && /returned/ {print $0; exit}' "$LOG" | cut -d' ' -f3-)
  term=$(awk -v t="$t" '$1 > t && /got SIGTERM/ {print $1; exit}' "$LOG")
  last=$(awk -v t="$t" '$1 > t {l=$0} END {print l}' "$LOG" | cut -d' ' -f1,3-)
  note "  7 实例自己 bootout：调用于 ${call:-无}；返回：${ret:-没返回}；收到 SIGTERM 于 ${term:-没收到}；最后一行：${last}；之后任务还在=$(job_exists)；25 秒内又起了 $(starts_after "$t") 次"
  echo sleep > "$MODE"; do_register; wait_running || true
  local pid; pid=$(job_pid); t=$(t0); do_unregister; sleep 3
  term=$(awk -v t="$t" '$1 > t && /got SIGTERM/ {print $1; exit}' "$LOG")
  note "  7 另一个进程 bootout：实例 pid ${pid:-?} 收到 SIGTERM 于 ${term:-没收到}（bootout 于 ${t}）；之后进程还在=$(kill -0 "${pid:-0}" 2>/dev/null && echo 是 || echo 否)；任务还在=$(job_exists)"
}

step_exec() { # N3 第 3 项：launchd 管着的进程 exec 自己之后仍被认作同一个任务
  ensure_sleeping
  local pid_before; pid_before=$(job_pid)
  local t; t=$(t0); echo exec > "$MODE"; sleep 3
  local pid_after; pid_after=$(job_pid)
  note "  N3-3 exec 自己：exec 前 launchctl 的 pid=${pid_before:-?}，exec 后=${pid_after:-?}，state=$(job_state)，带 --execd 的启动行=$(awk -v t="$t" '$1 > t && /--execd/' "$LOG" | wc -l | tr -d ' ')"
  echo sleep > "$MODE"
  local tk; tk=$(t0); kill -9 "${pid_after:-0}" 2>/dev/null || true; sleep 14
  note "    之后 kill -9 它：14 秒内被 launchd 再拉起=$(starts_after "$tk") 次（证明 exec 后仍归这个任务管）"
}

# ================================================================ 10 BundleProgram 与 Program：plist 放哪、怎么加载
note ""
note "## 10 LaunchAgent plist：BundleProgram 还是 Program（T09 通知探针报 BundleProgram + bootstrap 得 rc=5）"
note "  每格：确认干净 → 写 plist → 等 3 秒看系统有没有自己加载 → launchctl bootstrap 的 rc → 5 秒后 state → bootout、删 plist"
for sig in linker adhoc cert; do
  make_app "$sig"
  for key in BundleProgram Program; do
    for where in user work; do
      if [ $key = BundleProgram ]; then line='<key>BundleProgram</key><string>Contents/MacOS/agent</string>'
      else line="$PROGRAM_LINE"; fi
      if [ $where = user ]; then pl="$USER_PLIST"; else pl="$WORK/$PLIST_NAME"; fi
      echo sleep > "$MODE"; : > "$LOG"
      launchctl bootout "$TARGET" >/dev/null 2>&1 || true; sleep 2
      pre=$(job_exists)
      agent_plist "$KA_SUCCESS" "$line" > "$pl"
      sleep 3
      auto=$(job_exists)
      err=$(launchctl bootstrap "$DOMAIN" "$pl" 2>&1) && rc=0 || rc=$?
      sleep 5
      note "  ${sig} / ${key} / $( [ $where = user ] && echo '~/Library/LaunchAgents' || echo '临时目录')：写入前已加载=${pre}；写入后自己加载=${auto}；bootstrap rc=${rc}$( [ -n "$err" ] && echo "（$(echo "$err" | tr '\n' ' ' | redact)）")；state=$(job_state)；探针启动=$(grep -c ' start ' "$LOG" || true)"
      launchctl bootout "$TARGET" >/dev/null 2>&1 || true
      rm -f -- "${pl:?}"; sleep 1
    done
  done
done

# ================================================================ 5–7 LaunchAgent（Program 指向固定路径的测试包）
note ""
note "## 5–7 ~/Library/LaunchAgents/${PLIST_NAME} + launchctl bootstrap / bootout（Program 指向包里的 agent，链接器签名的包）"
make_app linker
for ka in SuccessfulExit Crashed; do
  if [ $ka = SuccessfulExit ]; then kad="$KA_SUCCESS"; else kad="$KA_CRASHED"; fi
  agent_plist "$kad" "$PROGRAM_LINE" > "$USER_PLIST"
  echo sleep > "$MODE"
  sleep 3
  note "  [${ka}] 写入 plist 3 秒后系统自己加载了吗：$(job_exists)"
  if [ $ka = SuccessfulExit ]; then
    step5
  else
    do_register; wait_running || true
  fi
  step6 "KeepAlive={${ka}}"
  if [ $ka = SuccessfulExit ]; then
    step_exec
    step7
  fi
  launchctl bootout "$TARGET" >/dev/null 2>&1 || true
  rm -f -- "${USER_PLIST:?}"
done

# ---------------------------------------------------------------- 清理与核对
cp "$LOG" "$OUT/agent-last.log" 2>/dev/null || true
launchctl bootout "$TARGET" >/dev/null 2>&1 || true
rm -f -- "${USER_PLIST:?}"
rm -rf -- "${APP_HOME:?}"
sleep 1
note ""
note "## 清理核对"
note "  launchctl print ${LABEL}：$(job_exists | sed 's/yes/仍存在（未清干净）/;s/no/不存在/')"
note "  ~/Library/LaunchAgents/${PLIST_NAME}：$( [ -e "$USER_PLIST" ] && echo 仍存在 || echo 不存在)"
note "  测试包 ~/Library/Caches/dev.iwork.t04probe：$( [ -e "$APP_HOME" ] && echo 仍存在 || echo 已删)"
# launchd 起的实例 argv 是「agent --service」，手动起的是包内绝对路径：两种都数。
# 匹配串拆成两段拼起来，awk 自己的命令行里没有连续的这一串，不会把自己数进去
note "  探针进程：$(ps -axo args= | awk '($1 == "agent" && $2 == "--service") || index($0, "T04Probe" ".app/Contents/MacOS/agent")' | wc -l | tr -d ' ') 个"
note "完成。"
