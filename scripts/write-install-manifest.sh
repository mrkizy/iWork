#!/usr/bin/env bash
# 生成安装清单 iwork-manifest.json（T03 方案 2.4a）：登记这一版 iwork-daemon、iwork-client 的映像身份，
# 放在两个二进制旁边。只给无证书构建用；后台服务与客户端启动时读一次（iwork-ipc 的 peer::Manifest）。
#
# 用法：scripts/write-install-manifest.sh <安装目录> [app_version]
#   app_version 不给就取 workspace 的版本（根 Cargo.toml 的 [workspace.package] version）。
# 每个二进制登记 sha256:<全文哈希>；macOS 上二进制带签名（含 ad-hoc）时再登记 cdhash:<代码目录哈希>。
# 退出码：0 写好；1 缺二进制或取哈希失败；2 用法或环境问题。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARIES=(iwork-daemon iwork-client)
MANIFEST_FILE=iwork-manifest.json

die() { echo "write-install-manifest: $2" >&2; exit "$1"; }

[ $# -ge 1 ] && [ $# -le 2 ] || die 2 "用法：$0 <安装目录> [app_version]"
DIR="$1"
[ -d "$DIR" ] || die 2 "安装目录不存在：${DIR}"
command -v jq >/dev/null || die 2 "需要 jq"

workspace_version() {
  # 只读 [workspace.package] 段里的 version，别的段里的 version 不算。
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") }
    in_pkg && /^version[[:space:]]*=/ { gsub(/^[^"]*"|"[^"]*$/, ""); print; exit }
  ' "$ROOT/Cargo.toml"
}
APP_VERSION="${2:-$(workspace_version)}"
[ -n "$APP_VERSION" ] || die 2 "取不到 workspace 版本（${ROOT}/Cargo.toml 的 [workspace.package] version），请用第二个参数给出"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) EXE_SUFFIX=.exe ;;
  *) EXE_SUFFIX= ;;
esac
IS_MACOS=false
[ "$(uname -s)" = Darwin ] && IS_MACOS=true

if command -v sha256sum >/dev/null; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die 2 "需要 sha256sum 或 shasum"
fi

# 未签名的二进制 codesign 报错退出，这时不登记 cdhash，输出为空。漏登 cdhash 只会让对端连不上，
# 不会放进来不该放的（失败即关闭）。清单解析只认 20 字节的 cdhash。
cdhash_of() {
  { codesign -dvvv "$1" 2>&1 || true; } | sed -n 's/^CDHash=\([0-9a-f]\{40\}\)$/\1/p'
}

missing=()
for name in "${BINARIES[@]}"; do
  [ -f "$DIR/${name}${EXE_SUFFIX}" ] || missing+=("$DIR/${name}${EXE_SUFFIX}")
done
[ ${#missing[@]} -eq 0 ] || die 1 "缺二进制：${missing[*]}"

binaries_json='{}'
for name in "${BINARIES[@]}"; do
  file="$DIR/${name}${EXE_SUFFIX}"
  sha="$(sha256_of "$file")"
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die 1 "算 ${file} 的 sha256 失败：${sha}"
  entries=("sha256:${sha}")
  if $IS_MACOS; then
    cd_hash="$(cdhash_of "$file")"
    [ -z "$cd_hash" ] || entries+=("cdhash:${cd_hash}")
  fi
  binaries_json="$(jq --arg name "$name" '.[$name] = $ARGS.positional' \
    --args "${entries[@]}" <<<"$binaries_json")"
done

OUT="$DIR/$MANIFEST_FILE"
jq -n --arg app_version "$APP_VERSION" --argjson binaries "$binaries_json" \
  '{manifest_version: 1, app_version: $app_version, binaries: $binaries}' >"$OUT.tmp"
mv "$OUT.tmp" "$OUT"
echo "write-install-manifest: 已写 ${OUT}"
