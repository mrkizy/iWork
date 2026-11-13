#!/usr/bin/env bash
# 把重测试放到 mac120 上跑（产品负责人 2026-11-14 定：全量测试、clippy、变异、大规模属性测试放 mac120，
# 本机只跑定向测试和必须本机的，GitHub CI 兜底）。
#
# 用法：
#   scripts/remote-heavy.sh [run] [选项] <分支|提交> -- <命令…>
#       选项：--task <名字>        任务名，决定远端的 worktree 与 target；默认取分支名（/ 换成 -）
#             --timeout <时长>     拿到锁、检出完成后开始计时；90、30m、2h 都行；默认 2h
#             --min-free-gb <GB>   远端剩余空间低于它就拒绝；默认 50
#       例：scripts/remote-heavy.sh t10a-b -- cargo test --workspace --no-fail-fast
#           scripts/remote-heavy.sh t10a-b -- env PROPTEST_CASES=5000 cargo test -p iwork-core --test engine_fold
#   scripts/remote-heavy.sh clean <任务名>   删这个任务的 worktree、target、日志（等锁，不打断正在跑的）
#   scripts/remote-heavy.sh list             看剩余空间、PERF_WINDOW、谁占着锁、各任务占多大
#   scripts/remote-heavy.sh --selftest       本机用假 ssh 跑一遍，证明拒绝未推送提交、锁、PERF_WINDOW、超时清进程组、退出码透传
#
# 退出码：命令自己的退出码原样返回；另外 2 = 用法错误或被拒绝（未推送、PERF_WINDOW、空间不够、任务名不合法），
#         124 = 超时（进程组已清掉），129 = 远端发现 ssh 断了（只会出现在远端日志里），255 = ssh 本身失败。
#
# 远端布局（mac120 的 ~/work/iwork-remote/，不碰 ~/work/iWork，那份留给 T11 正式性能验收）：
#   repo.git/              只从 origin 抓取的裸仓库，各任务的 worktree 都挂在它上面
#   tasks/<任务>/src/      按提交号检出的 worktree（每次运行前 checkout --force + clean -ffdx）
#   tasks/<任务>/target/   这个任务专用的 CARGO_TARGET_DIR（不同任务不共用，免得串产物，见 STATUS「踩过的坑」）
#   tasks/<任务>/tmp/      命令的 TMPDIR
#   tasks/<任务>/logs/     每次运行一份日志
#   heavy.lock / heavy.holder   重测试锁与当前持有者（holder 只供显示）
#   repo.lock              抓取、建删 worktree 的短锁
#   PERF_WINDOW            存在时拒绝起新的重测试（T11 夜间测量手动建、测完删）
#
# 只用 ssh 和 git：不开端口（ClearAllForwardings）、不转发 agent、不改系统设置与钥匙串（git 抓取关掉凭据助手）。
# 命令在 env -i 下启动，只带下面 rh_command_env 列出的变量，本机和 ssh 会话里的环境变量一个都不带进去。
#
# 测试替身：REMOTE_HEAVY_SSH（替换 ssh 程序）、REMOTE_HEAVY_ORIGIN（远端抓取的地址），只给 --selftest 用。
set -euo pipefail

HOST="mac120"
ORIGIN_URL="${REMOTE_HEAVY_ORIGIN:-https://github.com/mrkizy/iWork.git}"
DEFAULT_TIMEOUT="2h"
DEFAULT_MIN_FREE_GB=50
TASK_RE='^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'

die_usage() {
  echo "remote-heavy: $*" >&2
  echo "用法见 $0 文件头" >&2
  exit 2
}

# ---------------------------------------------------------------------------
# 远端程序：rh_ 开头的函数经 declare -f 原样发到 mac120，由那边的 /bin/bash（3.2）执行。
# 只能用 bash 3.2 的语法，只能依赖 macOS 自带的 git、perl、df、du。
# ---------------------------------------------------------------------------

rh_say() {
  printf 'remote-heavy[%s]: %s\n' "$(hostname -s)" "$*"
  if [ -n "${LOG:-}" ]; then printf 'remote-heavy: %s\n' "$*" >>"$LOG"; fi
}

rh_refuse() {
  rh_say "拒绝：$*" >&2
  # 拿着重测试锁时被拒，持有者记录也要撤掉，免得别人等锁时看到过期的名字（锁本身随进程退出释放）。
  if [ "${HOLDING_HEAVY:-0}" -eq 1 ]; then rm -f "$HOLDER"; fi
  exit 2
}

rh_main() {
  set -u
  set -o pipefail
  # ssh 断了以后往 stdout 写会收到 SIGPIPE；忽略它，好让看门狗走完清进程组。
  trap '' PIPE
  ROOT="$HOME/work/iwork-remote"
  REPO="$ROOT/repo.git"
  TASKS="$ROOT/tasks"
  HEAVY_LOCK="$ROOT/heavy.lock"
  HOLDER="$ROOT/heavy.holder"
  REPO_LOCK="$ROOT/repo.lock"
  PERF_MARK="$ROOT/PERF_WINDOW"
  # 发起这次运行的 sshd 会话进程；它没了就说明连接断了。
  SESSION_PID=$PPID
  LOG=""
  mkdir -p "$TASKS" || rh_refuse "建不了 $TASKS"
  case "$R_MODE" in
    run) rh_run ;;
    clean) rh_clean ;;
    list) rh_list ;;
    *) rh_refuse "未知模式 $R_MODE" ;;
  esac
}

rh_check_task_name() {
  printf '%s' "$R_TASK" | grep -Eq "$R_TASK_RE" || rh_refuse "任务名不合法：'$R_TASK'"
}

# PERF_WINDOW 与剩余空间：起跑前查一次，拿到锁后再查一次（等锁期间可能有人建了标记或占了空间）。
rh_check_gate() {
  if [ -e "$PERF_MARK" ]; then
    rh_refuse "$PERF_MARK 存在：正式性能验收时段，不起新的重测试"
  fi
  local free_kb
  free_kb=$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')
  case "$free_kb" in '' | *[!0-9]*) rh_refuse "读不出 $ROOT 的剩余空间（df 输出：${free_kb}）" ;; esac
  local free_gb=$((free_kb / 1024 / 1024))
  if [ "$free_gb" -lt "$R_MIN_FREE_GB" ]; then
    rh_refuse "剩余空间 ${free_gb}G，低于阈值 ${R_MIN_FREE_GB}G；先 clean 不用的任务（list 看占用）"
  fi
}

rh_session_gone() {
  ! kill -0 "$SESSION_PID" 2>/dev/null
}

# 在已经打开的描述符上加排他 flock。用 perl 是因为 macOS 没有 flock(1)；
# 锁挂在打开的文件描述上，perl 退出后本 shell 手里的那份描述符还在，锁就还在，关掉描述符即释放。
rh_flock() { # fd nonblock(0|1)
  perl -MFcntl=:flock -e '
    my ($fd, $nb) = @ARGV;
    open(my $fh, ">&=", $fd) or die "fdopen $fd: $!\n";
    exit(flock($fh, LOCK_EX | ($nb ? LOCK_NB : 0)) ? 0 : 1);
  ' "$1" "$2"
}

# 短锁：抓取与 worktree 增删，几秒就放。
rh_lock_repo() {
  exec 8>>"$REPO_LOCK" || rh_refuse "打不开 $REPO_LOCK"
  rh_flock 8 0 || rh_refuse "拿不到 $REPO_LOCK"
}
rh_unlock_repo() { exec 8>&-; }

# 重测试锁：非阻塞轮询，等的时候照看 ssh 会话，断了就不再排队（不然会在没人看的时候跑起来）。
rh_lock_heavy() {
  exec 9>>"$HEAVY_LOCK" || rh_refuse "打不开 $HEAVY_LOCK"
  local announced=0 waited=0
  while ! rh_flock 9 1; do
    if [ "$announced" -eq 0 ]; then
      rh_say "等锁：$(cat "$HOLDER" 2>/dev/null || echo '持有者未知')"
      announced=1
    fi
    if rh_session_gone; then exit 129; fi
    sleep 2
    waited=$((waited + 2))
    if [ $((waited % 300)) -eq 0 ]; then rh_say "仍在等锁（${waited}s）"; fi
  done
  HOLDING_HEAVY=1
  if [ "$announced" -eq 1 ]; then rh_say "拿到锁（等了约 ${waited}s）"; fi
}
rh_unlock_heavy() {
  rm -f "$HOLDER"
  HOLDING_HEAVY=0
  exec 9>&-
}

rh_fetch_and_verify() {
  if [ ! -d "$REPO" ]; then
    git init -q --bare "$REPO" || rh_refuse "建不了裸仓库 $REPO"
  fi
  git -C "$REPO" config remote.origin.url "$R_ORIGIN"
  # 关掉凭据助手与终端提示：公开仓库不需要凭据，也绝不去读钥匙串。
  GIT_TERMINAL_PROMPT=0 git -C "$REPO" -c credential.helper= fetch --quiet --prune origin \
    '+refs/heads/*:refs/remotes/origin/*' || rh_refuse "从 $R_ORIGIN 抓取失败"
  if ! git -C "$REPO" cat-file -e "$R_SHA^{commit}" 2>/dev/null ||
    [ -z "$(git -C "$REPO" for-each-ref --count=1 --contains "$R_SHA" refs/remotes/origin)" ]; then
    rh_refuse "提交 $R_SHA 不在 origin 的任何分支上：先 git push 再跑（本机未提交、未推送的改动不会带过去）"
  fi
}

rh_checkout() { # src
  local src=$1
  if [ -e "$src/.git" ]; then
    git -C "$src" checkout -q --detach --force "$R_SHA" &&
      git -C "$src" clean -q -ffdx || rh_refuse "检出 $R_SHA 到 $src 失败"
  else
    git -C "$REPO" worktree prune
    git -C "$REPO" worktree add -q --detach "$src" "$R_SHA" || rh_refuse "建 worktree $src 失败"
  fi
  [ "$(git -C "$src" rev-parse HEAD)" = "$R_SHA" ] || rh_refuse "$src 检出后 HEAD 不是 $R_SHA"
  [ -z "$(git -C "$src" status --porcelain --ignored)" ] || rh_refuse "$src 检出后不干净"
}

# 命令能看到的全部环境变量。改这里就是改远端命令的环境。
rh_command_env() { # task_dir
  local td=$1
  RH_ENV=(
    "HOME=$HOME"
    "USER=$(id -un)"
    "LOGNAME=$(id -un)"
    "PATH=$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    "LANG=en_US.UTF-8"
    "TMPDIR=$td/tmp"
    "CARGO_TARGET_DIR=$td/target"
    "CARGO_INCREMENTAL=0"
  )
}

# 先 TERM 整组，10 秒还有活的就 KILL。返回 0 表示组里原本还有进程。
rh_kill_group() { # pgid
  local pgid=$1 i=0
  kill -0 -- "-$pgid" 2>/dev/null || return 1
  kill -TERM -- "-$pgid" 2>/dev/null
  while kill -0 -- "-$pgid" 2>/dev/null && [ "$i" -lt 10 ]; do
    sleep 1
    i=$((i + 1))
  done
  kill -KILL -- "-$pgid" 2>/dev/null
  return 0
}

rh_run() {
  rh_check_task_name
  rh_check_gate
  rh_lock_repo
  rh_fetch_and_verify
  rh_unlock_repo

  rh_lock_heavy
  printf 'task=%s sha=%s pid=%s since=%s cmd=%s\n' "$R_TASK" "$R_SHA" "$$" \
    "$(date '+%F %T')" "${R_CMD[*]}" >"$HOLDER"
  rh_check_gate

  local td="$TASKS/$R_TASK"
  mkdir -p "$td/target" "$td/tmp" "$td/logs"
  LOG="$td/logs/$(date -u +%Y%m%dT%H%M%SZ)-${R_SHA:0:12}.log"
  : >"$LOG"
  rh_lock_repo
  rh_checkout "$td/src"
  rh_unlock_repo

  rh_command_env "$td"
  rh_say "任务 $R_TASK  提交 $R_SHA  超时 ${R_TIMEOUT}s"
  rh_say "命令：${R_CMD[*]}"
  rh_say "日志：$LOG"

  # 命令自成进程组（perl setpgrp），超时或会话断开时整组收掉；锁的描述符不传给命令，
  # 漏网的子孙进程也拿不住锁。
  perl -e 'setpgrp(0, 0) or die "setpgrp: $!\n"; exec { $ARGV[0] } @ARGV or die "exec: $!\n"' \
    /bin/bash -c 'set -o pipefail; cd "$1" || exit 2; log=$2; shift 2; "$@" 2>&1 | tee -a "$log"' \
    rh-command "$td/src" "$LOG" /usr/bin/env -i "${RH_ENV[@]}" "${R_CMD[@]}" \
    </dev/null 8>&- 9>&- &
  local pid=$! start now deadline timed_out=0 rc
  # 命令已经带着自己的 stderr 起来了。外层从这里起只经 rh_say 写 stdout；把外层 stderr 丢掉，
  # 是为了不让 bash 在收掉进程组后打出「Terminated: 15 <整条命令>」这种作业通知（超时已经报过）。
  exec 2>/dev/null
  start=$(date +%s)
  deadline=$((start + R_TIMEOUT))
  while kill -0 "$pid" 2>/dev/null; do
    if rh_session_gone; then
      rh_say "ssh 会话断了，收掉进程组 $pid"
      rh_kill_group "$pid"
      rh_unlock_heavy
      exit 129
    fi
    now=$(date +%s)
    if [ "$now" -ge "$deadline" ]; then
      timed_out=1
      rh_say "超时（${R_TIMEOUT}s），收掉进程组 $pid"
      rh_kill_group "$pid"
      break
    fi
    sleep 1
  done
  wait "$pid"
  rc=$?
  if [ "$timed_out" -eq 1 ]; then rc=124; fi
  if rh_kill_group "$pid"; then rh_say "命令退出后进程组 $pid 里还有残留进程，已收掉"; fi
  now=$(date +%s)
  rh_say "结束：退出码 ${rc}，耗时 $((now - start))s，日志 $(hostname -s):$LOG"
  rh_unlock_heavy
  exit "$rc"
}

rh_clean() {
  rh_check_task_name
  local td="$TASKS/$R_TASK"
  if [ ! -e "$td" ]; then
    rh_say "没有任务 ${R_TASK}，不用清"
    exit 0
  fi
  rh_lock_heavy
  printf 'task=%s pid=%s since=%s cmd=clean\n' "$R_TASK" "$$" "$(date '+%F %T')" >"$HOLDER"
  local size
  size=$(du -sh "$td" 2>/dev/null | awk '{print $1}')
  rh_lock_repo
  if [ -e "$td/src/.git" ]; then git -C "$REPO" worktree remove --force "$td/src" || true; fi
  rm -rf "$td" || rh_refuse "删不掉 $td"
  if [ -d "$REPO" ]; then git -C "$REPO" worktree prune; fi
  rh_unlock_repo
  rh_unlock_heavy
  rh_say "已删任务 ${R_TASK}（约 ${size}）"
}

rh_list() {
  rh_say "剩余空间：$(df -Ph "$ROOT" | awk 'NR==2 {print $4}')"
  if [ -e "$PERF_MARK" ]; then rh_say "PERF_WINDOW：存在（拒绝新的重测试）"; else rh_say "PERF_WINDOW：无"; fi
  rh_say "锁：$(cat "$HOLDER" 2>/dev/null || echo '空闲')"
  local td
  for td in "$TASKS"/*; do
    [ -d "$td" ] || continue
    printf '  %-32s %8s  %s\n' "$(basename "$td")" "$(du -sh "$td" 2>/dev/null | awk '{print $1}')" \
      "$(git -C "$td/src" rev-parse --short=12 HEAD 2>/dev/null || echo '-')"
  done
}

# ---------------------------------------------------------------------------
# 本机部分
# ---------------------------------------------------------------------------

parse_seconds() { # 90 | 30m | 2h
  local v=$1 n unit
  n=${v%[smh]}
  unit=${v#"$n"}
  case "$n" in '' | *[!0-9]*) die_usage "时长不合法：$v" ;; esac
  case "$unit" in
    '' | s) echo "$n" ;;
    m) echo $((n * 60)) ;;
    h) echo $((n * 3600)) ;;
  esac
}

# 把远端程序与参数拼成一段脚本，经 ssh 的 stdin 交给远端 bash -s。
# 参数用 printf %q 写成赋值语句，不经远端登录 shell（zsh）再解析一遍。
send_remote() { # mode task sha timeout min_free cmd...
  local mode=$1 task=$2 sha=$3 timeout=$4 min_free=$5
  shift 5
  local ssh_cmd
  if [ -n "${REMOTE_HEAVY_SSH:-}" ]; then
    ssh_cmd=("$REMOTE_HEAVY_SSH")
  else
    ssh_cmd=(ssh -o BatchMode=yes -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes
      -o PermitLocalCommand=no -o 'SendEnv=-*' -o ServerAliveInterval=30 -o ServerAliveCountMax=4)
  fi
  local program
  program=$(
    printf 'R_MODE=%q\nR_TASK=%q\nR_SHA=%q\nR_TIMEOUT=%q\nR_MIN_FREE_GB=%q\nR_ORIGIN=%q\nR_TASK_RE=%q\n' \
      "$mode" "$task" "$sha" "$timeout" "$min_free" "$ORIGIN_URL" "$TASK_RE"
    printf 'R_CMD=('
    if [ "$#" -gt 0 ]; then printf ' %q' "$@"; fi
    printf ' )\n'
    declare -f $(compgen -A function rh_)
    # 整段先读完再执行：rh_main 里的命令都不读 stdin（</dev/null），脚本本身就是 stdin。
    printf 'rh_main\n'
  )
  local rc=0
  printf '%s\n' "$program" | "${ssh_cmd[@]}" "$HOST" /bin/bash -s || rc=$?
  if [ "$rc" -eq 255 ]; then echo "remote-heavy: ssh 失败或连接断开（退出码 255）" >&2; fi
  return "$rc"
}

cmd_run() {
  local task="" timeout="$DEFAULT_TIMEOUT" min_free="$DEFAULT_MIN_FREE_GB" ref=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --task) task=${2:-}; shift 2 || die_usage "--task 缺值" ;;
      --timeout) timeout=${2:-}; shift 2 || die_usage "--timeout 缺值" ;;
      --min-free-gb) min_free=${2:-}; shift 2 || die_usage "--min-free-gb 缺值" ;;
      --) die_usage "-- 前面要给分支或提交" ;;
      -*) die_usage "未知选项 $1" ;;
      *) ref=$1; shift; break ;;
    esac
  done
  [ -n "$ref" ] || die_usage "缺分支或提交"
  [ "${1:-}" = "--" ] || die_usage "分支或提交后面要跟 -- <命令…>"
  shift
  [ "$#" -gt 0 ] || die_usage "-- 后面没有命令"
  local seconds
  seconds=$(parse_seconds "$timeout")
  [ "$seconds" -gt 0 ] || die_usage "超时必须大于 0"
  case "$min_free" in '' | *[!0-9]*) die_usage "--min-free-gb 要是整数：$min_free" ;; esac

  # 只取提交号：远端照提交号检出，本机工作区里的东西一概不带。是否已推送由远端对着 origin 判定。
  local sha
  sha=$(git rev-parse --verify --quiet "${ref}^{commit}") ||
    sha=$(git rev-parse --verify --quiet "origin/${ref}^{commit}") ||
    die_usage "本机解析不了 '$ref'"
  if [ -z "$task" ]; then
    [ "$ref" != "HEAD" ] || die_usage "用 HEAD 时请给 --task"
    task=${ref//\//-}
  fi
  printf '%s' "$task" | grep -Eq "$TASK_RE" || die_usage "任务名不合法：'$task'（只许字母数字 . _ -，最长 64）"
  if ! git diff-index --quiet HEAD -- 2>/dev/null; then
    echo "remote-heavy: 提示：本机有未提交改动，不会带到远端；远端只跑 $sha" >&2
  fi
  send_remote run "$task" "$sha" "$seconds" "$min_free" "$@"
}

cmd_clean() {
  [ "$#" -eq 1 ] || die_usage "clean 要一个任务名"
  printf '%s' "$1" | grep -Eq "$TASK_RE" || die_usage "任务名不合法：'$1'"
  send_remote clean "$1" "" 0 0
}

cmd_list() {
  [ "$#" -eq 0 ] || die_usage "list 不带参数"
  send_remote list "" "" 0 0
}

# ---------------------------------------------------------------------------
# 自测：假 ssh 在本机起 bash -s，HOME 指到临时目录，origin 是临时裸仓库。
# 假 ssh 把本机环境整个传过去（比真 ssh 更坏的情况），用来证明命令的环境只来自白名单。
# ---------------------------------------------------------------------------

selftest() {
  local self
  # tmp 不用 local：EXIT trap 在函数返回后才跑，要还能读到它。
  self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/remote-heavy-selftest.XXXXXX")"
  # 自测里起的后台运行（锁、超时用例）在出错退出时也要收掉。
  trap 'jobs -p | xargs kill 2>/dev/null; rm -rf "$tmp"' EXIT
  local home="$tmp/home" root="$tmp/home/work/iwork-remote" out="$tmp/out"
  mkdir -p "$home" "$out"
  cat >"$tmp/fake-ssh" <<EOF
#!/bin/bash
env >"$out/ssh-env"
exec env HOME="$home" /bin/bash -s
EOF
  chmod +x "$tmp/fake-ssh"

  git init -q --bare "$tmp/origin.git"
  git clone -q "$tmp/origin.git" "$tmp/work" 2>/dev/null
  cd "$tmp/work"
  git -c user.name=t -c user.email=t@t commit -q --allow-empty -m init
  echo committed >file.txt
  git add file.txt
  git -c user.name=t -c user.email=t@t commit -q -m pushed
  git push -q origin HEAD:refs/heads/main 2>/dev/null
  local pushed
  pushed=$(git rev-parse HEAD)
  git -c user.name=t -c user.email=t@t commit -q --allow-empty -m unpushed
  local unpushed
  unpushed=$(git rev-parse HEAD)
  echo dirty >file.txt

  export REMOTE_HEAVY_SSH="$tmp/fake-ssh" REMOTE_HEAVY_ORIGIN="$tmp/origin.git"
  export REMOTE_HEAVY_SELFTEST_SECRET="must-not-leak"
  local fails=0 rc
  check() { # 说明 条件…
    local what=$1
    shift
    if "$@"; then echo "  ok    $what"; else
      echo "  FAIL  $what"
      fails=$((fails + 1))
    fi
  }
  # 跑一次，退出码记进 rc，输出进 $out/<名字>
  rh() { # 名字 参数…
    local name=$1
    shift
    rc=0
    "$self" "$@" >"$out/$name" 2>&1 || rc=$?
  }

  echo "remote-heavy selftest（临时目录 ${tmp}）"

  rh unpushed --task t-unpushed "$unpushed" -- /bin/sh -c "touch '$out/ran-unpushed'"
  check "未推送提交被拒（退出码 2）" [ "$rc" -eq 2 ]
  check "未推送提交：提示先推送" grep -q '不在 origin' "$out/unpushed"
  check "未推送提交：命令没跑" [ ! -e "$out/ran-unpushed" ]
  check "对照：未推送的提交本机确实存在" git cat-file -e "$unpushed^{commit}"

  # 推过、远端裸仓库也抓到过，之后 origin 上的分支被删（或强推掉）：对象还留在远端裸仓库里，仍要拒绝。
  git -c user.name=t -c user.email=t@t commit -q --allow-empty -m gone
  local gone
  gone=$(git rev-parse HEAD)
  git push -q origin HEAD:refs/heads/gone 2>/dev/null
  rh gone-1 --task t-gone "$gone" -- /bin/sh -c 'exit 0'
  check "对照：分支还在时能跑" [ "$rc" -eq 0 ]
  git push -q origin :refs/heads/gone 2>/dev/null
  rh gone-2 --task t-gone "$gone" -- /bin/sh -c "touch '$out/ran-gone'"
  check "origin 上已不在任何分支的提交被拒" test "$rc" -eq 2 -a ! -e "$out/ran-gone"
  check "对照：远端裸仓库里还留着这个提交" git -C "$root/repo.git" cat-file -e "$gone^{commit}"
  git reset -q --hard "$unpushed"
  echo dirty >file.txt

  rh exit7 --task t-a "$pushed" -- /bin/sh -c 'git rev-parse HEAD; cat file.txt; env >"$TMPDIR/../env"; exit 7'
  check "退出码透传（7）" [ "$rc" -eq 7 ]
  check "输出流回本机且检出的是指定提交" grep -q "^$pushed\$" "$out/exit7"
  check "本机未提交改动没带过去" grep -q '^committed$' "$out/exit7"
  check "结束行给出日志路径与退出码" grep -q '结束：退出码 7.*日志 .*/tasks/t-a/logs/' "$out/exit7"
  check "远端留了日志且含命令输出" grep -q "^$pushed\$" "$root"/tasks/t-a/logs/*.log
  check "命令环境有 CARGO_INCREMENTAL=0" grep -q '^CARGO_INCREMENTAL=0$' "$root/tasks/t-a/env"
  check "命令环境用任务自己的 target" grep -q "^CARGO_TARGET_DIR=$root/tasks/t-a/target\$" "$root/tasks/t-a/env"
  check "命令 PATH 以 ~/.cargo/bin 开头" grep -q "^PATH=$home/.cargo/bin:" "$root/tasks/t-a/env"
  check "本机环境变量没进命令" sh -c "! grep -q REMOTE_HEAVY_SELFTEST_SECRET '$root/tasks/t-a/env'"
  check "对照：假 ssh 确实把它带到了远端" grep -q REMOTE_HEAVY_SELFTEST_SECRET "$out/ssh-env"

  rh exit0 --task t-a "$pushed" -- /bin/sh -c 'exit 0'
  check "退出码透传（0）" [ "$rc" -eq 0 ]

  # 锁：A 拿锁后一直等到测试放行；测试先看到 B 报「等锁」再放 A，所以不靠睡多久，机器再忙也不抖。
  # B 必须等 A 结束才开始。等待都有上限（60 秒），超了按失败记。
  local order="$out/order" release="$out/release-a"
  "$self" --task t-lock-a "$pushed" -- /bin/sh -c \
    "echo A-start >>'$order'; i=0; while [ ! -e '$release' ] && [ \$i -lt 600 ]; do sleep 0.1; i=\$((i+1)); done; echo A-end >>'$order'" \
    >"$out/lock-a" 2>&1 &
  local a_pid=$!
  wait_for() { # 文件 模式
    local i=0
    while ! grep -q "$2" "$1" 2>/dev/null; do
      [ "$i" -lt 600 ] || return 1
      sleep 0.1
      i=$((i + 1))
    done
  }
  check "锁：A 开始了" wait_for "$order" A-start
  "$self" --task t-lock-b "$pushed" -- /bin/sh -c "echo B-start >>'$order'" >"$out/lock-b" 2>&1 &
  local b_pid=$!
  check "锁：B 报了在等锁、点名持有者" wait_for "$out/lock-b" '等锁：task=t-lock-a'
  touch "$release"
  wait "$a_pid" || true
  wait "$b_pid" || true
  check "锁：B 在 A 结束后才开始" [ "$(tr '\n' ' ' <"$order")" = "A-start A-end B-start " ]

  touch "$root/PERF_WINDOW"
  rh perf --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-perf'"
  check "PERF_WINDOW 存在时拒绝（退出码 2）" [ "$rc" -eq 2 ]
  check "PERF_WINDOW：命令没跑" [ ! -e "$out/ran-perf" ]
  rm "$root/PERF_WINDOW"
  rh perf-off --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-perf'"
  check "对照：删掉 PERF_WINDOW 后能跑" test "$rc" -eq 0 -a -e "$out/ran-perf"

  rh disk --task t-a --min-free-gb 999999999 "$pushed" -- /bin/sh -c "touch '$out/ran-disk'"
  check "剩余空间低于阈值时拒绝（退出码 2）" test "$rc" -eq 2 -a ! -e "$out/ran-disk"

  # 超时：命令起一个后台 sleep 后挂住；超时后整组（含那个 sleep）都要没了，锁也要放掉。
  rh timeout --task t-a --timeout 2 "$pushed" -- /bin/sh -c "sleep 300 & echo \$! >'$out/child'; wait"
  check "超时退出码 124" [ "$rc" -eq 124 ]
  check "超时：进程组里的后台子进程也被收掉" sh -c "! kill -0 \$(cat '$out/child') 2>/dev/null"
  rh after-timeout --task t-a "$pushed" -- /bin/sh -c 'exit 0'
  check "超时后锁已释放（下一次不用等锁）" sh -c "[ $rc -eq 0 ] && ! grep -q 等锁 '$out/after-timeout'"

  rh badname --task '../x' "$pushed" -- /bin/sh -c 'exit 0'
  check "任务名带路径被拒" [ "$rc" -eq 2 ]

  rh clean clean t-a
  check "clean 成功" [ "$rc" -eq 0 ]
  check "clean：任务目录没了" [ ! -e "$root/tasks/t-a" ]
  check "clean：worktree 登记也清了" sh -c "! git -C '$root/repo.git' worktree list | grep -q tasks/t-a"
  check "对照：别的任务还在" [ -d "$root/tasks/t-lock-a/src" ]

  if [ "$fails" -eq 0 ]; then
    echo "remote-heavy selftest：通过"
    return 0
  fi
  echo "remote-heavy selftest：$fails 项失败（输出在 ${out}，已随临时目录删除；要留现场把 trap 去掉重跑）" >&2
  return 1
}

main() {
  case "${1:-}" in
    --selftest) selftest ;;
    clean) shift; cmd_clean "$@" ;;
    list) shift; cmd_list "$@" ;;
    run) shift; cmd_run "$@" ;;
    -h | --help) sed -n '2,32p' "$0" ;;
    '') die_usage "缺参数" ;;
    *) cmd_run "$@" ;;
  esac
}

main "$@"
