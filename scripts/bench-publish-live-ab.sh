#!/usr/bin/env bash
# 流式消息发布链路的前后对照（审查 2026-11-13-m1-t10a-d7 1/F-4；T10a 方案 2.7 G6）：改动前的提交与当前工作树，
# 同一时段交替跑 N 轮（默认 5），每轮记 loadavg，最后按用例给出两边 P50 的中位数与相对变化。只报相对值：本机负载高
# 时绝对值不可信（AGENTS.md「性能」）。
#
# 用法：scripts/bench-publish-live-ab.sh [轮数] [改动前的提交，默认 origin/main 与 HEAD 的分叉点]
# 改动前的那份放在临时工作树里，基准源文件从当前树拷过去（它只用改动前就有的接口），用自己的 CARGO_TARGET_DIR，
# 跑完删掉。两份不能共用 target：cargo 给工作区成员算产物哈希、记依赖文件时用的是相对工作区根的路径，两份副本的
# 同名 crate 会撞成同一份产物，一边编出来的被另一边当成不用重编（2026-11-13 实测，改动后那份链到了改动前的
# iwork-ports）。每次 cargo bench 都自己套重测试锁，外面不要再套。
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH" CARGO_INCREMENTAL=0
ROUNDS="${1:-5}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${2:-$(git -C "$ROOT" merge-base HEAD origin/main)}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/iwork-ab.XXXXXX")"
cleanup() { git -C "$ROOT" worktree remove --force "$WORK/before" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

git -C "$ROOT" worktree add -q --detach "$WORK/before" "$BASE"
cp "$ROOT/crates/iwork-daemon/benches/publish_live.rs" "$WORK/before/crates/iwork-daemon/benches/publish_live.rs"
cat >>"$WORK/before/crates/iwork-daemon/Cargo.toml" <<'EOF'

[[bench]]
name = "publish_live"
harness = false
EOF

load() { sysctl -n vm.loadavg 2>/dev/null || cat /proc/loadavg; }
RAW="$WORK/raw.txt"
run() { # $1 目录 $2 标签 $3 target 目录
  local dir="$1" label="$2" target="$3" out
  echo "== $label load $(load)" | tee -a "$RAW"
  if ! out="$(cd "$dir" && CARGO_TARGET_DIR="$target" lockf /tmp/iwork-heavy-test.lock \
    cargo bench -q -p iwork-daemon --bench publish_live 2>&1)"; then
    echo "$out" | tail -30 >&2
    return 1
  fi
  echo "$out" | grep '"bench":"publish_live"' | sed "s/^/$label /" | tee -a "$RAW"
}
for r in $(seq 1 "$ROUNDS"); do
  run "$WORK/before" "before" "$WORK/target-before"
  run "$ROOT" "after" "$ROOT/target"
done

echo "== 中位数（各轮的 P50 取中位数；after 相对 before）"
python3 -I - "$RAW" <<'EOF'
import json, statistics, sys
rows = {}
for line in open(sys.argv[1]):
    if not line.startswith(("before ", "after ")):
        continue
    side, js = line.split(" ", 1)
    d = json.loads(js)
    for k, v in d.items():
        if k.startswith("p50_") or k == "deltas_received":
            rows.setdefault((d["case"], k), {}).setdefault(side, []).append(v)
for (case, k), sides in sorted(rows.items()):
    b = statistics.median(sides.get("before", [0]))
    a = statistics.median(sides.get("after", [0]))
    rel = f"{(a - b) / b * 100:+.1f}%" if b else "n/a"
    print(f"{case:<24} {k:<16} before {b:>10} after {a:>10} {rel}")
EOF
