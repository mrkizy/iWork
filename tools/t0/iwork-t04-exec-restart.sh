#!/usr/bin/env bash
# iWork T04 N3（方案第 8 节）第 1、2、4 项：exec 自行重启后锁、socket、子进程管道都随 CLOEXEC 放掉；等锁接班。
# 第 3 项（launchd 管着的进程 exec 自己后仍是同一个任务）在 iwork-t04-macos-smappservice.sh 里做（N3-3 那几行）；
# systemd 那一半在 iwork-t04-linux-systemd.sh 里做。macOS 在本机跑，Linux 在 Docker（rust:1）里跑同一个程序。
# 结果：tools/t0/out/t04-exec-restart/
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-exec-restart"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-exec.XXXXXX")" && pwd -P)
trap 'pkill -f "$WORK/" 2>/dev/null || true; rm -rf "$WORK"' EXIT
rm -rf "$OUT"; mkdir -p "$OUT"
S="$OUT/summary.txt"
note() { echo "$*" | tee -a "$S"; }

# run_all <可执行文件> <工作目录>：两组都跑一遍，输出到 stdout
cat > "$WORK/run.sh" <<'INNER'
set -eu
P="$1"; W="$2"
mkdir -p "$W/a" "$W/b"
"$P" exec "$W/a"
"$P" hold "$W/b" > "$W/b/hold.txt" & H=$!
sleep 0.5
"$P" waitlock "$W/b" > "$W/b/wait.txt" & WPID=$!
sleep 1
echo "kill -9 前任 at_ms=$(python3 -c 'import time;print(int(time.time()*1000))' 2>/dev/null || date +%s000)"
kill -9 "$H"
wait "$WPID" || true
cat "$W/b/hold.txt" "$W/b/wait.txt"
INNER

note "# T04 N3 exec 自行重启  $(date '+%F %T')"
rustc --edition 2024 -O "$ROOT/tools/t0/iwork-t04-exec-restart.rs" -o "$WORK/probe"
note "## macOS $(sw_vers -productVersion) $(uname -m)"
bash "$WORK/run.sh" "$WORK/probe" "$WORK/mac" 2>&1 | sed -e "s#$WORK#<work>#g" -e 's/^/  /' | tee -a "$S"

note "## Linux（Docker rust:1）"
if docker info >/dev/null 2>&1; then
  docker run --rm -v "$ROOT/tools/t0/iwork-t04-exec-restart.rs:/p.rs:ro" -v "$WORK/run.sh:/run.sh:ro" rust:1 \
    bash -c 'rustc --edition 2024 -O /p.rs -o /tmp/probe 2>/dev/null && uname -srm && bash /run.sh /tmp/probe /tmp/w' 2>&1 | sed 's/^/  /' | tee -a "$S"
else
  note "  Docker 不可用，跳过"
fi
