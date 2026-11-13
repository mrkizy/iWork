#!/usr/bin/env bash
# iWork T04 N4（方案第 8 节）Linux 部分 + N3 第 3 项的 systemd 一半：systemd 用户单元
# （Restart=on-failure、RestartSec=10、StartLimitIntervalSec=300、StartLimitBurst=10）对各种结束方式重不重启；
# enable --now 是否立刻起一个实例；disable --now 是否结束正在跑的实例；exec 自己后仍归这个单元管。
# 在 Docker 里起一个以 systemd 为 1 号进程的 ubuntu 容器（--privileged，跑完删除），用户 t 开 linger 得到用户实例。
# 被管的程序是一个 bash 探针（和 macOS 的 smprobe 同一套模式）：后台本身要到 T04 第 4 步才有，用真的后台走一遍
# 无客户端演练（S11c）留到第 9 步。约 6 分钟。
# 结果：tools/t0/out/t04-systemd/
set -euo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-systemd"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-systemd.XXXXXX")" && pwd -P)
NAME="iwork-t04-systemd-$$"
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
rm -rf "$OUT"; mkdir -p "$OUT"

cat > "$WORK/Dockerfile" <<'DF'
FROM ubuntu:24.04
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq systemd systemd-sysv dbus dbus-user-session >/dev/null \
 && useradd -m t
CMD ["/lib/systemd/systemd"]
DF
docker build -q -t iwork-t04-systemd "$WORK" > /dev/null

cat > "$WORK/agent" <<'AG'
#!/bin/bash
# 被 systemd 起的探针：每次启动往 $LOG 追加一行，再按 $MODE 第一行决定怎么结束
LOG=/home/t/agent.log; MODE=/home/t/mode
log() { echo "$(date +%s.%N | cut -c1-14) pid=$$ $*" >> "$LOG"; }
trap 'log "got SIGTERM"; trap - TERM; kill -TERM $$' TERM
log "start $*"
m=$(head -1 "$MODE" 2>/dev/null || echo sleep)
log "mode $m"
case "$m" in
  sleep) while true; do sleep 1; done ;;
  exit\ *) sleep 1; log exiting; exit "${m#exit }" ;;
  signal\ *) sleep 1; log raising; kill -"${m#signal }" $$; sleep 5 ;;
  exec) if [[ " $* " == *" --execd "* ]]; then while true; do sleep 1; done; fi
        sleep 1; log "exec self"; exec "$0" "$@" --execd ;;
esac
AG
cat > "$WORK/iwork-t04probe.service" <<'UNIT'
[Unit]
Description=iWork T04 probe
StartLimitIntervalSec=300
StartLimitBurst=10
[Service]
ExecStart=/opt/agent --service
Restart=on-failure
RestartSec=10
[Install]
WantedBy=default.target
UNIT

cat > "$WORK/inner.sh" <<'IN'
#!/bin/bash
set -u
S=/out/summary.txt
note() { echo "$*" | tee -a "$S"; }
TUID=$(id -u t)
U() { su - t -c "XDG_RUNTIME_DIR=/run/user/$TUID systemctl --user $*"; }
LOG=/home/t/agent.log
starts_after() { awk -v t="$1" '$1 > t && / start /' "$LOG" | wc -l; }
note "# T04 N4 systemd 用户单元  $(date '+%F %T')"
note "$(uname -srm)；$(systemctl --version | head -1)"
loginctl enable-linger t
for _ in $(seq 60); do [ -S /run/user/$TUID/bus ] && break; sleep 0.5; done
install -m 755 /w/agent /opt/agent
su - t -c 'mkdir -p ~/.config/systemd/user'
install -o t -m 644 /w/iwork-t04probe.service /home/t/.config/systemd/user/
echo sleep > /home/t/mode; chown t /home/t/mode; : > $LOG; chown t $LOG
U daemon-reload
t=$(date +%s.%N | cut -c1-14)
U enable --now iwork-t04probe.service >/dev/null 2>&1
sleep 2
note "## enable --now"
note "  2 秒内起的实例数：$(starts_after $t)；状态：$(U is-active iwork-t04probe.service)"
for m in "exit 0" "exit 12" "exit 20" "exit 21" "signal 9" "signal 15" "signal 11" "signal 6"; do
  echo "$m" > /home/t/mode
  t=$(date +%s.%N | cut -c1-14)
  U restart iwork-t04probe.service >/dev/null 2>&1
  sleep 14
  n=$(starts_after $t)
  ended=$(awk -v t="$t" '$1 > t && /(exiting|raising)/ {print $1; exit}' "$LOG")
  second=$(awk -v t="$t" '$1 > t && / start / {print $1}' "$LOG" | sed -n 2p)
  gap=-; [ -n "$second" ] && [ -n "$ended" ] && gap=$(awk -v a="$second" -v b="$ended" 'BEGIN{printf "%.1f", a-b}')
  note "  ${m}：14 秒内启动 $n 次（含 restart 那一次）；再拉=$([ "$n" -gt 1 ] && echo 是 || echo 否)；结束到再拉 ${gap}s；$(U show -p Result -p ExecMainStatus -p ExecMainCode iwork-t04probe.service | tr '\n' ' ')"
  echo sleep > /home/t/mode; U reset-failed iwork-t04probe.service >/dev/null 2>&1; U restart iwork-t04probe.service >/dev/null 2>&1; sleep 1
done
note "## exec 自己（N3 第 3 项）"
echo exec > /home/t/mode; t=$(date +%s.%N | cut -c1-14); U restart iwork-t04probe.service >/dev/null 2>&1; sleep 3
before=$(awk -v t="$t" '$1 > t && /exec self/ {print $2; exit}' "$LOG")
mp=$(U show -p MainPID --value iwork-t04probe.service)
note "  exec 前 ${before}，exec 后 MainPID=${mp}，状态 $(U is-active iwork-t04probe.service)，带 --execd 的启动行：$(awk -v t="$t" '$1 > t && /--execd/' "$LOG" | wc -l)"
t=$(date +%s.%N | cut -c1-14); kill -9 "$mp"; sleep 12
note "  kill -9 exec 后的进程：12 秒内再拉 $(starts_after $t) 次"
note "## disable --now"
echo sleep > /home/t/mode; U restart iwork-t04probe.service >/dev/null 2>&1; sleep 1
mp=$(U show -p MainPID --value iwork-t04probe.service); t=$(date +%s.%N | cut -c1-14)
U disable --now iwork-t04probe.service >/dev/null 2>&1; sleep 2
note "  正在跑的实例（pid ${mp}）收到 SIGTERM：$(awk -v t="$t" '$1 > t && /got SIGTERM/' "$LOG" | wc -l)；进程还在：$(kill -0 $mp 2>/dev/null && echo 是 || echo 否)；状态：$(U is-active iwork-t04probe.service)；is-enabled：$(U is-enabled iwork-t04probe.service 2>&1)"
note "## 启动次数上限（StartLimitBurst=10 / 300s）"
echo "exit 20" > /home/t/mode; U enable --now iwork-t04probe.service >/dev/null 2>&1
t=$(date +%s.%N | cut -c1-14); sleep 125
note "  一直以 20 退出，125 秒内启动 $(starts_after $t) 次；状态：$(U is-active iwork-t04probe.service)；Result：$(U show -p Result --value iwork-t04probe.service)"
U disable --now iwork-t04probe.service >/dev/null 2>&1
cp "$LOG" /out/agent.log
IN
chmod +x "$WORK/inner.sh"

docker run -d --name "$NAME" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  -v "$WORK:/w:ro" -v "$OUT:/out" iwork-t04-systemd >/dev/null
for _ in $(seq 60); do docker exec "$NAME" systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded' && break; sleep 1; done
docker exec "$NAME" /w/inner.sh
