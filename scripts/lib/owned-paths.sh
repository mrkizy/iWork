# 路径归属（T04p 审查 1/F-1、F-2、F-7）：发布装配与真包演练只删、只改、只往里写它们自己建的路径。
# 由 scripts/assemble-release.sh、scripts/drill-release-bundle.sh 用 `source` 引入；macOS 的 bash 3.2 也能用。
#
# 规则（每个会删、写、改名、判归属的地方都经过这里，不各写各的）：
#   - 只独占新建：要放东西的位置上已经有不认识的东西（目录、文件、链接）→ 拒绝，不碰它。
#   - 登记归属：自己建的路径记进 OWNED；删、改名只对登记过的路径做。跨次运行的残留靠目录里的归属标记认
#     （见 mark_owned_dir / is_marked_dir），没有标记的一律不认。
#   - 不走链接：路径本身是链接、或要操作的目录是链接 → 拒绝（rm、cp、ditto 会顺着链接写到别处）。
#   - 不跨文件系统改名：两端不在同一个文件系统上时 mv 会退化成「复制再删」，中途断了两边都不完整 → 拒绝。
#   - 判归属看规范化后的目录边界，不看子串。
# 出错时调用 owned_paths_fail（调用方可以先定义它；没定义就打一行到 stderr 并以 1 退出）。

OWNED=()
OWNED_MARK=.iwork-owned

_owned_fail() {
  if declare -F owned_paths_fail >/dev/null; then owned_paths_fail "$*"; fi
  echo "owned-paths: $*" >&2
  exit 1
}

# 路径存在（含悬空链接）
_exists() { [ -e "$1" ] || [ -L "$1" ]; }

# 是目录且不是链接
real_dir() { [ -d "$1" ] && [ ! -L "$1" ]; }

# 所在文件系统的设备号
_dev_of() {
  if [ "$(uname -s)" = Darwin ]; then stat -f %d "$1"; else stat -c %d "$1"; fi
}

# 规范化：目录取 cd -P 的真实路径；文件取「所在目录的真实路径/文件名」。路径或所在目录不存在 → 空
canonical() {
  local p=$1
  if [ -d "$p" ] && [ ! -L "$p" ]; then (cd -P "$p" 2>/dev/null && pwd -P); return; fi
  local dir base
  dir=$(dirname "$p"); base=$(basename "$p")
  dir=$( (cd -P "$dir" 2>/dev/null && pwd -P) ) || return 0
  [ -n "$dir" ] && printf '%s/%s\n' "${dir%/}" "$base"
}

# <路径> 规范化之后在 <目录> 之下（按目录边界：/a/b 不在 /a/bc 之下）
under_dir() {
  local p r
  p=$(canonical "$1"); r=$(canonical "$2")
  [ -n "$p" ] && [ -n "$r" ] || return 1
  case "$p" in "${r%/}"/*) return 0 ;; *) return 1 ;; esac
}

is_owned() {
  local o
  for o in ${OWNED[@]+"${OWNED[@]}"}; do [ "$o" = "$1" ] && return 0; done
  return 1
}
_disown() {
  local o keep=()
  for o in ${OWNED[@]+"${OWNED[@]}"}; do [ "$o" = "$1" ] || keep+=("$o"); done
  OWNED=(${keep[@]+"${keep[@]}"})
}
own() { is_owned "$1" || OWNED+=("$1"); }

# 目标位置空着，可以往那里新建：自己不存在，所在目录是真目录（不是链接）
dest_free() {
  _exists "$1" && _owned_fail "${1} 已经有东西（不是这一次建的），不碰它"
  real_dir "$(dirname "$1")" || _owned_fail "$(dirname "$1") 不是真目录（不存在或是链接），不往里放东西"
}

# 独占新建一个目录并登记
owned_mkdir() {
  dest_free "$1"
  mkdir "$1" || _owned_fail "建不了 ${1}"
  own "$1"
}

# 目录里的归属标记：跨次运行认「这是我上一次建的」只靠它
mark_owned_dir() { : > "$1/$OWNED_MARK"; }
is_marked_dir() { real_dir "$1" && [ -f "$1/$OWNED_MARK" ] && [ ! -L "$1/$OWNED_MARK" ]; }

# 认领一个目录：不存在就独占新建并打标记；已经是带标记的真目录就登记；是空的真目录就打标记登记；别的一律拒绝
claim_dir() {
  if ! _exists "$1"; then
    owned_mkdir "$1"; mark_owned_dir "$1"; return
  fi
  real_dir "$1" || _owned_fail "${1} 不是真目录（是链接或文件），不碰它"
  if is_marked_dir "$1"; then own "$1"; return; fi
  [ -z "$(ls -A "$1")" ] || _owned_fail "${1} 不是空的、也没有归属标记（不是这个脚本建的），不碰它"
  mark_owned_dir "$1"; own "$1"
}

# 只删登记过的路径。链接只删链接本身（rm 不跟随）
owned_rm() {
  _exists "$1" || { _disown "$1"; return 0; }
  is_owned "$1" || _owned_fail "${1} 不是这一次建的，不删"
  rm -rf "$1"
  _disown "$1"
}

# 改名：源是登记过的，目标空着，两端在同一个文件系统上
owned_mv() {
  local src=$1 dst=$2
  is_owned "$src" || _owned_fail "${src} 不是这一次建的，不挪"
  [ ! -L "$src" ] || _owned_fail "${src} 是链接，不挪"
  dest_free "$dst"
  [ "$(_dev_of "$src")" = "$(_dev_of "$(dirname "$dst")")" ] \
    || _owned_fail "${src} 与 ${dst} 不在同一个文件系统上，改名会退化成复制"
  mv "$src" "$dst" || _owned_fail "改名失败：${src} → ${dst}"
  _disown "$src"
  own "$dst"
}

# 目录里没有任何链接（cp、chmod、写文件时不会顺着链接写到别处）
no_links_in() {
  local found
  found=$(find "$1" -type l -print -quit)
  [ -z "$found" ] || _owned_fail "${1} 里有链接：${found}，不往里写"
}
