#!/usr/bin/env bash
# iWork T04 N2（方案第 8 节）：单调钟在睡眠期间走不走。不让本机真的睡：
# 读「计睡眠的钟 − 不计睡眠的钟」（开机以来睡过多久），对照系统电源日志里记的睡眠；再确认 std::time::Instant 用的
# 是不计睡眠的那个钟（tokio::time 的计时也建在 std::time::Instant 上）。
# macOS 在本机跑；Linux 在 Docker（rust:1）里跑同一个程序。
# 想真睡一次：`<编译出的程序> watch 180` 跑着时合盖或 `pmset sleepnow`，睡 60s 以上再唤醒，看 wall_minus_instant。
# 结果：tools/t0/out/t04-sleep-clock/
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-sleep-clock"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-clock.XXXXXX")" && pwd -P)
trap 'rm -rf "$WORK"' EXIT
rm -rf "$OUT"; mkdir -p "$OUT"
S="$OUT/summary.txt"
note() { echo "$*" | tee -a "$S"; }

note "# T04 N2 睡眠与单调钟  $(date '+%F %T')"
rustc --edition 2024 -O "$ROOT/tools/t0/iwork-t04-sleep-clock.rs" -o "$WORK/clock"
note "## macOS $(sw_vers -productVersion) $(uname -m)；$(rustc --version)"
note "  $("$WORK/clock")"
boot=$(sysctl -n kern.boottime | sed -n 's/.*sec = \([0-9]*\),.*/\1/p')
note "  开机于 $(date -r "$boot" '+%F %T')；电源日志里开机以来的睡眠条目："
pmset -g log | awk -v b="$(date -r "$boot" '+%F %T')" '($1" "$2) >= b && $4 == "Sleep" {n++; s=$0} END {print "    条数 " n "；最后一条：" s}' | tee -a "$S"
pmset -g log | awk -v b="$(date -r "$boot" '+%F %T')" '($1" "$2) >= b && ($4 == "Sleep" || $4 == "Wake" || $4 == "DarkWake")' > "$OUT/pmset-sleep-wake.txt" || true

note "## Linux（Docker rust:1）"
if docker info >/dev/null 2>&1; then
  docker run --rm -v "$ROOT/tools/t0/iwork-t04-sleep-clock.rs:/c.rs:ro" rust:1 \
    bash -c 'rustc --edition 2024 -O /c.rs -o /tmp/clock 2>/dev/null && uname -srm && /tmp/clock' | sed 's/^/  /' | tee -a "$S"
else
  note "  Docker 不可用，跳过"
fi
