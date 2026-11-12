# 路径归属（T04p 审查 1/F-1、F-2、F-7，2/F-1）：发布装配与真包演练只删、只改、只往里写它们自己建的路径。
# 由 scripts/assemble-release.sh、scripts/drill-release-bundle.sh 用 `source` 引入；macOS 的 bash 3.2 也能用。
#
# 规则（每个会删、写、改名、判归属的地方都经过这里，不各写各的）：
#   - 只独占新建：要放东西的位置上已经有东西（目录、文件、链接）→ 拒绝，不碰它。
#   - 这一次运行建的路径记进 OWNED；删、改名只对 OWNED 里的路径做。
#   - 跨次运行（上一次断在半路留下的）只认记录：受管根目录里独占新建的记录文件（journal_*），一条记下一个路径的
#     名字和身份（设备号:inode）。恢复时只动「最后一条是 add、并且身份和现在一致」的路径；同名但不在记录里、或身份
#     对不上的一律不动、报错。目录里有什么标记文件都不算证明（审查 2/F-1）。
#   - 链接：要删、要改名、要往里放东西的那个路径本身是链接，或它所在的那一级目录是链接 → 拒绝。只查这两级，
#     不是逐级 O_NOFOLLOW：更上面的祖先目录是链接时，照样会落到链接目标里（独占新建仍然成立）。
#   - 不跨文件系统改名：两端不在同一个文件系统上时 mv 会退化成「复制再删」，中途断了两边都不完整 → 拒绝。
#   - 判归属看规范化后的目录边界，不看子串。
# 出错时调用 owned_paths_fail（调用方可以先定义它；没定义就打一行到 stderr 并以 1 退出）。

OWNED=()
JOURNAL_NAME=.iwork-journal
JOURNAL_ROOT=""

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

# 路径本身的身份（设备号:inode；不跟随链接）
_ident() {
  if [ "$(uname -s)" = Darwin ]; then stat -f '%d:%i' "$1"; else stat -c '%d:%i' "$1"; fi
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

# ---------------------------------------------------------------- 跨次运行的记录
# 记录文件 <根>/.iwork-journal，一行「add|del<TAB><相对根的名字><TAB><设备号:inode>」，只追加。
# 认领受管根：不存在 → 独占新建目录与记录；是空的真目录 → 独占新建记录；是带记录（普通文件、不是链接）的真目录
# → 认；别的（链接、文件、没有记录的非空目录）一律拒绝。认了的根登记进 OWNED。
journal_root() {
  local root=$1
  if ! _exists "$root"; then
    owned_mkdir "$root"
  else
    real_dir "$root" || _owned_fail "${root} 不是真目录（是链接或文件），不碰它"
    own "$root"
  fi
  local j="$root/$JOURNAL_NAME"
  if _exists "$j"; then
    [ -f "$j" ] && [ ! -L "$j" ] || _owned_fail "${j} 不是普通文件，不认"
  else
    [ -z "$(ls -A "$root")" ] || _owned_fail "${root} 不是空的、也没有本工具的记录（不是这个脚本建的），不碰它"
    ( set -C; : > "$j" ) 2>/dev/null || _owned_fail "建不了记录 ${j}"
  fi
  JOURNAL_ROOT=$root
}

_journal_name() {
  case "$1" in
    "$JOURNAL_ROOT"/*) printf '%s\n' "${1#"$JOURNAL_ROOT"/}" ;;
    *) _owned_fail "${1} 不在受管根 ${JOURNAL_ROOT} 里，不记" ;;
  esac
}

# 记一条：<路径> 现在（或改名之后）是本工具的，身份是 <设备号:inode>（不给就取路径现在的身份）
journal_add() {
  local name ident=${2:-}
  name=$(_journal_name "$1")
  [ -n "$ident" ] || ident=$(_ident "$1")
  printf 'add\t%s\t%s\n' "$name" "$ident" >> "$JOURNAL_ROOT/$JOURNAL_NAME"
}

journal_del() {
  local name
  name=$(_journal_name "$1")
  printf 'del\t%s\t-\n' "$name" >> "$JOURNAL_ROOT/$JOURNAL_NAME"
}

# <路径> 是本工具记下的：最后一条记录是 add，身份和路径现在的一致；是的话登记进 OWNED
journal_knows() {
  local name last now
  _exists "$1" || return 1
  name=$(_journal_name "$1")
  last=$(awk -F '\t' -v n="$name" '$2 == n { a = $1; i = $3 } END { if (a == "add") print i }' \
    "$JOURNAL_ROOT/$JOURNAL_NAME")
  now=$(_ident "$1")
  [ -n "$last" ] && [ "$last" = "$now" ] || return 1
  own "$1"
}
