#!/usr/bin/env bash
# 审批引擎基准的前后对照（T07 方案 6.10.12；第 9 轮 9/F-5 ④）：引擎重做之前的 747db6b 与当前工作树，
# 同一时段交替跑 N 轮（默认 3），每轮记 loadavg。只报相对值：本机负载高时绝对值不可信（AGENTS.md「性能」）。
#
# 用法：scripts/bench-approval-engine-ab.sh [轮数]
# 旧版用的基准源文件在 crates/iwork-bench/ab/approval_engine_747db6b.rs（旧接口：new + set_modes，只跑 ① 与 ⑤）。
# 旧版放在临时工作树里、用自己的 CARGO_TARGET_DIR，跑完删掉。每次 cargo bench 都套重测试锁。
set -euo pipefail
# 用 rust-toolchain.toml 固定的工具链（经 rustup）；系统自带的 cargo 版本太旧
export PATH="$HOME/.cargo/bin:$PATH" CARGO_INCREMENTAL=0
ROUNDS="${1:-3}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE=747db6b
WORK="$(mktemp -d "${TMPDIR:-/tmp}/iwork-ab.XXXXXX")"
cleanup() { git -C "$ROOT" worktree remove --force "$WORK/before" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

git -C "$ROOT" worktree add -q --detach "$WORK/before" "$BASE"
cp "$ROOT/crates/iwork-bench/ab/approval_engine_747db6b.rs" "$WORK/before/crates/iwork-bench/benches/approval_engine.rs"
cat >>"$WORK/before/crates/iwork-bench/Cargo.toml" <<'EOF'

[[bench]]
name = "approval_engine"
harness = false
EOF
# 旧版的 iwork-bench 没有这几个 dev 依赖
sed -i.bak 's/^iwork-core.workspace = true$/iwork-core.workspace = true\niwork-store.workspace = true\niwork-app.workspace = true\ntokio.workspace = true/' \
  "$WORK/before/crates/iwork-bench/Cargo.toml"

load() { sysctl -n vm.loadavg 2>/dev/null || cat /proc/loadavg; }
run() { # $1 目录 $2 标签 其余是环境变量
  local dir="$1" label="$2" out; shift 2
  echo "== $label load $(load)"
  if ! out="$(cd "$dir" && env "$@" lockf /tmp/iwork-heavy-test.lock cargo bench -q -p iwork-bench --bench approval_engine 2>&1)"; then
    echo "$out" | tail -30 >&2
    return 1
  fi
  echo "$out" | grep -E '次|µs|ms'
}
for r in $(seq 1 "$ROUNDS"); do
  run "$WORK/before" "round $r before ($BASE)" CARGO_TARGET_DIR="$WORK/target-before"
  run "$ROOT" "round $r after (HEAD)" IWORK_BENCH_FOLD_MAX=0
done
