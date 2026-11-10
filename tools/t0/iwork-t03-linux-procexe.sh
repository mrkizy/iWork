#!/usr/bin/env bash
# iWork T03 实测 N3（方案 8.3）：Linux 上对端进程运行中，它的可执行文件被换掉之后，
# 打开 /proc/<pid>/exe 读到的是不是仍是运行中那份映像。
#
# 三种换法各起一个新进程测：改名替换（mv 覆盖）、原地覆盖（dd 改一字节 / 截断重写）、删除（rm）。
# 每种都在换文件之后才去打开 /proc/<pid>/exe（和后台服务的真实时序一致：连接进来时文件可能早被换了），
# 比较读到的哈希与启动前的原件哈希，并记下 readlink 的结果（看 " (deleted)" 后缀）。
#
# 只用 coreutils，不需要 root；只碰自己建的临时目录。结果写到本脚本旁边的 out/t03-linux/。
# 用法（在 Linux 上）：bash tools/t0/iwork-t03-linux-procexe.sh
set -u

OUT="$(cd "$(dirname "$0")" && pwd)/out/t03-linux"
rm -rf "$OUT"; mkdir -p "$OUT"
WORK="$(mktemp -d)"
trap 'kill "${PID:-}" 2>/dev/null; rm -rf "$WORK"' EXIT
SUMMARY="$OUT/summary.txt"
log() { echo "$*" | tee -a "$SUMMARY"; }
hash_of() { sha256sum "$1" 2>&1 | cut -d' ' -f1; }

# 原件：一个会停住的程序；替身：另一个不同内容的程序。
ORIG=/bin/sleep
OTHER=/bin/true

log "date: $(date -u +%FT%TZ)"
log "kernel: $(uname -srm)"
log "coreutils: $(sleep --version | head -1)"
log "原件 $ORIG sha256=$(hash_of "$ORIG")"
log "替身 $OTHER sha256=$(hash_of "$OTHER")"

# start_peer <名字>：把原件复制成 $WORK/<名字> 并运行，设置 PID 与 BEFORE。
start_peer() {
    PEER="$WORK/$1"
    cp "$ORIG" "$PEER"
    "$PEER" 300 &
    PID=$!
    sleep 0.2
    BEFORE="$(hash_of "$PEER")"
}

# check <情形>：换完文件后打开 /proc/<pid>/exe，记录哈希与 readlink，判定哈希是否不变。
check() {
    local case_name="$1" now link verdict
    now="$(hash_of "/proc/$PID/exe")"
    link="$(readlink "/proc/$PID/exe" 2>&1)"
    if [ "$now" = "$BEFORE" ]; then verdict=SAME; else verdict=CHANGED; fi
    log "[$case_name] exe-hash=$verdict before=${BEFORE:0:16} now=${now:0:16} readlink=\"$link\""
    kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; PID=
}

log ""
log "== (a) 改名替换：mv 走原件，再在原路径放替身；以及 mv 覆盖（rename 到原路径）"
start_peer a-peer
mv "$PEER" "$PEER.old"; cp "$OTHER" "$PEER"
log "路径上的文件现在 sha256=$(hash_of "$PEER" | cut -c1-16)"
check "a1 mv-away+new-file"

start_peer a2-peer
cp "$OTHER" "$WORK/a2-replacement"; mv -f "$WORK/a2-replacement" "$PEER"
check "a2 mv-over"

log ""
log "== (b) 原地覆盖：对运行中的映像文件写入"
start_peer b-peer
printf '\x00' | dd of="$PEER" bs=1 seek=100 conv=notrunc 2>"$WORK/dd.err"; rc=$?
log "dd 改一字节 exit=$rc stderr=\"$(tr '\n' ' ' < "$WORK/dd.err")\""
check "b1 dd-notrunc"

start_peer b2-peer
cp "$OTHER" "$PEER" 2>"$WORK/cp.err"; rc=$?
log "cp 截断重写 exit=$rc stderr=\"$(tr '\n' ' ' < "$WORK/cp.err")\""
check "b2 cp-truncate"

log ""
log "== (c) 删除"
start_peer c-peer
rm "$PEER"
log "路径还在吗：$([ -e "$PEER" ] && echo yes || echo no)"
check "c rm"

log ""
log "== (d) 进程退出后再读（pid 已不在）"
start_peer d-peer
kill "$PID"; wait "$PID" 2>/dev/null
log "[d exited] $(sha256sum "/proc/$PID/exe" 2>&1 | head -1)"
PID=

log ""
log "完成，结果在 $OUT"
