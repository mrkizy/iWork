#!/usr/bin/env bash
# 发布构建里不存在 --dev-trust（AGENTS.md 安全不变量；T03 任务卡验收 2；方案 S3）。三步证据：
#   ① feature 图：从每个发布根（scripts/test-only-features.txt 的 release-root）算 normal + build 依赖图
#      （cargo tree，不含 dev 依赖：发布构建只编 bin，不打开 dev 依赖），图里不许出现清单里只给测试用的 feature
#      （test-hooks、dev、fault-injection、fake-cli、e2e……）；并且每个工作区 crate 声明的每个 feature 都要在清单里登记，
#      新加 feature 不登记就红。这是 feature 依赖图，不是字符串。打包那次 cargo 调用本身有没有把测试 feature 统一进
#      进包的程序，由打包命令在出包时查（client/scripts/release-build.ts，读同一份清单）；
#      覆盖范围只到发布根：iwork-bench、iwork-fake-* 这类不发布的 crate，正式依赖里打开测试 feature 这里不报（它们本来就
#      只给测试、基准用；打包与发布构建都带 -p 指定发布根，不会把它们的 feature 统一进来）；
#   ② 产物：release 构建的 iwork-daemon、iwork-bridge-claude（以及 --artifacts 给的打包产物）里 grep 不到
#      开发字样（dev-trust、IWORK_DEV_、桥接的故障注入变量、假 CLI 的变量）；同一份源码开对应测试 feature
#      构建出来的必须 grep 得到、且被同一个扫描判红（阳性对照，证明扫描真能识别）；
#   ③ 行为：release 的 iwork-daemon 带 --dev-trust 启动，退出码非 0、stderr 有「未知参数」、不留 socket。
# ①、③ 才是证明，② 是冒烟。
#
# 用法：scripts/check-no-dev-trust.sh                    三步全跑（会做六次 release 构建）
#       scripts/check-no-dev-trust.sh --graph-only       只跑 ①（lint job 用，不构建）
#       scripts/check-no-dev-trust.sh --selftest         ① 的自测：在仓库副本里注入违规，必须判红（不碰工作区）
#       scripts/check-no-dev-trust.sh --artifacts F...   ② 额外检查这些文件（Tauri 打包产物）
# 退出码：0 通过；1 违规；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
command -v jq >/dev/null || { echo "check-no-dev-trust: 需要 jq" >&2; exit 2; }

GRAPH_ONLY=0
SELFTEST=0
ARTIFACTS=()
LIST="$ROOT/scripts/test-only-features.txt"
while [ $# -gt 0 ]; do
  case "$1" in
    --graph-only) GRAPH_ONLY=1; shift ;;
    --selftest) SELFTEST=1; shift ;;
    --artifacts) shift; while [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; do ARTIFACTS+=("$1"); shift; done ;;
    *) echo "check-no-dev-trust: 未知参数 ${1}" >&2; exit 2 ;;
  esac
done

# ① feature 图
# 清单里某一种的条目，一条一行「crate feature」或「crate bin」
list_of() {
  [ -f "$LIST" ] || { echo "check-no-dev-trust: 找不到清单 ${LIST}" >&2; exit 2; }
  awk -v kind="$1" -v file="$LIST" '
    /^[[:space:]]*(#|$)/ { next }
    NF != 3 || ($1 != "test-feature" && $1 != "release-feature" && $1 != "release-root") {
      printf "check-no-dev-trust: %s 第 %d 行写错了：%s\n", file, NR, $0 > "/dev/stderr"; bad = 1; exit
    }
    $1 == kind { print $2, $3 }
    END { exit bad ? 2 : 0 }' "$LIST" | tr -d '\r'
}
graph_check() {
  local tests releases roots meta declared unlisted crate bin found=""
  tests="$(list_of test-feature)" || exit 2
  releases="$(list_of release-feature)" || exit 2
  roots="$(list_of release-root)" || exit 2
  [ -n "$tests" ] || { echo "check-no-dev-trust: 清单里一条 test-feature 都没有" >&2; exit 2; }
  [ -n "$roots" ] || { echo "check-no-dev-trust: 清单里一条 release-root 都没有" >&2; exit 2; }
  echo "== ① feature 图（清单 scripts/test-only-features.txt）"
  # 每个工作区 crate 声明的每个 feature（default 除外）都要登记：新加的不登记，这里就不知道它该不该进发布
  meta="$(cargo metadata --format-version 1 --locked --no-deps)" || { echo "check-no-dev-trust: cargo metadata 失败" >&2; exit 2; }
  declared="$(printf '%s' "$meta" | jq -r '.packages[] | .name as $n | .features | keys[] | select(. != "default") | "\($n) \(.)"' | tr -d '\r' | sort -u)"
  unlisted="$(comm -23 <(printf '%s\n' "$declared") <(printf '%s\n%s\n' "$tests" "$releases" | sort -u))"
  if [ -n "$unlisted" ]; then
    echo "check-no-dev-trust: 下列 feature 没在 scripts/test-only-features.txt 登记（只给测试用的写 test-feature，发布可用的写 release-feature）：" >&2
    echo "$unlisted" >&2
    exit 1
  fi
  echo "通过：工作区声明的 $(printf '%s\n' "$declared" | grep -c .) 个 feature 都已登记"
  # 从每个发布根算 normal + build 依赖图（全部目标平台），逐个包看开了哪些 feature
  while read -r crate bin; do
    local tree pairs
    tree="$(cargo tree --locked -p "$crate" -e normal,build --target all --prefix none -f '{p}|{f}')" \
      || { echo "check-no-dev-trust: cargo tree -p ${crate} 失败" >&2; exit 2; }
    # 每行「名字 版本 (来源)|f1,f2」，重复的子树末尾带 (*)；拆成「名字 feature」
    pairs="$(printf '%s\n' "$tree" | tr -d '\r' | sed 's/ (\*)$//' | awk -F'|' '{ split($1, p, " "); n = split($2, f, ","); for (i = 1; i <= n; i++) if (f[i] != "") print p[1], f[i] }' | sort -u)"
    local hit
    hit="$(comm -12 <(printf '%s\n' "$pairs") <(printf '%s\n' "$tests" | sort -u))"
    if [ -n "$hit" ]; then
      found="${found}发布根 ${crate}（bin ${bin}）的依赖图里开了：$(printf '%s' "$hit" | tr '\n' ';')"$'\n'
    fi
  done <<< "$roots"
  if [ -n "$found" ]; then
    echo "check-no-dev-trust: 发布构建的依赖图里出现了只给测试用的 feature：" >&2
    printf '%s' "$found" >&2
    exit 1
  fi
  echo "通过：$(printf '%s\n' "$roots" | grep -c .) 个发布根的依赖图里没有 $(printf '%s\n' "$tests" | grep -c .) 个只给测试用的 feature 中的任何一个"
}

# ① 的自测：仓库副本（工作区里跟踪的与没忽略的新文件）里注入违规，必须判红并点名；未注入的副本必须通过
selftest() {
  local out code fails=0 tmp
  # 收尾在函数返回之后跑，用不了局部变量
  SELFTEST_TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-no-dev-trust-selftest.XXXXXX")"
  trap 'rm -rf "$SELFTEST_TMP"' EXIT
  tmp="$SELFTEST_TMP"
  git -C "$ROOT" ls-files -co --exclude-standard -z | (cd "$ROOT" && tar --null -T - -cf -) | tar -xf - -C "$tmp"
  run_copy() { set +e; out="$(bash "$tmp/scripts/check-no-dev-trust.sh" --graph-only 2>&1)"; code=$?; set -e; }
  expect_red() {
    local what="$1" needle="$2"
    run_copy
    if [ "$code" -eq 1 ] && printf '%s' "$out" | grep -qF -- "$needle"; then
      echo "自测通过：${what} → 判红（${needle}）"
    else
      echo "check-no-dev-trust 自测失败：${what} 应判红并含「${needle}」，实得退出码 ${code}：" >&2
      printf '%s\n' "$out" >&2
      fails=1
    fi
  }
  run_copy
  [ "$code" -eq 0 ] || { echo "check-no-dev-trust 自测失败：未注入的副本没通过：" >&2; printf '%s\n' "$out" >&2; exit 1; }
  echo "自测通过：未注入的副本通过"
  local m="$tmp/crates/iwork-daemon/Cargo.toml"
  cp "$m" "$m.orig"
  # 发布根的正式依赖里打开 test-hooks（dev-dependencies 里本来就有，写进 [dependencies] 才算进发布）
  sed -i.bak 's/^iwork-store\.workspace = true$/iwork-store = { workspace = true, features = ["test-hooks"] }/' "$m"
  grep -q 'iwork-store = { workspace = true, features = \["test-hooks"\] }' "$m" || { echo "check-no-dev-trust 自测：注入点没找到（iwork-daemon 的 iwork-store 依赖改写法了）" >&2; exit 2; }
  expect_red "iwork-daemon 正式依赖打开 iwork-store 的 test-hooks" "iwork-store test-hooks"
  cp "$m.orig" "$m"
  # 新声明一个 feature 不登记
  printf '\n[features]\nunlisted-selftest = []\n' >> "$tmp/crates/iwork-log/Cargo.toml"
  grep -q '^\[features\]' "$tmp/crates/iwork-log/Cargo.toml"
  expect_red "iwork-log 新加 feature 不登记" "iwork-log unlisted-selftest"
  [ "$fails" -eq 0 ] || exit 1
  echo "check-no-dev-trust: 自测全部通过"
}

if [ "$SELFTEST" -eq 1 ]; then
  selftest
  exit 0
fi
graph_check
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
