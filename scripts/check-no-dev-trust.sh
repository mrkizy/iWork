#!/usr/bin/env bash
# 发布构建里不存在 --dev-trust（AGENTS.md 安全不变量；T03 任务卡验收 2；方案 S3）。三步证据：
#   ① feature 图：cargo metadata 的解析结果里，iwork-ipc / iwork-client-core / iwork-daemon / iwork-client
#      在工作区默认构建下都没有开 `dev`（这是 feature 依赖图，不是字符串）；
#   ② 产物：release 构建的 iwork-daemon（以及 --artifacts 给的打包产物）里 grep 不到 dev-trust；
#      同一份源码开 dev 构建出来的必须 grep 得到（阳性对照，证明 grep 真能找到）；
#   ③ 行为：release 的 iwork-daemon 带 --dev-trust 启动，退出码非 0、stderr 有「未知参数」、不留 socket。
# ①、③ 才是证明，② 是冒烟。
#
# 用法：scripts/check-no-dev-trust.sh                    三步全跑（会做两次 release 构建）
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
[ "$GRAPH_ONLY" -eq 1 ] && exit 0

EXE=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
BIN="target/release/iwork-daemon${EXE}"
count() { grep -a -c 'dev-trust' "$1" || true; }

# ② 产物：先构建 dev 版做阳性对照，再构建 release（同一路径，后者覆盖前者）
echo "== ② 产物字符串"
cargo build --release --locked -p iwork-daemon --features dev >/dev/null 2>&1 \
  || { echo "check-no-dev-trust: dev 构建失败" >&2; exit 2; }
dev_hits="$(count "$BIN")"
[ "$dev_hits" -gt 0 ] || { echo "check-no-dev-trust: 阳性对照失败：dev 构建里也 grep 不到 dev-trust，grep 证明不了任何事" >&2; exit 2; }
cargo build --release --locked -p iwork-daemon >/dev/null 2>&1 \
  || { echo "check-no-dev-trust: release 构建失败" >&2; exit 2; }
rel_hits="$(count "$BIN")"
echo "dev 构建 ${dev_hits} 处（对照）；release 构建 ${rel_hits} 处"
[ "$rel_hits" -eq 0 ] || { echo "check-no-dev-trust: release 的 iwork-daemon 里有 dev-trust" >&2; exit 1; }
for f in ${ARTIFACTS[@]+"${ARTIFACTS[@]}"}; do
  [ -f "$f" ] || { echo "check-no-dev-trust: 找不到产物 ${f}" >&2; exit 2; }
  n="$(count "$f")"
  echo "${f}：${n} 处"
  [ "$n" -eq 0 ] || { echo "check-no-dev-trust: 打包产物 ${f} 里有 dev-trust" >&2; exit 1; }
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
