#!/usr/bin/env bash
# 发布包的装配（T04p 方案 4.2）：把后台程序放进 Tauri 打出来的包，按形态写清单或签名，在暂存目录里核对过才发布。
# 只由打包命令（`pnpm -C client tauri build` 的包装脚本）调用；路径全由调用方给，脚本自己不知道 target/ 在哪。
#
# 用法：
#   macOS：assemble-release.sh --app <Tauri 的 .app> --client <裸的客户端程序> --daemon <后台程序> \
#                              --check <check-install 程序> --dist <成品目录>
#   Linux：assemble-release.sh --client … --daemon … --check … --dist …
# 形态（macOS）：没设 IWORK_SIGN_IDENTITY = 形态 A（带清单、不封印）；设了 = 形态 B（证书签名、整包封印、不带清单）。
#   IWORK_SIGN_KEYCHAIN=<钥匙串文件> 可选，原样传给 codesign --keychain。脚本不建证书、不碰钥匙串的设置。
# 成品：<dist>/unsigned/iWork.app、<dist>/signed/iWork.app、<dist>/linux/iwork。
# 退出码：0 发布了；1 装配或核对没过（成品路径上原来的东西一个字节都没动）；2 用法或环境问题。
#
# 成品路径上只出现核对过的包（I6）：装配、核对都在 <dist>/.staging-<进程号>/ 里做，过了才改名到位：
#   旧成品改名成 .old-<进程号> → 暂存的改名到位 → 删 .old-*。断在哪一步，下一次开头都能收拾（见 recover）。
# 哪些是本工具的，只看 <dist>/.iwork-journal 里的记录（名字 + 设备号:inode，见 lib/owned-paths.sh）；每次改名之前先
# 记下改名之后的样子。同一个 <dist> 同时只能有一个装配在跑（<dist>/.iwork-assemble.lock）；收拾残留也在锁里做。
set -euo pipefail
# 空数组用 ${a[@]+"${a[@]}"} 展开：macOS 自带的 bash 3.2 在 set -u 下把空数组的 "${a[@]}" 当成未定义

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# 路径归属：删、写、改名都经过这一处（只碰自己建的、不走链接、不跨文件系统改名）
# shellcheck source=lib/owned-paths.sh
source "$ROOT/scripts/lib/owned-paths.sh"
owned_paths_fail() { die 1 "$*"; }
# 包里的程序。和 write-install-manifest.sh 的 BINARIES、iwork_daemon::install 的两个名字是同一组（R1 对拍）
PROGRAMS=(iwork-daemon iwork-client)
MANIFEST_FILE=iwork-manifest.json
DAEMON_SIGN_ID=dev.iwork.daemon

die() { echo "assemble-release: $2" >&2; exit "$1"; }
usage() { die 2 "用法：$0 [--app <Tauri 的 .app>] --client <程序> --daemon <程序> --check <check-install> --dist <成品目录>（--app 只在 macOS 上要）"; }

APP="" CLIENT="" DAEMON="" CHECK="" DIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP="${2:-}"; shift 2 || usage ;;
    --client) CLIENT="${2:-}"; shift 2 || usage ;;
    --daemon) DAEMON="${2:-}"; shift 2 || usage ;;
    --check) CHECK="${2:-}"; shift 2 || usage ;;
    --dist) DIST="${2:-}"; shift 2 || usage ;;
    *) usage ;;
  esac
done

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux) PLATFORM=linux ;;
  *) die 2 "M1 只装配 macOS 与 Linux 的包" ;;
esac

# 没有默认值：缺一个就是用法错，不去「默认位置」找（那里可能是上一次的东西，I7）
[ -n "$CLIENT" ] && [ -n "$DAEMON" ] && [ -n "$CHECK" ] && [ -n "$DIST" ] || usage
if [ "$PLATFORM" = macos ]; then [ -n "$APP" ] || usage; else [ -z "$APP" ] || usage; fi
for f in "$CLIENT" "$DAEMON" "$CHECK"; do [ -f "$f" ] || die 1 "找不到 ${f}"; done
[ "$PLATFORM" = linux ] || [ -d "$APP" ] || die 1 "找不到 Tauri 的包：${APP}"

SIGN_ID="${IWORK_SIGN_IDENTITY-}"
SIGNED=false
[ -n "${IWORK_SIGN_IDENTITY+x}" ] && SIGNED=true
if [ "$PLATFORM" = linux ]; then
  $SIGNED && die 1 "这个平台没有签名的形态（设了 IWORK_SIGN_IDENTITY）"
  FORM=linux; NAME=iwork
elif $SIGNED; then
  FORM=signed; NAME=iWork.app
else
  FORM=unsigned; NAME=iWork.app
fi

# 成品目录：不存在就新建（连同记录）；已有的必须带本工具的记录，或是空的真目录；链接、别人的目录一律不碰
journal_root "$DIST"
DIST="$(canonical "$DIST")"
JOURNAL_ROOT=$DIST
OWNED=("$DIST")

# 同一个 <dist> 一次只许一个装配，锁由跑这个脚本的进程自己拿着：fd 9 开着锁文件，perl 在这个 fd 的打开文件上加
# flock（不阻塞）。flock 跟着打开文件走，不跟着进程走：perl 退出后锁还在，直到这个脚本和它起的子进程（核对、签名…）
# 全都关掉 fd 9 才放。所以只杀这个脚本、核对还活着、还在用暂存时，第二个装配拿不到锁，不会去收拾它（审查 3/F-2）。
# 两个平台同一个办法；perl 两边都自带，不另加依赖（macOS 没有 flock 命令）。收拾残留也在锁里做
LOCK="$DIST/.iwork-assemble.lock"
if _exists "$LOCK"; then [ -f "$LOCK" ] && [ ! -L "$LOCK" ] || die 1 "${LOCK} 不是普通文件，不用它"; fi
exec 9>>"$LOCK"
perl -MFcntl=:flock -e 'open(my $fh, ">&=", 9) or exit 2; flock($fh, LOCK_EX | LOCK_NB) or exit 75; exit 0' || {
  rc=$?
  [ "$rc" = 75 ] && die 1 "另一个装配正在用 ${DIST}，这一次不做"
  die 1 "拿不了锁 ${LOCK}（perl 退出 ${rc}）"
}

# 上一次断在半路留下的：只动记录里有、身份也对得上的。没验完的暂存删掉；断在两次改名之间的，把旧成品改回去；
# 新成品已到位的，删留底。同名却不在记录里的（别人放的备份、别的东西）一律不动，停下报错
recover() {
  local entry f product old
  for entry in "$DIST"/.staging-*; do
    _exists "$entry" || continue
    journal_knows "$entry" || die 1 "${entry} 不在装配的记录里（不是装配建的，或已经被换过），不删；确认没用后手工处理"
    owned_rm "$entry"; journal_del "$entry"
  done
  for f in unsigned signed linux; do
    _exists "$DIST/$f" || continue
    journal_knows "$DIST/$f" && real_dir "$DIST/$f" || die 1 "${DIST}/${f} 不在装配的记录里，不碰它"
    product="$DIST/$f/iWork.app"
    [ "$f" = linux ] && product="$DIST/$f/iwork"
    for old in "$DIST/$f"/.old-*; do
      _exists "$old" || continue
      journal_knows "$old" && real_dir "$old" || die 1 "${old} 不在装配的记录里（不是装配留的底），不碰它"
      if _exists "$product"; then
        owned_rm "$old"; journal_del "$old"
      else
        journal_add "$product" "$(_ident "$old")"
        owned_mv "$old" "$product"; journal_del "$old"
      fi
    done
  done
}
recover

STAGE="$DIST/.staging-$$"
owned_mkdir "$STAGE"
journal_add "$STAGE"
# 失败、被中断：只删自己的暂存；成品路径没被动过。被 kill -9 时删不到，下一次开头的 recover 按记录删
trap 'owned_rm "$STAGE"; journal_del "$STAGE"' EXIT

codesign_args=()
EXPECT_LEAF=()
if [ "$FORM" = signed ]; then
  # 第 0 步：身份是 ad-hoc（-）或空 → 这样的包没有证书也没有清单，两端互不相认（核对的第 2 件也会拦，这里报得清楚）
  { [ -z "$SIGN_ID" ] || [ "$SIGN_ID" = - ]; } \
    && die 1 "IWORK_SIGN_IDENTITY 是「${SIGN_ID}」：- 是 ad-hoc 签名，空值没有身份；这样的包没有证书也没有清单，两端互不相认"
  [ -n "${IWORK_SIGN_KEYCHAIN:-}" ] && codesign_args=(--keychain "$IWORK_SIGN_KEYCHAIN")
  find_args=(-a -c "$SIGN_ID" -Z)
  [ -n "${IWORK_SIGN_KEYCHAIN:-}" ] && find_args+=("$IWORK_SIGN_KEYCHAIN")
  found=$(security find-certificate "${find_args[@]}" 2>/dev/null || true)
  leaves=$(printf '%s\n' "$found" | sed -n 's/^SHA-256 hash: //p' | tr 'A-F' 'a-f' | sort -u)
  count=$(printf '%s' "$leaves" | grep -c . || true)
  [ "$count" = 1 ] || die 1 "按名字「${SIGN_ID}」在钥匙串里找到 ${count} 张证书，要恰好一张"
  EXPECT_LEAF=(--expect-leaf "$leaves")
  # 签名时用这张证书的 SHA-1 指定身份：名字只用来找到恰好这一张，签的就是核对时预期的那张叶证书
  SIGN_HASH=$(printf '%s\n' "$found" | sed -n 's/^SHA-1 hash: //p' | head -1)
  [ -n "$SIGN_HASH" ] || die 1 "取不到证书「${SIGN_ID}」的 SHA-1"
fi

if [ "$PLATFORM" = macos ]; then
  PKG="$STAGE/iWork.app"
  BIN="$PKG/Contents/MacOS"
  ditto "$APP" "$PKG"                                                                       # 1
  # 复制出来的包里不许有链接：ditto 原样保留链接，后面往 Contents/MacOS 里放后台、写清单会顺着链接写到包外
  no_links_in "$PKG"
  [ -e "$PKG/Contents/_CodeSignature" ] && die 1 "Tauri 给包签了名，装配不接着做（签名只在装配里做）"    # 2
  cmp -s "$BIN/iwork-client" "$CLIENT" \
    || die 1 "包里的主程序和 ${CLIENT} 不是同一份：打包器改过它，清单算的就不是成品"               # 3
  dest_free "$BIN/iwork-daemon"
  cp "$DAEMON" "$BIN/iwork-daemon"                                                           # 4
  chmod 755 "$BIN/iwork-daemon"
  if [ "$FORM" = unsigned ]; then
    dest_free "$BIN/$MANIFEST_FILE"
    "$ROOT/scripts/write-install-manifest.sh" "$BIN" >/dev/null                               # 5
  else
    [ -e "$BIN/$MANIFEST_FILE" ] && die 1 "签名的包里不能有清单：$BIN/$MANIFEST_FILE"            # 5
    codesign -f -s "$SIGN_HASH" ${codesign_args[@]+"${codesign_args[@]}"} -i "$DAEMON_SIGN_ID" "$BIN/iwork-daemon"
    codesign -f -s "$SIGN_HASH" ${codesign_args[@]+"${codesign_args[@]}"} "$PKG"                                    # 6
  fi
else
  PKG="$STAGE/iwork"
  BIN="$PKG"
  owned_mkdir "$PKG"
  cp "$CLIENT" "$PKG/iwork-client"
  cp "$DAEMON" "$PKG/iwork-daemon"
  chmod 755 "$PKG/iwork-client" "$PKG/iwork-daemon"
  "$ROOT/scripts/write-install-manifest.sh" "$PKG" >/dev/null
fi

# 核对（4.4）：恰好一种形态、能互认、（有证书时）是预期的证书
"$CHECK" "$BIN" ${EXPECT_LEAF[@]+"${EXPECT_LEAF[@]}"} || die 1 "核对没过，不发布"
if [ "$FORM" = signed ]; then
  codesign --verify --deep --strict "$PKG" || die 1 "整包验签不过，不发布"
fi

# 发布：两次改名。形态目录、成品位置上原来的东西都必须在记录里（上一次发布的成品）；每次改名之前先记下改名之后
# 的样子（改名不改 inode），断在任何一步，recover 都按记录收拾
if _exists "$DIST/$FORM"; then
  journal_knows "$DIST/$FORM" && real_dir "$DIST/$FORM" || die 1 "${DIST}/${FORM} 不在装配的记录里，不碰它"
else
  owned_mkdir "$DIST/$FORM"; journal_add "$DIST/$FORM"
fi
PRODUCT="$DIST/$FORM/$NAME"
OLD="$DIST/$FORM/.old-$$"
own "$PKG"
if _exists "$PRODUCT"; then
  journal_knows "$PRODUCT" && real_dir "$PRODUCT" || die 1 "${PRODUCT} 不在装配的记录里（不是装配发布的），不碰它"
  journal_add "$OLD" "$(_ident "$PRODUCT")"
  owned_mv "$PRODUCT" "$OLD"
fi
journal_add "$PRODUCT" "$(_ident "$PKG")"
owned_mv "$PKG" "$PRODUCT"
if _exists "$OLD"; then owned_rm "$OLD"; journal_del "$OLD"; fi
echo "assemble-release: 已发布 ${PRODUCT}"
