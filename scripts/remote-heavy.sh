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
# 只用 ssh 和 git：不开端口（ClearAllForwardings）、不转发 agent、不改系统设置与钥匙串。
# 信任边界（每一层都不靠「过滤」，而是只放进白名单）：
#   本机 → ssh：ssh 在 env -i 下启动，用 -F /dev/null 不读任何 ssh 配置；连接参数只从 `ssh -G mac120`
#     里挑 hostname、user、port、identityfile 等几项（ssh_args_for），所以配置里的 SendEnv / SetEnv 带不过去。
#   远端 git：rh_git 在 env -i 下跑，不读全局与系统 git 配置，关掉凭据助手、askpass 与终端提示；要鉴权就直接失败。
#   远端命令：env -i，只带 rh_command_env 列出的变量。
#   任务名：只许字母数字 . _ -、不许 ..，本机与远端共用 rh_is_task_name；动路径前再核真实路径落在 tasks/ 之下。
#
#   远端锚点：任何写操作（mkdir、开锁、抓取、建 worktree）之前，先核 ~/work/iwork-remote 是真目录、物理路径
#     与 ~/work/iWork（T11 检出）互不包含，根下的固定条目都不是符号链接；repo.git 必须是真目录、是裸仓库、
#     带本脚本建库时写的 iwork.remoteHeavy=1 标记。
#
# 可信覆盖项：REMOTE_HEAVY_SSH（替换 ssh 程序）、REMOTE_HEAVY_ORIGIN（远端抓取的地址）是给 --selftest 用的，
# 由调用者自己设、脚本不核。上面的隔离保证以它们都没设为前提：设了 REMOTE_HEAVY_SSH 就不经 ssh 那一层；
# REMOTE_HEAVY_ORIGIN 换成 ssh 地址时 git 会走自己的 ssh 传输，不受 rh_git 关掉的那几项约束。
set -euo pipefail

HOST="mac120"
ORIGIN_URL="${REMOTE_HEAVY_ORIGIN:-https://github.com/mrkizy/iWork.git}"
DEFAULT_TIMEOUT="2h"
DEFAULT_MIN_FREE_GB=50

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
  rh_validate_inputs
  rh_check_root
  if [ ! -d "$TASKS" ]; then mkdir "$TASKS" || rh_refuse "建不了 $TASKS"; fi
  case "$R_MODE" in
    run) rh_run ;;
    clean) rh_clean ;;
    list) rh_list ;;
    *) rh_refuse "未知模式 $R_MODE" ;;
  esac
}

# 任务名会拼进远端路径，是删除操作的目标，本机与远端都只用这一个判断。
# 字符集逐个列出而不用 A-Z 这类范围：范围的含义随 locale 变，列出来的不会。整串匹配，换行、斜杠、
# 多字节字符都不在集合里；首字符不许是 . _ -，整串不许含 ..。
rh_is_task_name() {
  local n=$1 alnum='ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'
  [ "${#n}" -ge 1 ] && [ "${#n}" -le 64 ] || return 1
  case "$n" in
    [!$alnum]* | *[!$alnum._-]* | *..*) return 1 ;;
  esac
  return 0
}

rh_is_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in *[!0123456789abcdef]*) return 1 ;; esac
  return 0
}

rh_is_uint() {
  case "$1" in '' | *[!0123456789]*) return 1 ;; esac
  [ "${#1}" -le 9 ]
}

# 远端入口先把收到的参数全部核一遍，之后的代码只信任它们的形状。
rh_validate_inputs() {
  case "$R_MODE" in run | clean | list) ;; *) rh_refuse "未知模式 '$R_MODE'" ;; esac
  if [ "$R_MODE" != list ]; then
    rh_is_task_name "$R_TASK" || rh_refuse "任务名不合法：$(printf '%q' "$R_TASK")"
  fi
  if [ "$R_MODE" = run ]; then
    rh_is_sha "$R_SHA" || rh_refuse "提交号不合法：$(printf '%q' "$R_SHA")"
    rh_is_uint "$R_TIMEOUT" && [ "$R_TIMEOUT" -gt 0 ] || rh_refuse "超时不合法：$(printf '%q' "$R_TIMEOUT")"
    rh_is_uint "$R_MIN_FREE_GB" || rh_refuse "空间阈值不合法：$(printf '%q' "$R_MIN_FREE_GB")"
    [ -n "$R_ORIGIN" ] || rh_refuse "origin 地址为空"
    [ "${#R_CMD[@]}" -gt 0 ] || rh_refuse "没有命令"
  fi
}

# a 就是 b，或在 b 之下。按文件系统身份（设备号 + inode，即 test -ef）比，不按路径拼写比：
# macOS 默认卷不区分大小写，iwork-remote 与 IWORK-REMOTE 拼写不同却是同一目录。
# 从 a 的物理路径一级级往上走到 /，每一级都和 b 比一次。a、b 都要存在；b 不存在时 -ef 恒为假。
rh_is_within() { # a b
  local p
  p=$(cd -P "$1" && pwd) || return 1
  while :; do
    if [ "$p" -ef "$2" ]; then return 0; fi
    [ "$p" != / ] || return 1
    p=$(dirname "$p")
  done
}

# 根目录这个锚点：在任何写操作之前核。ROOT 不存在时只在 ~/work 下建这一层，不建 ~/work。
# 和 T11 检出（~/work/iWork）的关系按文件系统身份判：ROOT 不能是它、不能在它之下、也不能是它的上级。
# 核过之后 ROOT 和派生路径都换成物理路径。
rh_check_root() {
  local parent="$HOME/work" t11="$HOME/work/iWork" root_real e
  # T11 是一个指向不存在之处的链接时，没法判它和 ROOT 的关系（ROOT 建出来可能正好是它指的地方），拒绝。
  if [ -L "$t11" ] && [ ! -e "$t11" ]; then rh_refuse "$t11 是指向不存在之处的链接，判断不了和 $ROOT 的关系"; fi
  if [ ! -e "$ROOT" ] && [ ! -L "$ROOT" ]; then
    [ -d "$parent" ] || rh_refuse "$parent 不存在"
    # 建之前：将建的 ROOT 的父目录不能是 T11 或在 T11 之下（~/work 本身可能是链接）
    if [ -e "$t11" ] && rh_is_within "$parent" "$t11"; then rh_refuse "$parent 是 T11 检出或在它之内，不在这里建 $ROOT"; fi
    mkdir "$ROOT" || rh_refuse "建不了 $ROOT"
  fi
  [ ! -L "$ROOT" ] || rh_refuse "$ROOT 是符号链接"
  [ -d "$ROOT" ] || rh_refuse "$ROOT 不是目录"
  root_real=$(cd -P "$ROOT" && pwd) || rh_refuse "解析不了 $ROOT"
  if [ -e "$t11" ] && { rh_is_within "$ROOT" "$t11" || rh_is_within "$t11" "$ROOT"; }; then
    rh_refuse "$ROOT 和 T11 检出 $t11 是同一目录或互相包含"
  fi
  for e in tasks repo.git heavy.lock heavy.holder repo.lock PERF_WINDOW; do
    [ ! -L "$root_real/$e" ] || rh_refuse "$root_real/$e 是符号链接"
  done
  ROOT=$root_real
  REPO="$ROOT/repo.git"
  TASKS="$ROOT/tasks"
  HEAVY_LOCK="$ROOT/heavy.lock"
  HOLDER="$ROOT/heavy.holder"
  REPO_LOCK="$ROOT/repo.lock"
  PERF_MARK="$ROOT/PERF_WINDOW"
}

# repo.git 这个锚点：真目录、裸仓库、git 认的 git-dir 就是它自己、带本脚本写的身份标记。
# 所有对它的 git 调用都用 --git-dir，不让 git 往上层目录找仓库。
rh_check_repo() {
  [ ! -L "$REPO" ] || rh_refuse "$REPO 是符号链接"
  [ -d "$REPO" ] || rh_refuse "$REPO 不是目录"
  [ "$(rh_git --git-dir="$REPO" rev-parse --is-bare-repository 2>/dev/null)" = true ] ||
    rh_refuse "$REPO 不是裸仓库"
  local gd
  gd=$(rh_git --git-dir="$REPO" rev-parse --absolute-git-dir 2>/dev/null) || rh_refuse "$REPO 读不出 git-dir"
  [ "$(cd -P "$gd" && pwd)" = "$(cd -P "$REPO" && pwd)" ] || rh_refuse "$REPO 的 git-dir 指到了 $gd"
  [ "$(rh_git --git-dir="$REPO" config --get iwork.remoteHeavy 2>/dev/null)" = 1 ] ||
    rh_refuse "$REPO 没有 iwork.remoteHeavy=1 标记，不是本脚本建的；确认是本脚本早先建的就补上标记（git --git-dir=$REPO config iwork.remoteHeavy 1），否则挪走它"
}

rh_ensure_repo() {
  if [ ! -e "$REPO" ] && [ ! -L "$REPO" ]; then
    rh_git init -q --bare "$REPO" && rh_git --git-dir="$REPO" config iwork.remoteHeavy 1 ||
      rh_refuse "建不了裸仓库 $REPO"
  fi
  rh_check_repo
}

# 任务目录：入口核过名字之后，动路径前再核一次（纵深防御）：名字本身、tasks/ 不是符号链接、
# 任务目录存在时解析后的物理路径正好是 <tasks 的物理路径>/<任务名>、固定子目录不是符号链接。
# 结果放在 TD。目录刚建出来后要再调一次，才核得到物理路径。
rh_task_dir() {
  local tasks_real root_real sub
  rh_is_task_name "$R_TASK" || rh_refuse "任务名不合法：$(printf '%q' "$R_TASK")"
  root_real=$(cd -P "$ROOT" && pwd) || rh_refuse "解析不了 $ROOT"
  [ ! -L "$TASKS" ] || rh_refuse "$TASKS 是符号链接"
  tasks_real=$(cd -P "$TASKS" && pwd) || rh_refuse "解析不了 $TASKS"
  [ "$tasks_real" = "$root_real/tasks" ] || rh_refuse "$TASKS 解析到了 ${tasks_real}，不在 $root_real 之下"
  TD="$tasks_real/$R_TASK"
  if [ -e "$TD" ]; then
    [ "$(cd -P "$TD" && pwd)" = "$TD" ] || rh_refuse "$TD 解析后不在 $tasks_real 之下"
    for sub in src target tmp logs; do
      [ ! -L "$TD/$sub" ] || rh_refuse "$TD/$sub 是符号链接"
    done
  fi
}

# 远端所有 git 调用都走这里：env -i 清掉 GIT_*、GIT_ASKPASS、SSH_ASKPASS 等一切环境；
# 不读全局与系统配置（凭据助手、core.askPass、url.insteadOf 都进不来）；命令行再显式关掉凭据助手与
# askpass，终端提示也关掉，需要鉴权时 git 直接失败而不是去问谁。
rh_git() {
  env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git -c credential.helper= -c core.askPass= "$@"
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
  rh_ensure_repo
  rh_git --git-dir="$REPO" config remote.origin.url "$R_ORIGIN"
  # 公开仓库不需要凭据；rh_git 让需要鉴权的抓取直接失败，绝不去读钥匙串或问 askpass。
  rh_git --git-dir="$REPO" fetch --quiet --prune origin \
    '+refs/heads/*:refs/remotes/origin/*' || rh_refuse "从 $R_ORIGIN 抓取失败"
  if ! rh_git --git-dir="$REPO" cat-file -e "$R_SHA^{commit}" 2>/dev/null ||
    [ -z "$(rh_git --git-dir="$REPO" for-each-ref --count=1 --contains "$R_SHA" refs/remotes/origin)" ]; then
    rh_refuse "提交 $R_SHA 不在 origin 的任何分支上：先 git push 再跑（本机未提交、未推送的改动不会带过去）"
  fi
}

rh_checkout() { # src
  local src=$1
  if [ -e "$src" ]; then
    # 已有的 src 必须是挂在我们裸仓库上的 worktree，才能对它 checkout --force 和 clean -ffdx。
    rh_is_our_worktree "$src" || rh_refuse "$src 不是 $REPO 的 worktree，不动它"
    rh_git -C "$src" checkout -q --detach --force "$R_SHA" &&
      rh_git -C "$src" clean -q -ffdx || rh_refuse "检出 $R_SHA 到 $src 失败"
  else
    rh_git --git-dir="$REPO" worktree prune
    rh_git --git-dir="$REPO" worktree add -q --detach "$src" "$R_SHA" || rh_refuse "建 worktree $src 失败"
  fi
  [ "$(rh_git -C "$src" rev-parse HEAD)" = "$R_SHA" ] || rh_refuse "$src 检出后 HEAD 不是 $R_SHA"
  [ -z "$(rh_git -C "$src" status --porcelain --ignored)" ] || rh_refuse "$src 检出后不干净"
}

rh_is_our_worktree() { # dir
  local common repo_real
  [ -f "$1/.git" ] || return 1
  repo_real=$(cd -P "$REPO" && pwd) || return 1
  common=$(rh_git -C "$1" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || return 1
  [ "$(cd -P "$common" && pwd)" = "$repo_real" ]
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
  rh_check_gate
  rh_lock_repo
  rh_fetch_and_verify
  rh_unlock_repo

  rh_lock_heavy
  printf 'task=%s sha=%s pid=%s since=%s cmd=%s\n' "$R_TASK" "$R_SHA" "$$" \
    "$(date '+%F %T')" "${R_CMD[*]}" >"$HOLDER"
  rh_check_gate

  rh_task_dir
  mkdir -p "$TD/target" "$TD/tmp" "$TD/logs" || rh_refuse "建不了 $TD 下的目录"
  rh_task_dir
  local td=$TD
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
  rh_task_dir
  if [ ! -e "$TD" ]; then
    rh_say "没有任务 ${R_TASK}，不用清"
    exit 0
  fi
  rh_lock_heavy
  printf 'task=%s pid=%s since=%s cmd=clean\n' "$R_TASK" "$$" "$(date '+%F %T')" >"$HOLDER"
  # 等锁期间目录可能被换掉，拿到锁后再核一次真实路径，之后才删。
  rh_task_dir
  local td=$TD size
  size=$(du -sh "$td" 2>/dev/null | awk '{print $1}')
  rh_lock_repo
  if [ -e "$REPO" ] || [ -L "$REPO" ]; then rh_check_repo; fi
  if rh_is_our_worktree "$td/src"; then rh_git --git-dir="$REPO" worktree remove --force "$td/src" || true; fi
  # rm -rf 不跟随符号链接；td 已核过是 tasks/ 下的真实目录。
  rm -rf "$td" || rh_refuse "删不掉 $td"
  if [ -d "$REPO" ]; then rh_git --git-dir="$REPO" worktree prune; fi
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
      "$(rh_git -C "$td/src" rev-parse --short=12 HEAD 2>/dev/null || echo '-')"
  done
}

# ---------------------------------------------------------------------------
# 本机部分
# ---------------------------------------------------------------------------

parse_seconds() { # 90 | 30m | 2h
  local v=$1 n unit
  n=${v%[smh]}
  unit=${v#"$n"}
  rh_is_uint "$n" || die_usage "时长不合法：$(printf '%q' "$v")"
  case "$unit" in
    '' | s) echo "$n" ;;
    m) echo $((n * 60)) ;;
    h) echo $((n * 3600)) ;;
  esac
}

# ssh -G 打印值时去掉了引号，拼回 -o 后 ssh 会按配置语法再解析一遍：带空白、引号、反斜杠的值会被拆开或报错。
# 不去还原引号，而是只接受不含这些字符的值，含了就点名拒绝。
ssh_plain_value() { # 键 值
  case "$2" in
    *[[:space:]\"\'\\]*)
      echo "remote-heavy: ssh 配置里的 $1 含空白、引号或反斜杠，本脚本不接受：$(printf '%q' "$2")（改用不含这些字符的路径）" >&2
      return 1 ;;
  esac
}

# 由 `ssh -G <别名>` 解析出的有效配置里只挑连接要的几项，拼成 SSH_ARGS；之后 ssh 用 -F /dev/null 运行，
# 不再读用户或系统的 ssh 配置，配置里的 SendEnv、SetEnv、LocalForward、ProxyCommand 等都进不来。
# UserKnownHostsFile 不转写：它是空格分隔的多个路径，ssh -G 打印时丢了引号，「一个带空格的路径」和「两个路径」
# 打出来一模一样，猜不出边界。ssh 调用不传它，用 ssh 在 -F /dev/null 下的默认值；用户配置对这台主机改过它
# （有效值和默认值不同）就拒绝，宁可连不上也不猜。
# 第二个参数只给自测用：指定解析时读的配置文件。
ssh_args_for() { # 主机别名 [配置文件]
  local alias=$1 cfg=${2:-} conf line key val hostname="" user="" port="" known_hosts="" default_known_hosts=""
  if [ -n "$cfg" ]; then
    conf=$(ssh -G -F "$cfg" "$alias" 2>/dev/null) || { echo "remote-heavy: ssh -G $alias 失败" >&2; return 2; }
  else
    conf=$(ssh -G "$alias" 2>/dev/null) || { echo "remote-heavy: ssh -G $alias 失败" >&2; return 2; }
  fi
  SSH_ARGS=(-F /dev/null)
  while IFS= read -r line; do
    key=${line%% *}
    val=${line#* }
    case "$key" in
      hostname) hostname=$val ;;
      user) user=$val ;;
      port) port=$val ;;
      identityfile) ssh_plain_value "$key" "$val" || return 2; SSH_ARGS+=(-o "IdentityFile=$val") ;;
      certificatefile) ssh_plain_value "$key" "$val" || return 2; SSH_ARGS+=(-o "CertificateFile=$val") ;;
      identitiesonly) SSH_ARGS+=(-o "IdentitiesOnly=$val") ;;
      hostkeyalias) ssh_plain_value "$key" "$val" || return 2; SSH_ARGS+=(-o "HostKeyAlias=$val") ;;
      userknownhostsfile) known_hosts=$val ;;
      proxycommand | proxyjump)
        [ "$val" = none ] || { echo "remote-heavy: $alias 配了 ${key}，本脚本不经代理连接" >&2; return 2; } ;;
    esac
  done <<<"$conf"
  case "$hostname" in '' | *[!A-Za-z0-9.:-]*) echo "remote-heavy: hostname 不合法：$(printf '%q' "$hostname")" >&2; return 2 ;; esac
  case "$user" in '' | *[!A-Za-z0-9._-]*) echo "remote-heavy: user 不合法：$(printf '%q' "$user")" >&2; return 2 ;; esac
  rh_is_uint "$port" || { echo "remote-heavy: port 不合法：$(printf '%q' "$port")" >&2; return 2; }
  default_known_hosts=$(ssh -F /dev/null -G "$hostname" 2>/dev/null | sed -n 's/^userknownhostsfile //p')
  if [ -z "$default_known_hosts" ] || [ "$known_hosts" != "$default_known_hosts" ]; then
    echo "remote-heavy: ssh 配置给 $alias 改了 UserKnownHostsFile（$(printf '%q' "$known_hosts")，默认是 $(printf '%q' "$default_known_hosts")）；本脚本只用默认的 known_hosts，请去掉这项配置或把主机密钥放进默认文件" >&2
    return 2
  fi
  SSH_ARGS+=(-o "HostName=$hostname" -o "User=$user" -o "Port=$port"
    -o BatchMode=yes -o StrictHostKeyChecking=yes -o ForwardAgent=no -o ForwardX11=no
    -o ClearAllForwardings=yes -o PermitLocalCommand=no -o RequestTTY=no
    -o ServerAliveInterval=30 -o ServerAliveCountMax=4)
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
    ssh_args_for "$HOST" || return 2
    ssh_cmd=(ssh "${SSH_ARGS[@]}")
  fi
  local program
  program=$(
    printf 'R_MODE=%q\nR_TASK=%q\nR_SHA=%q\nR_TIMEOUT=%q\nR_MIN_FREE_GB=%q\nR_ORIGIN=%q\n' \
      "$mode" "$task" "$sha" "$timeout" "$min_free" "$ORIGIN_URL"
    printf 'R_CMD=('
    if [ "$#" -gt 0 ]; then printf ' %q' "$@"; fi
    printf ' )\n'
    declare -f $(compgen -A function rh_)
    # 整段先读完再执行：rh_main 里的命令都不读 stdin（</dev/null），脚本本身就是 stdin。
    printf 'rh_main\n'
  )
  # ssh 本身也在 env -i 下跑：本机的环境变量一个都不经它出去。SSH_AUTH_SOCK 只给本机 ssh 找钥匙用
  # （ForwardAgent=no，不转发到远端）。
  local rc=0
  printf '%s\n' "$program" |
    env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin ${SSH_AUTH_SOCK:+SSH_AUTH_SOCK="$SSH_AUTH_SOCK"} \
      "${ssh_cmd[@]}" "$HOST" /bin/bash -s || rc=$?
  if [ "$rc" -eq 255 ]; then echo "remote-heavy: ssh 失败或连接断开（退出码 255）" >&2; fi
  return "$rc"
}

need_task_name() {
  rh_is_task_name "$1" ||
    die_usage "任务名不合法：$(printf '%q' "$1")（只许字母数字 . _ -，首字符是字母数字，不含 ..，最长 64）"
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
  rh_is_uint "$min_free" || die_usage "--min-free-gb 要是整数：$(printf '%q' "$min_free")"

  # 只取提交号：远端照提交号检出，本机工作区里的东西一概不带。是否已推送由远端对着 origin 判定。
  local sha
  sha=$(git rev-parse --verify --quiet "${ref}^{commit}") ||
    sha=$(git rev-parse --verify --quiet "origin/${ref}^{commit}") ||
    die_usage "本机解析不了 $(printf '%q' "$ref")"
  if [ -z "$task" ]; then
    [ "$ref" != "HEAD" ] || die_usage "用 HEAD 时请给 --task"
    task=${ref//\//-}
  fi
  need_task_name "$task"
  if ! git diff-index --quiet HEAD -- 2>/dev/null; then
    echo "remote-heavy: 提示：本机有未提交改动，不会带到远端；远端只跑 $sha" >&2
  fi
  send_remote run "$task" "$sha" "$seconds" "$min_free" "$@"
}

cmd_clean() {
  [ "$#" -eq 1 ] || die_usage "clean 要一个任务名"
  need_task_name "$1"
  send_remote clean "$1" "" 0 0
}

cmd_list() {
  [ "$#" -eq 0 ] || die_usage "list 不带参数"
  send_remote list "" "" 0 0
}

# ---------------------------------------------------------------------------
# 自测：假 ssh 在本机起 bash -s，HOME 指到临时目录，origin 是临时裸仓库。
# 整个自测在 env -i 下重新启动，临时目录里只会出现自测自己造的值（canary），不会有调用者的环境。
# ---------------------------------------------------------------------------

selftest() {
  local self
  self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  if [ "${REMOTE_HEAVY_SELFTEST_CLEAN_ENV:-}" != 1 ]; then
    exec env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="${TMPDIR:-/tmp}" \
      REMOTE_HEAVY_SELFTEST_CLEAN_ENV=1 "$BASH" "$self" --selftest
  fi
  # tmp 不用 local：EXIT trap 在函数返回后才跑，要还能读到它。
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/remote-heavy-selftest.XXXXXX")"
  # 自测里起的后台运行（锁、超时用例）在出错退出时也要收掉。
  trap 'jobs -p | xargs kill 2>/dev/null; rm -rf "$tmp"' EXIT
  export HOME="$tmp/localhome" GIT_CONFIG_NOSYSTEM=1
  local home="$tmp/home" root="$tmp/home/work/iwork-remote" out="$tmp/out"
  mkdir -p "$HOME" "$home" "$out" "$tmp/fakedf"
  # 远端 ~/work/iWork 的替身：T11 的检出（一个带 origin 的 git 仓库）。任何用例都不许动它。
  mkdir -p "$home/work/iWork"
  git init -q "$home/work/iWork"
  git -C "$home/work/iWork" config remote.origin.url keep-url
  echo keep >"$home/work/iWork/sentinel"

  # 假 ssh：只记本机 canary 有没有漏到它这里（不落盘任何别的环境）；远端会话里放一个 REMOTE_SESSION_CANARY，
  # 模拟远端 sshd 接受了某个变量，用来证明远端命令的环境仍只来自白名单。$tmp/lowdisk 存在时换上假 df。
  cat >"$tmp/fake-ssh" <<FAKE
#!/bin/bash
if [ -n "\${REMOTE_HEAVY_SELFTEST_CANARY:-}" ]; then echo leaked >"$out/ssh-saw-canary"; fi
prefix=""
if [ -e "$tmp/lowdisk" ]; then prefix="$tmp/fakedf:"; fi
exec env -i HOME="$home" PATH="\${prefix}/usr/bin:/bin:/usr/sbin:/sbin" REMOTE_SESSION_CANARY=remote-canary /bin/bash -s
FAKE
  chmod +x "$tmp/fake-ssh"
  cat >"$tmp/fakedf/df" <<'FAKE'
#!/bin/sh
echo "Filesystem 1024-blocks Used Available Capacity Mounted on"
echo "/dev/fake 1000000 999000 1000 99% /"
FAKE
  chmod +x "$tmp/fakedf/df"

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
  ORIGIN_URL="$REMOTE_HEAVY_ORIGIN"
  export REMOTE_HEAVY_SELFTEST_CANARY="canary-$$"
  local fails=0 total=0 rc
  # check 总是返回 0（脚本开着 set -e），结果记在 last_ok。
  local last_ok
  check() { # 说明 命令…
    local what=$1
    shift
    total=$((total + 1))
    if "$@"; then
      echo "  ok    $what"
      last_ok=1
    else
      echo "  FAIL  $what"
      fails=$((fails + 1))
      last_ok=0
    fi
  }
  # 前置条件：不成立时后面依赖它的用例没有意义，立刻停。
  need() {
    check "$@"
    if [ "$last_ok" -eq 0 ]; then
      echo "remote-heavy selftest：前置条件不成立，停止（已 ${fails}/${total} 项失败）" >&2
      exit 1
    fi
  }
  # 跑一次，退出码记进 rc，输出进 $out/<名字>
  rh() { # 名字 参数…
    local name=$1
    shift
    rc=0
    "$self" "$@" >"$out/$name" 2>&1 || rc=$?
  }
  # 正向用例：空间门禁放到 0，不受宿主剩余空间影响（门禁单独用假 df 测）。
  rr() { # 名字 参数…
    local name=$1
    shift
    rh "$name" --min-free-gb 0 "$@"
  }
  sentinel_intact() {
    [ "$(cat "$home/work/iWork/sentinel" 2>/dev/null)" = keep ] &&
      [ "$(ls -A "$home/work/iWork")" = "$(printf '.git\nsentinel')" ] &&
      [ "$(git -C "$home/work/iWork" config --get remote.origin.url)" = keep-url ]
  }

  echo "remote-heavy selftest（临时目录 ${tmp}）"

  # --- 任务名：一个判断，本机与远端共用 ---
  local good bad
  for good in main t10a-b a.b_c-1 "$(printf 'a%.0s' $(seq 64))"; do
    check "合法任务名：$(printf '%q' "$good")" rh_is_task_name "$good"
  done
  not() { ! "$@"; }
  for bad in '' .x -x _x a/b ../x a..b .. $'ok\n/../../../iWork' $'a\nb' $'a\tb' ' a' 'é' "$(printf 'a%.0s' $(seq 65))"; do
    check "非法任务名被拒：$(printf '%q' "$bad")" not rh_is_task_name "$bad"
  done

  # 本机入口：多行名字、.. 、斜杠，run 和 clean 都在本机就被拒
  for bad in $'ok\n/../../../iWork' '../iWork' 'a/b' '..'; do
    rr "badname-run" --task "$bad" "$pushed" -- /bin/sh -c "touch '$out/ran-badname'"
    check "本机 run 拒绝任务名 $(printf '%q' "$bad")" test "$rc" -eq 2 -a ! -e "$out/ran-badname"
    rh "badname-clean" clean "$bad"
    check "本机 clean 拒绝任务名 $(printf '%q' "$bad")" [ "$rc" -eq 2 ]
  done
  # 远端入口：绕过本机检查直接把坏名字送过去，远端自己也要拒
  local evil=$'ok\n/../../../iWork'
  rc=0
  send_remote run "$evil" "$pushed" 60 0 /bin/sh -c "touch '$out/ran-badname'" >"$out/remote-badname-run" 2>&1 || rc=$?
  check "远端 run 自己拒绝多行任务名" test "$rc" -eq 2 -a ! -e "$out/ran-badname"
  rc=0
  send_remote clean "$evil" "" 0 0 >"$out/remote-badname-clean" 2>&1 || rc=$?
  check "远端 clean 自己拒绝多行任务名" [ "$rc" -eq 2 ]
  check "边界外哨兵（远端 ~/work/iWork）没被动" sentinel_intact

  # 纵深防御：任务目录或 src 是指向边界外的符号链接时，run 与 clean 都拒绝
  mkdir -p "$root/tasks" "$root/tasks/t-sym"
  ln -s "$home/work/iWork" "$root/tasks/evil"
  ln -s "$home/work/iWork" "$root/tasks/t-sym/src"
  rr sym-run --task evil "$pushed" -- /bin/sh -c "touch '$out/ran-sym'"
  check "任务目录是符号链接：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-sym"
  rh sym-clean clean evil
  check "任务目录是符号链接：clean 拒绝" [ "$rc" -eq 2 ]
  rr sym-src --task t-sym "$pushed" -- /bin/sh -c "touch '$out/ran-sym'"
  check "src 是符号链接：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-sym"
  check "符号链接用例后哨兵没被动" sentinel_intact
  rm "$root/tasks/evil" "$root/tasks/t-sym/src"
  # logs 指向边界外：日志不许写到外面去
  ln -s "$home/work/iWork" "$root/tasks/t-sym/logs"
  rr sym-logs --task t-sym "$pushed" -- /bin/sh -c "touch '$out/ran-sym'"
  check "logs 是符号链接：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-sym"
  rm "$root/tasks/t-sym/logs"
  # src 是普通目录但不是我们裸仓库的 worktree：不许对它 checkout --force / clean -ffdx
  # 用 origin 的克隆冒充：它有这个提交，少了 worktree 核对就会被 checkout --force、clean -ffdx 掉 precious
  git clone -q "$tmp/origin.git" "$root/tasks/t-sym/src" 2>/dev/null
  echo keep >"$root/tasks/t-sym/src/precious"
  rr notwt --task t-sym "$pushed" -- /bin/sh -c "touch '$out/ran-sym'"
  check "src 不是本仓库的 worktree：run 拒绝且没清它" test "$rc" -eq 2 -a ! -e "$out/ran-sym" -a -e "$root/tasks/t-sym/src/precious"
  check "符号链接与非 worktree 用例后哨兵没被动" sentinel_intact
  rm -rf "$root/tasks/t-sym"

  # --- 只认 origin ---
  rr unpushed --task t-unpushed "$unpushed" -- /bin/sh -c "touch '$out/ran-unpushed'"
  check "未推送提交被拒（退出码 2）" [ "$rc" -eq 2 ]
  check "未推送提交：提示先推送" grep -q '不在 origin' "$out/unpushed"
  check "未推送提交：命令没跑" [ ! -e "$out/ran-unpushed" ]
  check "对照：未推送的提交本机确实存在" git cat-file -e "$unpushed^{commit}"

  # 推过、远端裸仓库也抓到过，之后 origin 上的分支被删（或强推掉）：对象还留在远端裸仓库里，仍要拒绝。
  git -c user.name=t -c user.email=t@t commit -q --allow-empty -m gone
  local gone
  gone=$(git rev-parse HEAD)
  git push -q origin HEAD:refs/heads/gone 2>/dev/null
  rr gone-1 --task t-gone "$gone" -- /bin/sh -c 'exit 0'
  need "对照：分支还在时能跑" [ "$rc" -eq 0 ]
  git push -q origin :refs/heads/gone 2>/dev/null
  rr gone-2 --task t-gone "$gone" -- /bin/sh -c "touch '$out/ran-gone'"
  check "origin 上已不在任何分支的提交被拒" test "$rc" -eq 2 -a ! -e "$out/ran-gone"
  check "对照：远端裸仓库里还留着这个提交" git -C "$root/repo.git" cat-file -e "$gone^{commit}"
  git reset -q --hard "$unpushed"
  echo dirty >file.txt

  # --- 正常运行：退出码、输出、日志、环境 ---
  rr exit7 --task t-a "$pushed" -- /bin/sh -c 'git rev-parse HEAD; cat file.txt; env >"$TMPDIR/../env"; exit 7'
  need "退出码透传（7）" [ "$rc" -eq 7 ]
  check "输出流回本机且检出的是指定提交" grep -q "^$pushed\$" "$out/exit7"
  check "本机未提交改动没带过去" grep -q '^committed$' "$out/exit7"
  check "结束行给出日志路径与退出码" grep -q '结束：退出码 7.*日志 .*/tasks/t-a/logs/' "$out/exit7"
  check "远端留了日志且含命令输出" grep -q "^$pushed\$" "$root"/tasks/t-a/logs/*.log
  check "命令环境有 CARGO_INCREMENTAL=0" grep -q '^CARGO_INCREMENTAL=0$' "$root/tasks/t-a/env"
  # 远端按物理路径定任务目录（/var → /private/var 这类链接会被解开）
  local root_real
  root_real=$(cd -P "$root" && pwd)
  check "命令环境用任务自己的 target" grep -q "^CARGO_TARGET_DIR=$root_real/tasks/t-a/target\$" "$root/tasks/t-a/env"
  check "命令 PATH 以 ~/.cargo/bin 开头" grep -q "^PATH=$home/.cargo/bin:" "$root/tasks/t-a/env"
  check "远端会话里的变量没进命令" not grep -q REMOTE_SESSION_CANARY "$root/tasks/t-a/env"
  check "本机变量没进命令" not grep -q REMOTE_HEAVY_SELFTEST_CANARY "$root/tasks/t-a/env"
  check "本机变量没经过 ssh（ssh 在 env -i 下启动）" [ ! -e "$out/ssh-saw-canary" ]
  check "对照：本机环境里确实有这个变量" [ -n "$REMOTE_HEAVY_SELFTEST_CANARY" ]

  rr exit0 --task t-a "$pushed" -- /bin/sh -c 'exit 0'
  check "退出码透传（0）" [ "$rc" -eq 0 ]

  # --- 锁 ---
  # A 拿锁后一直等到测试放行；测试先看到 B 报「等锁」再放 A，所以不靠睡多久，机器再忙也不抖。
  # B 必须等 A 结束才开始。等待都有上限（60 秒），超了按失败记。
  local order="$out/order" release="$out/release-a"
  "$self" --min-free-gb 0 --task t-lock-a "$pushed" -- /bin/sh -c \
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
  need "锁：A 开始了" wait_for "$order" A-start
  "$self" --min-free-gb 0 --task t-lock-b "$pushed" -- /bin/sh -c "echo B-start >>'$order'" >"$out/lock-b" 2>&1 &
  local b_pid=$!
  check "锁：B 报了在等锁、点名持有者" wait_for "$out/lock-b" '等锁：task=t-lock-a'
  touch "$release"
  wait "$a_pid" || true
  wait "$b_pid" || true
  check "锁：B 在 A 结束后才开始" [ "$(tr '\n' ' ' <"$order")" = "A-start A-end B-start " ]

  # --- PERF_WINDOW ---
  touch "$root/PERF_WINDOW"
  rr perf --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-perf'"
  check "PERF_WINDOW 存在时拒绝（退出码 2）" test "$rc" -eq 2 -a ! -e "$out/ran-perf"
  rm "$root/PERF_WINDOW"
  rr perf-off --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-perf'"
  check "对照：删掉 PERF_WINDOW 后能跑" test "$rc" -eq 0 -a -e "$out/ran-perf"

  # --- 空间门禁：用假 df（剩 1000 KB），不看宿主真实空间 ---
  touch "$tmp/lowdisk"
  rh disk --task t-a --min-free-gb 1 "$pushed" -- /bin/sh -c "touch '$out/ran-disk'"
  check "剩余空间低于阈值时拒绝（退出码 2）" test "$rc" -eq 2 -a ! -e "$out/ran-disk"
  check "拒绝理由来自假 df（剩 0G）" grep -q '剩余空间 0G，低于阈值 1G' "$out/disk"
  rh disk-ok --task t-a --min-free-gb 0 "$pushed" -- /bin/sh -c "touch '$out/ran-disk'"
  check "对照：阈值 0 时同一个假 df 下能跑" test "$rc" -eq 0 -a -e "$out/ran-disk"
  rm "$tmp/lowdisk"

  # --- 超时：命令起一个后台 sleep 后挂住；超时后整组（含那个 sleep）都要没了，锁也要放掉 ---
  rr timeout --task t-a --timeout 2 "$pushed" -- /bin/sh -c "sleep 300 & echo \$! >'$out/child'; wait"
  check "超时退出码 124" [ "$rc" -eq 124 ]
  check "超时：进程组里的后台子进程也被收掉" sh -c "! kill -0 \$(cat '$out/child') 2>/dev/null"
  rr after-timeout --task t-a "$pushed" -- /bin/sh -c 'exit 0'
  check "超时后锁已释放（下一次不用等锁）" sh -c "[ $rc -eq 0 ] && ! grep -q 等锁 '$out/after-timeout'"

  # --- 远端锚点：根目录与 repo.git ---
  check "repo.git 带本脚本的身份标记" [ "$(git --git-dir="$root/repo.git" config --get iwork.remoteHeavy)" = 1 ]
  # 根目录链到 T11：run、clean、list 都在动任何东西之前拒绝
  mv "$root" "$root.aside"
  ln -s "$home/work/iWork" "$root"
  rr root-run --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-anchor'"
  check "根目录链到 T11：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-anchor"
  rh root-clean clean t-a
  check "根目录链到 T11：clean 拒绝" [ "$rc" -eq 2 ]
  rh root-list list
  check "根目录链到 T11：list 拒绝" [ "$rc" -eq 2 ]
  check "根目录链到 T11：T11 里没建出 repo.git、tasks、锁" sentinel_intact
  rm "$root"
  mv "$root.aside" "$root"
  # repo.git 链到外部仓库（T11 的 .git）：run 不许改它的 origin，clean 不许拿它删 worktree
  mv "$root/repo.git" "$root/repo.git.aside"
  ln -s "$home/work/iWork/.git" "$root/repo.git"
  rr repo-link-run --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-anchor'"
  check "repo.git 链到外部仓库：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-anchor"
  rh repo-link-clean clean t-a
  check "repo.git 链到外部仓库：clean 拒绝" [ "$rc" -eq 2 ]
  check "repo.git 链到外部仓库：T11 的 origin 没被改" sentinel_intact
  rm "$root/repo.git"
  # repo.git 是真的裸仓库但不是本脚本建的（没有标记）
  git init -q --bare "$root/repo.git"
  rr repo-nomark --task t-nomark "$pushed" -- /bin/sh -c "touch '$out/ran-anchor'"
  check "repo.git 没有身份标记：run 拒绝并说明" test "$rc" -eq 2 -a ! -e "$out/ran-anchor"
  check "repo.git 没有身份标记：提示里点名标记" grep -q 'iwork.remoteHeavy' "$out/repo-nomark"
  rm -rf "$root/repo.git"
  mv "$root/repo.git.aside" "$root/repo.git"
  # 根下的固定文件链到外面：holder 会被截断写，不许
  ln -s "$home/work/iWork/sentinel" "$root/heavy.holder"
  rr holder-link --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-anchor'"
  check "heavy.holder 是符号链接：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-anchor"
  check "heavy.holder 链接用例后 T11 哨兵没被截断" sentinel_intact
  rm "$root/heavy.holder"
  rr anchors-ok --task t-a "$pushed" -- /bin/sh -c 'exit 0'
  check "对照：锚点恢复后能跑" [ "$rc" -eq 0 ]

  # 大小写别名：macOS 默认卷不区分大小写，T11 链到 .../IWORK-REMOTE 时和 ROOT 是同一目录，拼写却不同。
  # 卷区分大小写时这两格造不出来，跳过（不计数）。
  mkdir "$tmp/caseprobe"
  if [ -d "$tmp/CASEPROBE" ]; then
    mv "$home/work/iWork" "$home/work/iWork.aside"
    # 已有 ROOT：T11 是它的大小写别名，clean 不许删，run、list 也拒绝
    ln -s "$home/work/IWORK-REMOTE" "$home/work/iWork"
    echo keep >"$root/t11-sentinel"
    need "对照：别名与 ROOT 是同一目录（test -ef）" [ "$home/work/iWork" -ef "$root" ]
    need "对照：两者的物理路径拼写不同" [ "$(cd -P "$home/work/iWork" && pwd)" != "$(cd -P "$root" && pwd)" ]
    rh case-clean clean t-a
    check "大小写别名、已有 ROOT：clean 拒绝" [ "$rc" -eq 2 ]
    check "大小写别名、已有 ROOT：任务目录与 T11 哨兵都还在" test -d "$root/tasks/t-a" -a -e "$root/t11-sentinel"
    rr case-run --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-case'"
    check "大小写别名、已有 ROOT：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-case"
    rh case-list list
    check "大小写别名、已有 ROOT：list 拒绝" [ "$rc" -eq 2 ]
    rm "$root/t11-sentinel"
    # 互相包含（同样借大小写别名，拼写比较看不出来）：T11 在 ROOT 之内、ROOT 在 T11 之内
    rm "$home/work/iWork"
    ln -s "$home/work/IWORK-REMOTE/tasks" "$home/work/iWork"
    rr case-inside --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-case'"
    check "大小写别名、T11 在 ROOT 之内：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-case"
    rm "$home/work/iWork"
    ln -s "$home/WORK" "$home/work/iWork"
    rh case-above clean t-a
    check "大小写别名、ROOT 在 T11 之内：clean 拒绝且任务目录还在" test "$rc" -eq 2 -a -d "$root/tasks/t-a"
    rm "$home/work/iWork"
    ln -s "$home/work/IWORK-REMOTE" "$home/work/iWork"
    # ROOT 未建：T11 链到将来 ROOT 的大小写别名（此刻指向不存在之处），list 不许建出 ROOT 和 tasks
    mv "$root" "$root.aside"
    rh case-list-new list
    check "大小写别名、ROOT 未建：list 拒绝" [ "$rc" -eq 2 ]
    check "大小写别名、ROOT 未建：没建出 ROOT（也就没在 T11 里建 tasks）" [ ! -e "$root" ]
    rr case-run-new --task t-a "$pushed" -- /bin/sh -c "touch '$out/ran-case'"
    check "大小写别名、ROOT 未建：run 拒绝" test "$rc" -eq 2 -a ! -e "$out/ran-case" -a ! -e "$root"
    mv "$root.aside" "$root"
    rm "$home/work/iWork"
    mv "$home/work/iWork.aside" "$home/work/iWork"
    check "大小写别名用例后 T11 哨兵完好" sentinel_intact
  else
    echo "  skip  大小写别名两格（这个卷区分大小写）"
  fi

  # --- clean ---
  rh clean clean t-a
  check "clean 成功" [ "$rc" -eq 0 ]
  check "clean：任务目录没了" [ ! -e "$root/tasks/t-a" ]
  check "clean：worktree 登记也清了" not sh -c "git -C '$root/repo.git' worktree list | grep -q tasks/t-a"
  check "对照：别的任务还在" [ -d "$root/tasks/t-lock-a/src" ]
  check "全部用例跑完哨兵仍在" sentinel_intact

  # --- ssh 边界：配置里的 SendEnv / SetEnv / 转发进不了真正发出去的参数 ---
  cat >"$tmp/ssh_config" <<'CFG'
Host rhtest
  HostName 192.0.2.10
  User someone
  Port 2222
  IdentityFile ~/.ssh/id_rhtest
  IdentitiesOnly yes
  SendEnv RH_SECRET_TOKEN LC_*
  SetEnv RH_SET=1
  LocalForward 9999 127.0.0.1:9999
  ForwardAgent yes
CFG
  need "对照：直接读这份配置时 SendEnv、SetEnv 都生效" sh -c \
    "ssh -G -F '$tmp/ssh_config' rhtest 2>/dev/null | grep -q '^sendenv RH_SECRET_TOKEN' && ssh -G -F '$tmp/ssh_config' rhtest 2>/dev/null | grep -q '^setenv RH_SET=1'"
  need "ssh_args_for 解析成功" ssh_args_for rhtest "$tmp/ssh_config"
  local eff
  eff=$(ssh -G "${SSH_ARGS[@]}" rhtest 2>/dev/null)
  check "有效配置里没有 SendEnv" not grep -q '^sendenv ' <<<"$eff"
  check "有效配置里没有 SetEnv" not grep -q '^setenv ' <<<"$eff"
  check "有效配置里没有 LocalForward" not grep -q '^localforward ' <<<"$eff"
  check "ForwardAgent 关着" grep -q '^forwardagent no$' <<<"$eff"
  check "连接参数保留：hostname/user/port" sh -c 'printf "%s\n" "$1" | grep -q "^hostname 192.0.2.10$" && printf "%s\n" "$1" | grep -q "^user someone$" && printf "%s\n" "$1" | grep -q "^port 2222$"' _ "$eff"
  check "连接参数保留：identityfile" grep -q '^identityfile ~/.ssh/id_rhtest$' <<<"$eff"
  # 带空白、引号的路径：ssh -G 打印时丢了引号，拼回去会被拆开，要点名拒绝
  printf 'Host rhq\n  HostName 192.0.2.10\n  User u\n  IdentityFile "/tmp/my key"\n' >"$tmp/ssh_config_space"
  need "对照：带空格的 IdentityFile 在原配置里合法" sh -c "ssh -G -F '$tmp/ssh_config_space' rhq 2>/dev/null | grep -q '^identityfile /tmp/my key\$'"
  check "IdentityFile 含空格：拒绝" not ssh_args_for rhq "$tmp/ssh_config_space" 2>/dev/null
  printf 'Host rhq\n  HostName 192.0.2.10\n  User u\n  CertificateFile "/tmp/a\x27b"\n' >"$tmp/ssh_config_quote"
  check "CertificateFile 含引号：拒绝" not ssh_args_for rhq "$tmp/ssh_config_quote" 2>/dev/null
  # known_hosts：不从配置转写，只用 ssh 的默认值；配置改过它就拒绝（两种写法 ssh -G 打出来一模一样，没法猜）
  printf 'Host rhq\n  HostName 192.0.2.10\n  User u\n  UserKnownHostsFile "/tmp/kh /x/known_hosts"\n' >"$tmp/ssh_config_kh1"
  printf 'Host rhq\n  HostName 192.0.2.10\n  User u\n  UserKnownHostsFile /tmp/kh /x/known_hosts\n' >"$tmp/ssh_config_kh2"
  need "对照：一个带空格的路径和两个路径，ssh -G 打出来一样" sh -c \
    "[ \"\$(ssh -G -F '$tmp/ssh_config_kh1' rhq 2>/dev/null | grep ^userknownhostsfile)\" = \"\$(ssh -G -F '$tmp/ssh_config_kh2' rhq 2>/dev/null | grep ^userknownhostsfile)\" ]"
  check "UserKnownHostsFile 一个带空格的路径：拒绝" not ssh_args_for rhq "$tmp/ssh_config_kh1" 2>/dev/null
  check "UserKnownHostsFile 两个路径：拒绝" not ssh_args_for rhq "$tmp/ssh_config_kh2" 2>/dev/null
  printf 'Host rhq\n  HostName 192.0.2.10\n  User u\n' >"$tmp/ssh_config_kh0"
  need "UserKnownHostsFile 用默认值：接受" ssh_args_for rhq "$tmp/ssh_config_kh0"
  check "发出去的参数里没有 UserKnownHostsFile" not sh -c 'printf "%s\n" "$@" | grep -q UserKnownHostsFile' _ "${SSH_ARGS[@]}"
  check "有效 known_hosts 就是 -F /dev/null 下的默认值" sh -c '[ "$1" = "$2" ] && [ -n "$1" ]' _ \
    "$(ssh -G "${SSH_ARGS[@]}" rhq 2>/dev/null | grep ^userknownhostsfile)" \
    "$(ssh -F /dev/null -G 192.0.2.10 2>/dev/null | grep ^userknownhostsfile)"
  printf 'Host rhproxy\n  HostName 192.0.2.11\n  ProxyCommand nc %%h %%p\n' >"$tmp/ssh_config_proxy"
  check "配了 ProxyCommand 时拒绝" not ssh_args_for rhproxy "$tmp/ssh_config_proxy" 2>/dev/null

  # --- 远端 git：askpass、凭据助手、全局配置都进不来 ---
  local gh="$tmp/githome" called="$out/askpass-called"
  mkdir -p "$gh"
  printf '#!/bin/sh\necho x >>"%s"\necho x\n' "$called" >"$tmp/askpass"
  chmod +x "$tmp/askpass"
  printf '[credential]\n\thelper = "!f() { echo helper >>%s; }; f"\n[core]\n\taskPass = %s\n' "$called" "$tmp/askpass" >"$gh/.gitconfig"
  local cred_req=$'protocol=https\nhost=example.invalid\n\n'
  rm -f "$called"
  HOME="$gh" GIT_ASKPASS="$tmp/askpass" GIT_TERMINAL_PROMPT=0 git credential fill <<<"$cred_req" >/dev/null 2>&1 || true
  need "对照：普通 git 会调 askpass / 凭据助手" [ -s "$called" ]
  rm -f "$called"
  rc=0
  HOME="$gh" GIT_ASKPASS="$tmp/askpass" SSH_ASKPASS="$tmp/askpass" rh_git credential fill <<<"$cred_req" >/dev/null 2>&1 || rc=$?
  check "rh_git：需要鉴权时直接失败" [ "$rc" -ne 0 ]
  check "rh_git：askpass、凭据助手都没被调用" [ ! -e "$called" ]

  if [ "$fails" -eq 0 ]; then
    echo "remote-heavy selftest：通过（${total} 项）"
    return 0
  fi
  echo "remote-heavy selftest：${fails}/${total} 项失败（临时目录随退出删除；要留现场把 trap 去掉重跑）" >&2
  return 1
}

main() {
  case "${1:-}" in
    --selftest) selftest ;;
    clean) shift; cmd_clean "$@" ;;
    list) shift; cmd_list "$@" ;;
    run) shift; cmd_run "$@" ;;
    -h | --help) sed -n '2,40p' "$0" ;;
    '') die_usage "缺参数" ;;
    *) cmd_run "$@" ;;
  esac
}

main "$@"
