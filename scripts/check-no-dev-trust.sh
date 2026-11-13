#!/usr/bin/env bash
# 发布构建里不存在 --dev-trust（AGENTS.md 安全不变量；T03 任务卡验收 2；方案 S3）。三步证据：
#   ① feature 图：cargo metadata 的解析结果里，iwork-ipc / iwork-client-core / iwork-daemon / iwork-client
#      在工作区默认构建下都没有开 `dev`；iwork-bridge-claude 没有开 `fault-injection` 与 `fake-cli`（T06 方案 4.3、
#      15.1：前者让环境变量改 hook 的 timeout，后者是测试用的假 CLI）。这是 feature 依赖图，不是字符串；
#   ② 产物：release 构建的 iwork-daemon、iwork-bridge-claude（以及 --artifacts 给的打包产物）里 grep 不到
#      开发字样（dev-trust、IWORK_DEV_、桥接的故障注入变量、假 CLI 的变量）；同一份源码开对应测试 feature
#      构建出来的必须 grep 得到、且被同一个扫描判红（阳性对照，证明扫描真能识别）；
#   ③ 行为：release 的 iwork-daemon 带 --dev-trust 启动，退出码非 0、stderr 有「未知参数」、不留 socket。
# ①、③ 才是证明，② 是冒烟。
#
# 用法：scripts/check-no-dev-trust.sh                    三步全跑（会做六次 release 构建）
#       scripts/check-no-dev-trust.sh --graph-only       只跑 ①（lint job 用，不构建）
#       scripts/check-no-dev-trust.sh --artifacts F...   ② 额外检查这些文件（Tauri 打包产物）
# 退出码：0 通过；1 违规；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
command -v jq >/dev/null || { echo "check-no-dev-trust: 需要 jq" >&2; exit 2; }

GRAPH_ONLY=0
ARTIFACTS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --graph-only) GRAPH_ONLY=1; shift ;;
    --artifacts) shift; while [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; do ARTIFACTS+=("$1"); shift; done ;;
    *) echo "check-no-dev-trust: 未知参数 ${1}" >&2; exit 2 ;;
  esac
done

# ① feature 图
echo "== ① feature 图"
GUARDED='["iwork-ipc","iwork-client-core","iwork-daemon","iwork-client"]'
meta="$(cargo metadata --format-version 1 --locked)" || { echo "check-no-dev-trust: cargo metadata 失败" >&2; exit 2; }
bad="$(printf '%s' "$meta" | jq -r --argjson guarded "$GUARDED" '
  . as $m
  | [$m.packages[] | select(.source == null) | {id, name}] as $local
  | $m.resolve.nodes[]
  | . as $n
  | ($local[] | select(.id == $n.id) | .name) as $name
  | select($guarded | index($name))
  | select($n.features | index("dev"))
  | $name')"
if [ -n "$bad" ]; then
  echo "check-no-dev-trust: 下列 crate 在工作区默认构建下开了 dev feature：" >&2
  echo "$bad" >&2
  exit 1
fi
echo "通过：iwork-ipc、iwork-client-core、iwork-daemon、iwork-client 都没有开 dev"
bad="$(printf '%s' "$meta" | jq -r '
  . as $m
  | [$m.packages[] | select(.source == null and .name == "iwork-bridge-claude") | .id] as $ids
  | $m.resolve.nodes[]
  | select(.id as $id | $ids | index($id))
  | .features[]
  | select(. == "fault-injection" or . == "fake-cli")')"
if [ -n "$bad" ]; then
  echo "check-no-dev-trust: iwork-bridge-claude 在工作区默认构建下开了：" >&2
  echo "$bad" >&2
  exit 1
fi
echo "通过：iwork-bridge-claude 没有开 fault-injection、fake-cli"
[ "$GRAPH_ONLY" -eq 1 ] && exit 0

EXE=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
BIN="target/release/iwork-daemon${EXE}"
BRIDGE="target/release/iwork-bridge-claude${EXE}"
FAKE_CLI="target/release/iwork-fake-claude${EXE}"
# 发布产物里不许出现的开发字样，每条注明由哪个构建做阳性对照：
#   - 开关本身与后台的开发故障注入点（`IWORK_DEV_*` 环境变量，T04）：iwork-daemon 开 dev；
#   - 桥接的故障注入（T06 方案 4.3 的 hook timeout；S6-5b 审查的会话任务 panic）：iwork-bridge-claude 开 fault-injection；
#   - 测试用假 CLI 的脚本与记录变量（T06 方案 15.1）：iwork-fake-claude（fake-cli feature 才有这个程序）。
PATTERNS=('dev-trust' 'IWORK_DEV_' 'IWORK_BRIDGE_HOOK_TIMEOUT_SECS' 'IWORK_BRIDGE_FAULT_PANIC_ON' 'IWORK_FAKE_CLAUDE_')
count() { grep -a -c -- "$2" "$1" || true; }
# 文件里出现任何一条字样就报出来、返回 1
scan() {
  local f="$1" p n bad=0
  for p in "${PATTERNS[@]}"; do
    n="$(count "$f" "$p")"
    echo "${f}：${p} ${n} 处"
    [ "$n" -eq 0 ] || bad=1
  done
  return "$bad"
}
# 阳性对照：开了测试 feature 的构建里必须 grep 得到它那条字样，并且 scan 判它红（证明扫描真能识别）
positive() {
  local f="$1" p="$2" n
  n="$(count "$f" "$p")"
  echo "${f}：${p} ${n} 处（阳性对照）"
  [ "$n" -gt 0 ] || { echo "check-no-dev-trust: 阳性对照失败：${f} 里 grep 不到 ${p}，grep 证明不了任何事" >&2; exit 2; }
  if scan "$f" >/dev/null; then
    echo "check-no-dev-trust: 阳性对照失败：scan 没把 ${f} 判红" >&2; exit 2
  fi
}
build() {
  local log
  log="$(cargo build --release --locked "$@" 2>&1)" || {
    echo "check-no-dev-trust: 构建失败：cargo build --release --locked $*" >&2
    printf '%s\n' "$log" | tail -20 >&2
    exit 2
  }
}
# ② 产物：先构建开测试 feature 的版本做阳性对照，再构建发布版（同一路径，后者覆盖前者）
echo "== ② 产物字符串"
build -p iwork-daemon --features dev
positive "$BIN" 'dev-trust'
positive "$BIN" 'IWORK_DEV_'
# 桥接的 ACP 端与假 CLI 的脚本执行只在 unix 上编（M1 不支持 Windows），Windows 上没有可对照的字样
if [ -z "$EXE" ]; then
  build -p iwork-bridge-claude --features fault-injection
  positive "$BRIDGE" 'IWORK_BRIDGE_HOOK_TIMEOUT_SECS'
  positive "$BRIDGE" 'IWORK_BRIDGE_FAULT_PANIC_ON'
  build -p iwork-bridge-claude --features fake-cli --bin iwork-fake-claude
  positive "$FAKE_CLI" 'IWORK_FAKE_CLAUDE_'
fi
build -p iwork-daemon
build -p iwork-bridge-claude
for f in "$BIN" "$BRIDGE"; do
  scan "$f" || { echo "check-no-dev-trust: release 的 ${f} 里有开发字样" >&2; exit 1; }
done
for f in ${ARTIFACTS[@]+"${ARTIFACTS[@]}"}; do
  [ -f "$f" ] || { echo "check-no-dev-trust: 找不到产物 ${f}" >&2; exit 2; }
  scan "$f" || { echo "check-no-dev-trust: 打包产物 ${f} 里有开发字样" >&2; exit 1; }
done
# ③ 行为：发布进程收到 --dev-trust 拒绝启动、不留 socket。家目录与运行目录都指到临时目录。
echo "== ③ 发布进程拒绝 --dev-trust"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
set +e
HOME="$tmp" XDG_RUNTIME_DIR="$tmp/run" XDG_DATA_HOME="$tmp/data" "$BIN" --dev-trust >"$tmp/out" 2>"$tmp/err"
code=$?
set -e
echo "退出码 ${code}；stderr：$(cat "$tmp/err")"
[ "$code" -ne 0 ] || { echo "check-no-dev-trust: release 后台接受了 --dev-trust" >&2; exit 1; }
grep -q '未知参数' "$tmp/err" || { echo "check-no-dev-trust: stderr 里没有「未知参数」" >&2; exit 1; }
socks="$(find "$tmp" -name '*.sock' | wc -l | tr -d ' ')"
[ "$socks" -eq 0 ] || { echo "check-no-dev-trust: 拒绝启动却留下了 socket" >&2; exit 1; }
echo "check-no-dev-trust: 三步全部通过"
