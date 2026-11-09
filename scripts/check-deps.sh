#!/usr/bin/env bash
# 检查 crate 依赖方向（AGENTS.md「依赖只向下」）。真值是 scripts/deps-policy.toml，规则见 docs/adr/0005。
#
# 用法：
#   scripts/check-deps.sh                     读当前工作区的 cargo metadata 检查
#   scripts/check-deps.sh --metadata FILE     检查一份 cargo metadata JSON（自测用）
#   scripts/check-deps.sh --policy FILE       换一份策略文件
#   scripts/check-deps.sh --selftest          自测：合法图必须通过，注入的各类违规必须失败
#
# 退出码：0 通过；1 有违规；2 环境或策略文件有问题（缺工具、策略写错）。CI 把非 0 都当失败。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
POLICY="$SCRIPT_DIR/deps-policy.toml"
METADATA=""
MODE="check"

die_env() { echo "check-deps: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --metadata) METADATA="${2:?--metadata 需要文件路径}"; shift 2 ;;
    --policy) POLICY="${2:?--policy 需要文件路径}"; shift 2 ;;
    --selftest) MODE="selftest"; shift ;;
    *) die_env "未知参数：$1" ;;
  esac
done

command -v jq >/dev/null 2>&1 || die_env "缺 jq，请先安装 jq（不会跳过检查）"
command -v cargo >/dev/null 2>&1 || die_env "缺 cargo"

# Windows 的 jq 输出可能带 \r，统一去掉
jqx() { jq "$@" | tr -d '\r'; }

parse_policy() {
  jqx -R -s -f "$SCRIPT_DIR/deps-policy.jq" "$1" 2>&1 || {
    echo "check-deps: 策略文件 $1 解析失败" >&2; return 2; }
}

workspace_metadata() {
  (cd "$ROOT" && cargo metadata --format-version 1 --locked)
}

# 检查一份 metadata；打印违规，返回 0/1/2
run_check() {
  local metadata_file="$1" policy_file="$2" policy_json out
  policy_json="$(parse_policy "$policy_file")" || { echo "$policy_json" >&2; return 2; }
  out="$(jqx -r --argjson policy "$policy_json" -f "$SCRIPT_DIR/check-deps.jq" "$metadata_file")" \
    || { echo "check-deps: jq 判定失败" >&2; return 2; }
  if [ -z "$out" ]; then
    echo "check-deps: 通过（策略 $(basename "$policy_file")）"
    return 0
  fi
  echo "$out"
  if grep -q '^POLICY_ERROR:' <<<"$out"; then return 2; fi
  return 1
}

selftest() {
  local tmp snapshot fails=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/check-deps-selftest.XXXXXX")"
  trap 'rm -rf "$tmp"' RETURN
  snapshot="$tmp/metadata.json"
  workspace_metadata > "$snapshot"

  # 往快照里加一个假的外部包（只需要 id 与名字）
  add_fake_pkg='def add_pkg($id; $name):
      .packages += [{ id: $id, name: $name, dependencies: [] }]
      | .resolve.nodes += [{ id: $id, deps: [] }];
    def id_of($name): [.packages[] | select(.name == $name) | .id][0]
      // error("快照里没有包 \($name)");
    def add_edge($from; $to; $kind):
      (.resolve.nodes[] | select(.id == $from) | .deps) += [{ pkg: $to, name: "x", dep_kinds: [{ kind: $kind, target: null }] }];
    def add_direct($from; $name; $kind):
      (.packages[] | select(.id == $from) | .dependencies) += [{ name: $name, kind: $kind }];'

  # 用例：名字、jq 改写、期望退出码、输出里必须出现的文字（可多条，用 | 分隔）
  run_case() {
    local name="$1" mutation="$2" want_code="$3" want_text="$4" file="$tmp/case.json" out code
    if ! jqx "$add_fake_pkg $mutation" "$snapshot" > "$file"; then
      echo "selftest 失败：${name}；构造用例的 jq 改写出错（快照里缺了用例依赖的包？）"
      fails=$((fails + 1)); return 0
    fi
    set +e
    out="$(run_check "$file" "$POLICY" 2>&1)"
    code=$?
    set -e
    local ok=1
    [ "$code" -eq "$want_code" ] || ok=0
    if [ -n "$want_text" ]; then
      local IFS='|' t
      for t in $want_text; do grep -qF -- "$t" <<<"$out" || ok=0; done
    fi
    if [ "$ok" -eq 1 ]; then
      echo "selftest 通过：${name}（退出码 ${code}）"
    else
      echo "selftest 失败：${name}；期望退出码 ${want_code}、包含「${want_text}」，实际退出码 ${code}，输出："
      echo "$out" | sed 's/^/    /'
      fails=$((fails + 1))
    fi
  }

  # 合法图：真实工作区里 thiserror 的宏链、domain 的 proptest、ports 经 domain 的 ulid、client 经 client-core 的 ipc 都在
  run_case "合法依赖图（原样快照）" '.' 0 "通过"

  run_case "间接违规（外部）：ulid 带进 tokio" \
    'add_pkg("fake-tokio"; "tokio") | add_edge(id_of("ulid"); "fake-tokio"; null)' \
    1 "iwork-domain 经 ulid → tokio|规则：传递禁用"

  run_case "间接违规（内部）：iwork-ports 依赖 iwork-store" \
    'add_edge(id_of("iwork-ports"); id_of("iwork-store"); null)' \
    1 "crate iwork-ports：iwork-ports → iwork-store|crate iwork-core：iwork-core 经 iwork-ports → iwork-store|规则：工作区内部"

  run_case "dev 依赖违规：iwork-domain 的 dev 依赖 tokio" \
    'add_pkg("fake-tokio"; "tokio") | add_edge(id_of("iwork-domain"); "fake-tokio"; "dev")
     | add_direct(id_of("iwork-domain"); "tokio"; "dev")' \
    1 "crate iwork-domain：iwork-domain → tokio（规则：传递禁用，范围 normal_build_dev）"

  run_case "直接依赖越白名单：iwork-core 直接依赖 rand" \
    'add_pkg("fake-rand"; "rand") | add_edge(id_of("iwork-core"); "fake-rand"; null)
     | add_direct(id_of("iwork-core"); "rand"; null)' \
    1 "crate iwork-core：直接依赖了 rand（normal）|规则：直接许可"

  run_case "对照：豁免框架带进来的依赖不算违规（iwork-ipc 经 tokio → mio）" \
    'add_pkg("fake-tokio"; "tokio") | add_pkg("fake-mio"; "mio")
     | add_edge(id_of("iwork-ipc"); "fake-tokio"; null) | add_edge("fake-tokio"; "fake-mio"; null)' \
    0 "通过"

  run_case "策略缺节：工作区多一个 crate" \
    '.packages += [{ id: "fake-ws", name: "iwork-unlisted", dependencies: [] }]
     | .resolve.nodes += [{ id: "fake-ws", deps: [] }] | .workspace_members += ["fake-ws"]' \
    2 "iwork-unlisted 在策略里没有"

  # 策略文件写错：未知键必须报错退出 2，不能被当成「没有限制」
  local bad_policy="$tmp/bad-policy.toml"
  sed 's/^direct_dev = \["proptest"\]/direct_devv = ["proptest"]/' "$POLICY" > "$bad_policy"
  set +e
  local out code
  out="$(run_check "$snapshot" "$bad_policy" 2>&1)"; code=$?
  set -e
  if [ "$code" -eq 2 ] && grep -qF "未知的键：direct_devv" <<<"$out"; then
    echo "selftest 通过：策略文件有未知键（退出码 2）"
  else
    echo "selftest 失败：策略文件有未知键；期望退出码 2，实际 ${code}，输出："; echo "$out" | sed 's/^/    /'
    fails=$((fails + 1))
  fi

  if [ "$fails" -gt 0 ]; then
    echo "check-deps selftest：$fails 个用例失败"
    return 1
  fi
  echo "check-deps selftest：全部通过"
}

if [ "$MODE" = "selftest" ]; then
  selftest
  exit $?
fi

if [ -z "$METADATA" ]; then
  METADATA="$(mktemp "${TMPDIR:-/tmp}/check-deps-metadata.XXXXXX")"
  trap 'rm -f "$METADATA"' EXIT
  workspace_metadata > "$METADATA"
fi
set +e
run_check "$METADATA" "$POLICY"
exit $?
