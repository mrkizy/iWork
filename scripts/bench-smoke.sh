#!/usr/bin/env bash
# 基准冒烟：把 workspace 里每个 bench 的函数体用缩小的负载真跑一遍，起不来、panic、断言失败就红。
# 不比性能数字。为什么要有它：`cargo test` 不跑基准的函数体，基准布景走的老捷径被存储或引擎的新校验拒掉时
# 没人发现（T10a 1b 实测，approval_engine 因此起不来）。
#
# 跑法：调试构建（`cargo test --bench <名字> -- --bench`）。自写框架的基准认 `--bench` 才开始量
# （write_commit 没有它就直接返回），所以这里显式带上；调试构建复用 cargo test 已经编好的依赖，还多跑一遍 debug_assert。
#
# 用法：scripts/bench-smoke.sh              跑表里全部
#       scripts/bench-smoke.sh <基准名>…   只跑这几个（排查用）
# 退出码：0 全过；1 有基准失败或超时；2 表与 Cargo.toml 里的 bench 目标对不上（新加的 bench 要在下面登记一行）。
# 重测试：本机套 `lockf /tmp/iwork-heavy-test.lock`，或 `scripts/remote-heavy.sh <分支> -- scripts/bench-smoke.sh`。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_INCREMENTAL=0

# 每个基准跑多久算挂住（秒）
PER_BENCH_TIMEOUT=600

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
  start=$(date +%s)
  rc=0
  # vars 是表里写死的 KEY=VALUE，按空白拆开交给 env
  # shellcheck disable=SC2086
  env $vars timeout "$PER_BENCH_TIMEOUT" \
    cargo test -q -p "$pkg" ${features[@]+"${features[@]}"} --bench "$bench" -- --bench </dev/null || rc=$?
  echo "::endgroup::"
  elapsed=$(($(date +%s) - start))
  ran=$((ran + 1))
  if [ "$rc" -eq 0 ]; then
    echo "bench-smoke ok    ${label}（${elapsed}s）"
  else
    [ "$rc" -eq 124 ] && why="超时（${PER_BENCH_TIMEOUT}s）" || why="退出码 $rc"
    echo "bench-smoke FAIL  ${label}：${why}（${elapsed}s）"
    failed+=("$label")
  fi
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
