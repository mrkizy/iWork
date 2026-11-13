#!/usr/bin/env bash
# ipc 基准里慢消费者对照的前提检查（墙钟第一批审查 2/F-1）：对照组的 A 没收齐、或停读组的 A 没被关时，
# 这一对只打印 INVALID 和原因，不报 P50/P95/P99，程序照常退出 0。
#
# 跑两次 ipc 基准（每组缩到 3 秒）：
#   1. 不注入：要有两行 ipc_slow_consumer 的延迟，没有 INVALID（对照，证明检查不是一律报无效）；
#   2. 让对照组的 A 读到第一条后停 2 秒（预算调小，停这么久就会被服务端关掉）：要有 INVALID 且点名
#      「对照组 A 没收齐」，没有任何 ipc_slow_consumer 的延迟行，退出码仍是 0。
#
# 用法：scripts/check-ipc-bench-invalid.sh
# 每次 cargo bench 都在脚本里套重测试锁，外面不要再套。
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH" CARGO_INCREMENTAL=0
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

bench() { # 环境变量 → 基准的标准输出
  env IWORK_BENCH_FANOUT_SECS=3 "$@" lockf /tmp/iwork-heavy-test.lock \
    cargo bench -q -p iwork-bench --bench ipc 2>/dev/null
}
fail() { echo "FAIL：$1" >&2; echo "$2" >&2; exit 1; }

out="$(bench)" || fail "不注入时基准退出码非 0" "$out"
rows="$(grep -c '^ipc_slow_consumer（' <<<"$out" || true)"
[ "$rows" = 2 ] || fail "不注入时应有 2 行慢消费者延迟，实际 $rows 行" "$out"
! grep -q 'INVALID' <<<"$out" || fail "不注入时不该报 INVALID" "$out"
echo "ok 不注入：两行延迟，没有 INVALID"

out="$(bench IWORK_BENCH_SLOW_CONSUMER_CONTROL_PAUSE_MS=2000)" || fail "注入停顿后基准退出码非 0（应照常退出 0）" "$out"
grep -q '^ipc_slow_consumer INVALID：.*对照组 A 没收齐' <<<"$out" \
  || fail "注入停顿后应报 INVALID 并点名对照组 A 没收齐" "$out"
! grep -q '^ipc_slow_consumer（' <<<"$out" || fail "INVALID 时不该再报这一对的延迟行" "$out"
echo "ok 注入停顿：$(grep '^ipc_slow_consumer INVALID' <<<"$out")"
