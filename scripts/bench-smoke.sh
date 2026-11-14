#!/usr/bin/env bash
# 基准冒烟：把 workspace 里每个 bench 的函数体用缩小的负载真跑一遍，起不来、panic、断言失败就红。
# 不比性能数字。为什么要有它：`cargo test` 不跑基准的函数体，基准布景走的老捷径被存储或引擎的新校验拒掉时
# 没人发现（T10a 1b 实测，approval_engine 因此起不来）。
#
# 跑法：调试构建（`cargo test --bench <名字> -- --bench`）。自写框架的基准认 `--bench` 才开始量
# （write_commit 没有它就直接返回），所以这里显式带上；调试构建复用 cargo test 已经编好的依赖，还多跑一遍 debug_assert。
#
# 判红：退出码非 0、超时，或者输出里出现 panic 行（见 PANIC_LINE）。最后一条管的是后台线程和 tokio 任务里的
# panic：tokio 接住任务的 panic、产品代码可能不看 JoinError，基准照样退出 0（基准冒烟审查 1/F-1）。
#
# 用法：scripts/bench-smoke.sh              跑表里全部
#       scripts/bench-smoke.sh <基准名>…   只跑这几个（排查用）
#       scripts/bench-smoke.sh --selftest  不编不跑基准，用假命令核判红规则（panic 行必红、没有 panic 的退出 0 必绿）
# 退出码：0 全过；1 有基准失败或超时；2 表与 Cargo.toml 里的 bench 目标对不上（新加的 bench 要在下面登记一行）。
# 重测试：本机套 `lockf /tmp/iwork-heavy-test.lock`，或 `scripts/remote-heavy.sh <分支> -- scripts/bench-smoke.sh`。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_INCREMENTAL=0

# 每个基准跑多久算挂住（秒）
PER_BENCH_TIMEOUT=600

# Rust 默认 panic hook 在任何线程 panic 时都往 stderr 打这一行（tokio 接住任务的 panic 之前，hook 已经打过了；
# 基准自己拉起、stderr 没重定向的子进程也一样）：
#   thread 'tokio-rt-worker' (62670956) panicked at crates/iwork-ipc/src/server/connection.rs:405:17:
# 只认行首这个固定格式，括号里的线程号可有可无（旧版 Rust 没有）。防误报：基准自己的输出是一行 JSON 或
# 「组名 数字」，不以 thread ' 开头；cargo 的编译输出也没有这种行。哪天真有基准要故意 panic，在登记表里给它
# 单开一列允许，而不是放宽这条规则。
PANIC_LINE="^thread '[^']*'( \([0-9]+\))? panicked at "

# 在这些文件里找 panic 行。0 找到（打出第一行），1 没有，2 读不了（grep 出错）——调用方把 0 和 2 都判红。
scan_panic() { # 文件…
  local rc=0 hit
  hit=$(grep -hE "$PANIC_LINE" "$@" 2>&1) || rc=$?
  case "$rc" in
    0) printf '%s' "$hit" | head -1 ;;
    1) ;;
    *) printf '扫描失败（grep 退出码 %s）：%s' "$rc" "$hit" | head -1; rc=2 ;;
  esac
  return "$rc"
}

# 跑一次，退出码 0 表示通过；结论那一行（ok / FAIL）放在 RUN_RESULT 里，由调用方在折叠组外面打出来。
# 命令的输出同时进终端和一个临时文件；命令拉起的子进程把 stderr 写进 IWORK_BENCH_CHILD_STDERR_DIR（lifecycle 的后台）。
# 跑完在这两处找 panic 行。日志、目录建不起来、tee 写失败、grep 读错，一律判红：采集坏了不能当成「没看到 panic」。
run_one() { # 标签 命令…
  local label=$1 log childdir rc tee_rc start elapsed why hit scan_rc f
  shift
  RUN_RESULT="bench-smoke FAIL  ${label}：建不了临时日志（TMPDIR=${TMPDIR:-/tmp}）"
  log=$(mktemp "${TMPDIR:-/tmp}/bench-smoke.XXXXXX") || return 1
  childdir=$(mktemp -d "${TMPDIR:-/tmp}/bench-smoke-child.XXXXXX") || { rm -f "$log"; return 1; }
  start=$(date +%s)
  set +e
  env IWORK_BENCH_CHILD_STDERR_DIR="$childdir" "$@" </dev/null 2>&1 | tee "$log"
  local st=("${PIPESTATUS[@]}")
  set -e
  rc=${st[0]}
  tee_rc=${st[1]}
  elapsed=$(($(date +%s) - start))
  why=""
  if [ "$rc" -eq 124 ]; then
    why="超时（${PER_BENCH_TIMEOUT}s）"
  elif [ "$rc" -ne 0 ]; then
    why="退出码 $rc"
  elif [ "$tee_rc" -ne 0 ]; then
    why="退出码 0，但输出没能写进日志（tee 退出码 ${tee_rc}），没法检查 panic"
  else
    for f in "$log" "$childdir"/*; do
      [ -e "$f" ] || continue
      scan_rc=0
      hit=$(scan_panic "$f") || scan_rc=$?
      if [ "$scan_rc" -eq 0 ]; then
        [ "$f" = "$log" ] && why="退出码 0，但有线程或任务 panic 过：$hit" ||
          why="退出码 0，但子进程 $(basename "$f") 里有 panic：$hit"
        break
      elif [ "$scan_rc" -ne 1 ]; then
        why="退出码 0，但 $(basename "$f") 没法检查 panic：$hit"
        break
      fi
    done
  fi
  rm -rf "$log" "$childdir"
  if [ -z "$why" ]; then
    RUN_RESULT="bench-smoke ok    ${label}（${elapsed}s）"
    return 0
  fi
  RUN_RESULT="bench-smoke FAIL  ${label}：${why}（${elapsed}s）"
  return 1
}

# 判红规则的对照：不编译、不跑基准，几秒跑完（CI 的 lint job 跑它）。
selftest() {
  local fails=0 fake
  fake=$(mktemp -d "${TMPDIR:-/tmp}/bench-smoke-selftest.XXXXXX")
  # 假 tee / grep：模拟日志写不进、扫描读错
  mkdir "$fake/badtee" "$fake/badgrep"
  printf '#!/bin/sh\ncat >/dev/null\necho "tee: 写不进" >&2\nexit 1\n' >"$fake/badtee/tee"
  printf '#!/bin/sh\necho "grep: 读不了" >&2\nexit 2\n' >"$fake/badgrep/grep"
  chmod +x "$fake/badtee/tee" "$fake/badgrep/grep"
  expect() { # 期望(0|1) 说明 命令…
    local want=$1 what=$2 got=0
    shift 2
    run_one "$what" "$@" >/dev/null 2>&1 || got=1
    if [ "$got" -eq "$want" ]; then echo "  ok    $what"; else
      echo "  FAIL  ${what}（期望 $([ "$want" -eq 0 ] && echo 绿 || echo 红)）"
      fails=$((fails + 1))
    fi
  }
  local panic_line="thread 'tokio-rt-worker' (62670956) panicked at crates/iwork-ipc/src/server/connection.rs:405:17:"
  expect 1 "后台任务 panic、进程退出 0：红" sh -c \
    "echo \"$panic_line\" >&2; echo 'ipc_slow_consumer INVALID：对照组 A 没收齐（Closed）（这一对不报延迟）'; exit 0"
  expect 1 "旧版格式（没有线程号）的 panic 行：红" sh -c "echo \"thread 'main' panicked at src/a.rs:1:1:\" >&2; exit 0"
  expect 0 "没有 panic 的 INVALID、退出 0：绿" sh -c \
    "echo 'ipc_slow_consumer INVALID：对照组 A 没收齐（Closed）（这一对不报延迟）'; exit 0"
  expect 0 "输出里在行中提到 panicked、不是 panic 行：绿" sh -c \
    "echo '{\"bench\":\"x\",\"note\":\"thread 'a' panicked at\"}'; exit 0"
  expect 1 "退出码非 0：红" sh -c 'exit 3'
  expect 0 "对照：什么都没有、退出 0：绿" true
  # 子进程（后台）的 stderr：任务 panic 后被恢复、基准照样退出 0，也要红
  expect 1 "子进程 stderr 里有 panic、基准退出 0：红" sh -c \
    "echo \"thread 'tokio-rt-worker' (9) panicked at crates/iwork-daemon/src/tasks/outbox.rs:138:9:\" >\"\$IWORK_BENCH_CHILD_STDERR_DIR/daemon-0.stderr\"; echo ok; exit 0"
  expect 0 "子进程 stderr 有内容但没有 panic：绿" sh -c \
    "echo 'some log line' >\"\$IWORK_BENCH_CHILD_STDERR_DIR/daemon-0.stderr\"; exit 0"
  # 采集坏了一律红
  # 下面三格各在子 shell 里换掉 TMPDIR / PATH，只影响这一格；子 shell 的退出码就是这一格的结果
  (
    fails=0 TMPDIR="$fake/no-such-dir"
    expect 1 "TMPDIR 指向不存在的目录、日志建不起来：红" sh -c "echo \"$panic_line\" >&2; exit 0"
    exit "$fails"
  ) || fails=$((fails + 1))
  (
    fails=0 PATH="$fake/badtee:$PATH"
    expect 1 "tee 写失败、命令退出 0：红" sh -c 'echo hi; exit 0'
    exit "$fails"
  ) || fails=$((fails + 1))
  (
    fails=0 PATH="$fake/badgrep:$PATH"
    expect 1 "grep 读错（退出码 2）、命令退出 0：红" sh -c 'echo hi; exit 0'
    exit "$fails"
  ) || fails=$((fails + 1))
  rm -rf "$fake"
  if [ "$fails" -ne 0 ]; then
    echo "bench-smoke selftest：$fails 项不符" >&2
    exit 1
  fi
  echo "bench-smoke selftest：通过"
  exit 0
}

if [ "${1:-}" = "--selftest" ]; then selftest; fi

# 登记表：包 基准 feature（没有写 -） 环境变量…
# 一行一次运行；同一个基准可以有几行（例如带不带观察者）。规模开关要缩到「每一组都还会跑」：
#   approval_engine：IWORK_BENCH_FOLD_MAX 小于 10000 时启动折叠组一档都不跑，所以取 10000（只跑 1 万那档）；
#   lifecycle：IWORK_BENCH_COLD_EVENTS_MAX=10000 跑空库与 1 万两档，跳过 10 万；其余组都不受它影响；
#   write_commit：IWORK_BENCH_COMMIT_PER 只给每个线程封顶，三个场景照跑。
TABLE='
iwork-bench placeholder -
iwork-bench approval -
iwork-bench approval_engine - IWORK_BENCH_ENGINE_N=20 IWORK_BENCH_FOLD_MAX=10000
iwork-bench ipc - IWORK_BENCH_FANOUT_SECS=1
iwork-bench turn_stream - IWORK_BENCH_TURN_UPDATES=2000 IWORK_BENCH_TURN_REPS=1
iwork-bench view -
iwork-bench timeline_apply -
iwork-bench assistant_edits -
iwork-store write_commit - IWORK_BENCH_COMMIT_PER=20
iwork-store write_commit - IWORK_BENCH_COMMIT_PER=20 IWORK_BENCH_OBSERVER=1
iwork-store view_queries - IWORK_BENCH_VIEW_TURNS=5 IWORK_BENCH_VIEW_TOOLS=3 IWORK_BENCH_VIEW_REPS=3
iwork-daemon publish_live - IWORK_BENCH_LIVE_MESSAGES=5 IWORK_BENCH_LIVE_DELTAS=3
iwork-daemon command_path - IWORK_BENCH_COMMAND_N=20
iwork-daemon lifecycle dev IWORK_BENCH_COLD_EVENTS_MAX=10000 IWORK_BENCH_IDLE_SECS=1
iwork-claude-hook cold_start -
'

# 表里登记的「包 基准」与 Cargo.toml 里的 bench 目标必须一一对上：漏登记的新基准不会悄悄不跑。
check_table() {
  local declared listed
  declared=$(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | .name as $p | .targets[] | select(.kind | index("bench")) | "\($p) \(.name)"' | sort -u)
  listed=$(printf '%s\n' "$TABLE" | awk 'NF {print $1, $2}' | sort -u)
  if [ "$declared" != "$listed" ]; then
    echo "bench-smoke：登记表与 Cargo.toml 的 bench 目标对不上（< 只在 Cargo.toml，> 只在表里）：" >&2
    diff <(printf '%s\n' "$declared") <(printf '%s\n' "$listed") >&2 || true
    exit 2
  fi
}

check_table
failed=()
ran=0
while read -r pkg bench feature vars; do
  [ -n "${pkg:-}" ] || continue
  if [ "$#" -gt 0 ]; then
    want=0
    for b in "$@"; do [ "$b" = "$bench" ] && want=1; done
    [ "$want" -eq 1 ] || continue
  fi
  features=()
  [ "$feature" = - ] || features=(--features "$feature")
  label="$pkg/$bench${vars:+ ($vars)}"
  echo "::group::bench-smoke $label"
  ok=0
  # vars 是表里写死的 KEY=VALUE，按空白拆开交给 env
  # shellcheck disable=SC2086
  run_one "$label" env $vars timeout "$PER_BENCH_TIMEOUT" \
    cargo test -q -p "$pkg" ${features[@]+"${features[@]}"} --bench "$bench" -- --bench || ok=1
  echo "::endgroup::"
  echo "$RUN_RESULT"
  ran=$((ran + 1))
  [ "$ok" -eq 0 ] || failed+=("$label")
done <<<"$TABLE"

if [ "$ran" -eq 0 ]; then
  echo "bench-smoke：没有跑任何基准（参数里的名字不在表里？）" >&2
  exit 1
fi
if [ "${#failed[@]}" -gt 0 ]; then
  echo "bench-smoke：${#failed[@]}/${ran} 个失败：" >&2
  printf '  %s\n' "${failed[@]}" >&2
  exit 1
fi
echo "bench-smoke：${ran} 次运行全部通过"
