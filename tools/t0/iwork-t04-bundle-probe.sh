#!/usr/bin/env bash
# iWork T04 N5（方案第 8 节）：真实的 `tauri build` 包上，完成依据（安装清单 / 整包签名）与签名能不能同时成立。只在 macOS 跑。
#
# 先在仓库根目录构建（套重测试锁）：
#   pnpm -C client tauri build --bundles app && cargo build --release -p iwork-daemon
# 再跑本脚本。它只读 target/release 下的产物，所有改动都在临时目录的副本上做。
#
# 不碰登录钥匙串：自签证书放临时钥匙串（不加进搜索列表），跑完删除。不装任何 LaunchAgent。
# 结果：tools/t0/out/t04-bundle/（summary.txt 是结论；其余是各步原始输出）。
#
# iwork-daemon 现在还是空壳（T04 第 4 步才填），客户端也还不会连后台（第 8 步、T09），
# 所以「两端握手」与「运行映像身份」用探针冒充两个角色，放在真实包的同一位置、按同样的方式签名或登记清单：
#   ipcprobe（tools/t0/iwork-t04-ipcprobe.rs，依赖 iwork-ipc、不开 dev feature）——握手；
#   imgprobe（tools/t0/iwork-t04-imgprobe.c）——运行映像 cdhash、静态 cdhash、整包验签。
# 客户端主程序 iwork-client 与资源、Info.plist、图标都是 tauri build 的原样产物。
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-bundle"
BUILT_APP="$ROOT/target/release/bundle/macos/iWork.app"
BUILT_CLIENT="$ROOT/target/release/iwork-client"
BUILT_DAEMON="$ROOT/target/release/iwork-daemon"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-bundle.XXXXXX")" && pwd -P)  # 规范化（/private/var/…），日志里的路径才脱得掉
KC="$WORK/t04.keychain-db"
# 临时钥匙串：建好后关掉自动上锁（默认空闲 5 分钟、睡眠就锁，之后 codesign 会弹框找人要密码）；
# 每次签名前再解锁一次。整个过程不该出现要人输密码的框。
kc_codesign() { security unlock-keychain -p t "$KC"; codesign --keychain "$KC" "$@"; }
SIGNER="iWork T04 Signer"
CHILDREN=()

cleanup() {
  for pid in "${CHILDREN[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
  security delete-keychain "$KC" 2>/dev/null || true
  # 有些原始输出是直接重定向写的，最后统一脱敏一遍
  [ -d "$OUT" ] && find "$OUT" -type f -exec sed -i '' -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g" {} +
  rm -rf "$WORK"
}
trap cleanup EXIT  # bash 3.2 收到 TERM、INT 也会跑 EXIT trap（实测），被 timeout 掐断时照样清理

for f in "$BUILT_APP" "$BUILT_CLIENT" "$BUILT_DAEMON"; do
  [ -e "$f" ] || { echo "缺构建产物：${f}（先跑文件头里的构建命令）" >&2; exit 2; }
done

rm -rf "$OUT"; mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
note() { echo "$*" | tee -a "$SUMMARY"; }
# 输出里把临时目录与家目录换成占位符
redact() { sed -e "s#/private$WORK#<work>#g" -e "s#$WORK#<work>#g" -e "s#$HOME#<home>#g"; }
# check <编号> <条件说明> <实际> <期望>：实际等于期望记 PASS，否则 FAIL
FAILS=0
check() {
  if [ "$3" = "$4" ]; then note "  PASS $1 $2：$3"; else note "  FAIL $1 $2：实际 $3，期望 $4"; FAILS=$((FAILS + 1)); fi
}

note "# T04 N5 真实包实测  $(date '+%F %T')"
note "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)；loadavg $(sysctl -n vm.loadavg)"

# ---------------------------------------------------------------- 探针
mkdir -p "$WORK/bin" "$WORK/ipcprobe/src"
cc -framework Security -framework CoreFoundation -DMARK='"1"' "$ROOT/tools/t0/iwork-t04-imgprobe.c" -o "$WORK/bin/imgprobe1"
cc -framework Security -framework CoreFoundation -DMARK='"2"' "$ROOT/tools/t0/iwork-t04-imgprobe.c" -o "$WORK/bin/imgprobe2"
cc -arch arm64 -arch x86_64 -framework Security -framework CoreFoundation -DMARK='"u"' "$ROOT/tools/t0/iwork-t04-imgprobe.c" -o "$WORK/bin/imgprobe_universal"
IMG="$WORK/bin/imgprobe1"
cat > "$WORK/ipcprobe/Cargo.toml" <<EOF
[package]
name = "ipcprobe"
version = "0.1.0"
edition = "2024"
[workspace]
[dependencies]
iwork-ipc = { path = "$ROOT/crates/iwork-ipc" }
iwork-proto = { path = "$ROOT/crates/iwork-proto" }
tokio = { version = "1.54", features = ["rt", "macros", "time"] }
tokio-util = "0.7"
tracing-subscriber = { version = "0.3", default-features = false, features = ["fmt", "std"] }
[profile.release]
debug = false
EOF
cp "$ROOT/tools/t0/iwork-t04-ipcprobe.rs" "$WORK/ipcprobe/src/main.rs"
cp "$ROOT/Cargo.lock" "$WORK/ipcprobe/Cargo.lock"
(cd "$WORK/ipcprobe" && CARGO_TARGET_DIR="$WORK/ipcprobe/target" cargo build -q --release) 2>"$OUT/ipcprobe-build.txt"
IPC="$WORK/ipcprobe/target/release/ipcprobe"

# ---------------------------------------------------------------- 判据（方案 2.9 的两行登记表，照定义用 shell 写一遍）
cdhash_file() { { codesign -dvvv "$1" 2>&1 || true; } | sed -n 's/^CDHash=\([0-9a-f]\{40\}\)$/\1/p'; }
# 带清单的包：exe_dir 旁边有清单 → 能解析、每个二进制登记的每一种身份都对得上磁盘上的文件
manifest_form() {
  local dir="$1" m="$1/iwork-manifest.json"
  [ -f "$m" ] || { echo NotThisForm; return; }
  jq -e '.manifest_version == 1 and (.binaries | type == "object")' "$m" >/dev/null 2>&1 || { echo Incomplete; return; }
  local name entry
  for name in $(jq -r '.binaries | keys[]' "$m"); do
    [ -f "$dir/$name" ] || { echo Incomplete; return; }
    for entry in $(jq -r --arg n "$name" '.binaries[$n][]' "$m"); do
      case "$entry" in
        sha256:*) [ "sha256:$(shasum -a 256 "$dir/$name" | cut -d' ' -f1)" = "$entry" ] || { echo Incomplete; return; } ;;
        cdhash:*) [ "cdhash:$(cdhash_file "$dir/$name")" = "$entry" ] || { echo Incomplete; return; } ;;
        *) echo Incomplete; return ;;
      esac
    done
  done
  echo Complete
}
# 整包签名的包：后台在 .app 里且 .app 有封印 → 整包静态验签（严格 + 嵌套 + 全部架构）
signed_form() {
  local app="$1"
  [ -f "$app/Contents/_CodeSignature/CodeResources" ] || { echo NotThisForm; return; }
  if "$IMG" verify "$app" >/dev/null 2>&1; then echo Complete; else echo Incomplete; fi
}
forms() { echo "manifest=$(manifest_form "$1/Contents/MacOS") signed=$(signed_form "$1")"; }

# ---------------------------------------------------------------- 0. tauri build 在没配签名身份时留下了什么
{
  echo "== codesign -dvvv iWork.app"; codesign -dvvv "$BUILT_APP" 2>&1 | redact || true
  echo "== Contents/_CodeSignature"; ls -la "$BUILT_APP/Contents/_CodeSignature" 2>&1 | redact || true
  for b in "$BUILT_APP"/Contents/MacOS/*; do echo "== codesign -dvvv $(basename "$b")"; codesign -dvvv "$b" 2>&1 | redact || true; done
  echo "== codesign -dvvv target/release/iwork-client"; codesign -dvvv "$BUILT_CLIENT" 2>&1 | redact || true
  echo "== codesign --verify --deep --strict iWork.app"; codesign --verify --deep --strict --verbose=2 "$BUILT_APP" 2>&1 | redact || true
  echo "== cmp 包里的主程序 与 target/release/iwork-client"; cmp "$BUILT_APP/Contents/MacOS/iwork-client" "$BUILT_CLIENT" && echo identical || true
  echo "== find"; (cd "$BUILT_APP" && find . -type f | sort)
} > "$OUT/0-built-app.txt" 2>&1
HAS_SEAL=no; [ -f "$BUILT_APP/Contents/_CodeSignature/CodeResources" ] && HAS_SEAL=yes
APP_SIG=$(codesign -dvvv "$BUILT_APP" 2>&1 | sed -n 's/^Signature=//p' | head -1)
MAIN_FLAGS=$(codesign -dvvv "$BUILT_APP/Contents/MacOS/iwork-client" 2>&1 | sed -n 's/^CodeDirectory .*flags=\([^ ]*\).*/\1/p' | head -1)
note ""
note "## 0 tauri build（未配签名身份）的产物"
note "  整包封印 Contents/_CodeSignature/CodeResources：${HAS_SEAL}；codesign 报的 Signature=${APP_SIG:-<无>}；主程序 CodeDirectory flags=${MAIN_FLAGS:-<无>}"
note "  主程序与 target/release/iwork-client 相同：$(cmp -s "$BUILT_APP/Contents/MacOS/iwork-client" "$BUILT_CLIENT" && echo 是 || echo 否)"

# ---------------------------------------------------------------- 造两种形态
# 形态 A（无证书）：不做整包签名；每个二进制只有链接器的 ad-hoc 签名；最后跑 write-install-manifest.sh。
# tauri 若做了整包签名，就换回 cargo 原样产物、去掉封印（第 0 项记下了它做没做）。
make_a() { # make_a <目标.app> <daemon 二进制> [client 二进制]
  local app="$1"
  cp -R "$BUILT_APP" "$app"
  rm -rf "$app/Contents/_CodeSignature"
  cp "${3:-$BUILT_CLIENT}" "$app/Contents/MacOS/iwork-client"
  cp "$2" "$app/Contents/MacOS/iwork-daemon"
  "$ROOT/scripts/write-install-manifest.sh" "$app/Contents/MacOS" >/dev/null
}
# 形态 B（有证书）：整包签名，包里不放清单。先签嵌套的后台，再签整包。
make_b() { # make_b <目标.app> <daemon 二进制> [client 二进制] [后台的签名标识]
  local app="$1"
  cp -R "$BUILT_APP" "$app"
  rm -rf "$app/Contents/_CodeSignature"
  cp "${3:-$BUILT_CLIENT}" "$app/Contents/MacOS/iwork-client"
  cp "$2" "$app/Contents/MacOS/iwork-daemon"
  kc_codesign -f -s "$SIGNER" -i "${4:-dev.iwork.daemon}" "$app/Contents/MacOS/iwork-daemon" 2>>"$OUT/sign.txt"
  kc_codesign -f -s "$SIGNER" "$app" 2>>"$OUT/sign.txt"
}

security create-keychain -p t "$KC"
security unlock-keychain -p t "$KC"
security set-keychain-settings "$KC"
cat > "$WORK/cert.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$SIGNER
OU=IWORKTEST04
[v3]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$WORK/cert.cnf" -keyout "$WORK/c.key" -out "$WORK/c.pem" 2>/dev/null
openssl pkcs12 -export -legacy -inkey "$WORK/c.key" -in "$WORK/c.pem" -out "$WORK/c.p12" -passout pass:t
security import "$WORK/c.p12" -k "$KC" -P t -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k t "$KC" >/dev/null

# 起一个程序、等它跑起来、记 pid
# 不能写成 P=$(run_bg …)：命令替换会一直等后台进程关掉 stdout。结果放在全局变量 BG 里。
run_bg() { "$@" >/dev/null 2>&1 & BG=$!; CHILDREN+=("$BG"); }
alive_after() { sleep "$2"; kill -0 "$1" 2>/dev/null && echo alive || echo dead; }

# ================================================================ A 无证书的形态
note ""
note "## A 无证书的形态（不做整包签名，最后加清单）"
A="$WORK/A/iWork.app"; mkdir -p "$WORK/A"; make_a "$A" "$BUILT_DAEMON"
cp "$A/Contents/MacOS/iwork-manifest.json" "$OUT/A-manifest.json"
note "### A1 启动与握手"
# 真实客户端：命令行起一次、open（等同双击）起一次，各看 5 秒还在不在
run_bg "$A/Contents/MacOS/iwork-client"; P=$BG
check A1a "命令行起真实客户端 5 秒后" "$(alive_after "$P" 5)" alive
# A2 的真实客户端部分：运行中进程的 cdhash（codesign 按 pid 读动态代码）等于清单登记的 cdhash
RUN_CD=$(codesign -dvvv "$P" 2>&1 | sed -n 's/^CDHash=//p' | head -1)
MAN_CD=$(jq -r '.binaries["iwork-client"][] | select(startswith("cdhash:")) | sub("cdhash:"; "")' "$A/Contents/MacOS/iwork-manifest.json")
kill "$P" 2>/dev/null || true
open -n -g "$A"; sleep 5
# open 起的进程路径是 /private/var/…，按临时目录名匹配
OPEN_PID=$(pgrep -f "$(basename "$WORK")/A/iWork.app/Contents/MacOS/iwork-client" | head -1 || true)
check A1b "open（双击同一条路径）起真实客户端 5 秒后" "$([ -n "$OPEN_PID" ] && echo alive || echo dead)" alive
[ -n "$OPEN_PID" ] && kill "$OPEN_PID" 2>/dev/null || true
# 握手：同一个包的副本，两个角色换成 ipcprobe（不开 dev，RequireSigner），清单照打包流程最后重写
AH="$WORK/AH/iWork.app"; mkdir -p "$WORK/AH"; make_a "$AH" "$IPC" "$IPC"
SOCK="$WORK/s.sock"
"$AH/Contents/MacOS/iwork-daemon" serve "$SOCK" 6 >"$OUT/A1-serve.txt" 2>&1 & SP=$!; CHILDREN+=("$SP")
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
"$AH/Contents/MacOS/iwork-client" hello "$SOCK" >"$OUT/A1-hello.txt" 2>&1 || true
wait "$SP" || true
for f in A1-serve A1-hello; do redact < "$OUT/$f.txt" > "$OUT/$f.tmp" && mv "$OUT/$f.tmp" "$OUT/$f.txt"; done
check A1c "后台角色 manifest_loaded self_listed" "$(grep -c 'manifest_loaded.*self_listed=true' "$OUT/A1-serve.txt")" 1
check A1d "客户端角色 manifest_loaded self_listed" "$(grep -c 'manifest_loaded.*self_listed=true' "$OUT/A1-hello.txt")" 1
check A1e "握手" "$(grep -o 'handshake=[a-z]*' "$OUT/A1-hello.txt")" handshake=ok
check A1f "后台没有拒绝对端" "$(grep -c 'peer_rejected' "$OUT/A1-serve.txt" || true)" 0

# 客户端角色若取不到自己的身份：在同样的位置看 macos.rs check_image_file 那一步默认参数与不验资源两种结果
for role in iwork-client iwork-daemon; do
  RC="$WORK/RC-$role/iWork.app"; mkdir -p "$(dirname "$RC")"; make_a "$RC" "$IMG" "$IMG"
  note "  A1g [$role 位置，未封印的包] $("$RC/Contents/MacOS/$role" selfcheck)"
done
note "  A1g [散放的二进制] $("$IMG" selfcheck)"
# 对照：清单外的对端仍被拒：形态 A 打好清单后，把客户端位置换成换了签名标识重签的同一个探针（cdhash 必变）。
# 注意不能用「末尾追加一字节」：签名之外的尾部不在代码目录覆盖范围内，cdhash 不变，对端照样是清单里那一个。
PT="$WORK/PT/iWork.app"; mkdir -p "$WORK/PT"; make_a "$PT" "$IPC" "$IPC"
cp "$IPC" "$WORK/ipcp-other"; codesign -f -s - -i dev.iwork.t04.other "$WORK/ipcp-other" 2>/dev/null
cp "$WORK/ipcp-other" "$PT/Contents/MacOS/iwork-client"
note "  A1h 清单外的客户端 cdhash：$(cdhash_file "$PT/Contents/MacOS/iwork-client")；清单里的：$(jq -r '.binaries["iwork-client"][] | select(startswith("cdhash:"))' "$PT/Contents/MacOS/iwork-manifest.json")"
SOCK="$WORK/pt.sock"
"$PT/Contents/MacOS/iwork-daemon" serve "$SOCK" 6 >"$OUT/A1h-tamper-serve.txt" 2>&1 & SP=$!; CHILDREN+=("$SP")
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
"$PT/Contents/MacOS/iwork-client" hello "$SOCK" >"$OUT/A1h-tamper-hello.txt" 2>&1 || true
wait "$SP" || true
for f in A1h-tamper-serve A1h-tamper-hello; do redact < "$OUT/$f.txt" > "$OUT/$f.tmp" && mv "$OUT/$f.tmp" "$OUT/$f.txt"; done
check A1h "（对照）客户端换成清单外的映像，后台拒绝的理由" "$(grep -o 'reason=[A-Za-z]*' "$OUT/A1h-tamper-serve.txt" | head -1)" reason=ImageNotTrusted
# 对照：B 形态（整包签名）里两个角色握手
BH="$WORK/BH/iWork.app"; mkdir -p "$WORK/BH"
note "### A2 清单登记项与运行中进程、文件全文"
check A2a "真实客户端运行中 cdhash == 清单 cdhash" "$RUN_CD" "$MAN_CD"
for name in iwork-client iwork-daemon; do
  check "A2b" "$name 清单 sha256 == 文件全文 SHA-256" \
    "$(jq -r --arg n "$name" '.binaries[$n][] | select(startswith("sha256:"))' "$A/Contents/MacOS/iwork-manifest.json")" \
    "sha256:$(shasum -a 256 "$A/Contents/MacOS/$name" | cut -d' ' -f1)"
done
# 后台角色（imgprobe 放在后台的位置）：运行中 cdhash（SecCodeCopySelf）== 清单 cdhash
AD="$WORK/AD/iWork.app"; mkdir -p "$WORK/AD"; make_a "$AD" "$IMG"
"$AD/Contents/MacOS/iwork-daemon" self > "$OUT/A2-daemon-self.txt" 2>&1 || true
check A2c "后台位置的运行中 cdhash == 清单 cdhash" \
  "$(sed -n 's/^running=\([0-9a-f]*\) .*/\1/p' "$OUT/A2-daemon-self.txt")" \
  "$(jq -r '.binaries["iwork-daemon"][] | select(startswith("cdhash:")) | sub("cdhash:"; "")' "$AD/Contents/MacOS/iwork-manifest.json")"

note "### A3 带清单那一行的判定"
check A3a "完整的 A 包" "$(manifest_form "$A/Contents/MacOS")" Complete
A3="$WORK/A3"; mkdir -p "$A3"; cp -R "$A" "$A3/nomanifest.app"; rm "$A3/nomanifest.app/Contents/MacOS/iwork-manifest.json"
check A3b "删掉清单" "$(manifest_form "$A3/nomanifest.app/Contents/MacOS")" NotThisForm
cp -R "$A" "$A3/half.app"; f="$A3/half.app/Contents/MacOS/iwork-daemon"; truncate -s $(( $(stat -f%z "$f") / 2 )) "$f"
check A3c "后台二进制截断一半" "$(manifest_form "$A3/half.app/Contents/MacOS")" Incomplete
check A3d "完整的 A 包在整包签名那一行" "$(signed_form "$A")" NotThisForm

note "### A4 记录项（不设通过条件）"
{
  echo "== codesign --verify --deep --strict A"; codesign --verify --deep --strict --verbose=2 "$A" 2>&1 | redact || true
  echo "== spctl --assess --type exec A"; spctl --assess --type exec -vv "$A" 2>&1 | redact || true
  cp -R "$A" "$WORK/Aq.app"
  xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$WORK/Aq.app"
  echo "== 带隔离标记：spctl --assess --type exec"; spctl --assess --type exec -vv "$WORK/Aq.app" 2>&1 | redact || true
  echo "== 带隔离标记：命令行直接起主程序（不经 LaunchServices）"
  "$WORK/Aq.app/Contents/MacOS/iwork-client" & q=$!; sleep 4; kill -0 $q 2>/dev/null && echo "alive" || echo "dead"; kill $q 2>/dev/null || true
} > "$OUT/A4.txt" 2>&1
note "  见 A4.txt：$(grep -A1 '^== codesign --verify' "$OUT/A4.txt" | tail -1)"
note "  隔离标记下 spctl：$(grep -A1 '带隔离标记：spctl' "$OUT/A4.txt" | tail -1)"
note "  隔离标记下命令行起主程序：$(tail -1 "$OUT/A4.txt")（双击会走 Gatekeeper 弹窗，无人值守不做，见 t0-findings）"

# ================================================================ B 有证书的形态
note ""
note "## B 有证书的形态（自签证书整包签名，包里不放清单）"
B="$WORK/B/iWork.app"; mkdir -p "$WORK/B"; make_b "$B" "$BUILT_DAEMON"
{ echo "== codesign -dvvv B"; codesign -dvvv "$B" 2>&1 | redact; echo "== codesign -dvvv B/iwork-daemon"; codesign -dvvv "$B/Contents/MacOS/iwork-daemon" 2>&1 | redact; } > "$OUT/B-sign.txt" || true
note "### B1 整包验签"
check B1a "codesign --verify --deep --strict" "$(codesign --verify --deep --strict "$B" 2>/dev/null && echo ok || echo fail)" ok
check B1b "SecStaticCodeCheckValidity 严格 + 嵌套 + 全部架构" "$("$IMG" verify "$B" | cut -d' ' -f1)" verify=0
check B1c "整包签名那一行" "$(signed_form "$B")" Complete
check B1d "带清单那一行" "$(manifest_form "$B/Contents/MacOS")" NotThisForm

make_b "$BH" "$IPC" "$IPC"
SOCK="$WORK/b.sock"
"$BH/Contents/MacOS/iwork-daemon" serve "$SOCK" 6 >"$OUT/B1-serve.txt" 2>&1 & SP=$!; CHILDREN+=("$SP")
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
"$BH/Contents/MacOS/iwork-client" hello "$SOCK" >"$OUT/B1-hello.txt" 2>&1 || true
wait "$SP" || true
for f in B1-serve B1-hello; do redact < "$OUT/$f.txt" > "$OUT/$f.tmp" && mv "$OUT/$f.tmp" "$OUT/$f.txt"; done
check B1e "（对照）整包签名的包里两个角色握手" "$(grep -o 'handshake=[a-z]*' "$OUT/B1-hello.txt")" handshake=ok
note "### B2 逐个破坏 → 验签不过、判没装齐"
B2="$WORK/B2"; mkdir -p "$B2"
breakit() { # breakit <名字> <动作，在包目录里执行>
  local app="$B2/$1.app"; cp -R "$B" "$app"
  (cd "$app" && eval "$2")
  local v; v=$("$IMG" verify "$app" || true)
  echo "== $1: $v" >> "$OUT/B2.txt"
  check B2 "$1" "$(signed_form "$app")" Incomplete
}
breakit rm-resource 'rm Contents/Resources/icon.icns'
breakit trunc-daemon 'truncate -s $(( $(stat -f%z Contents/MacOS/iwork-daemon) / 2 )) Contents/MacOS/iwork-daemon'
breakit trunc-client 'truncate -s $(( $(stat -f%z Contents/MacOS/iwork-client) / 2 )) Contents/MacOS/iwork-client'
breakit plist-byte 'perl -pi -e "s/iWork/iWorX/ if \$. == 1 .. 0" Contents/Info.plist; grep -q iWorX Contents/Info.plist'
breakit extra-file 'echo x > Contents/Resources/extra.txt'
redact < "$OUT/B2.txt" > "$OUT/B2.tmp" && mv "$OUT/B2.tmp" "$OUT/B2.txt"

note "### B3 后台位置：磁盘静态 cdhash == 运行中 cdhash；换构建标记后两者都变"
B3="$WORK/B3"; mkdir -p "$B3/m1" "$B3/m2"
make_b "$B3/m1/iWork.app" "$WORK/bin/imgprobe1"; make_b "$B3/m2/iWork.app" "$WORK/bin/imgprobe2"
"$B3/m1/iWork.app/Contents/MacOS/iwork-daemon" self > "$OUT/B3-m1.txt" 2>&1 || true
"$B3/m2/iWork.app/Contents/MacOS/iwork-daemon" self > "$OUT/B3-m2.txt" 2>&1 || true
r1=$(sed -n 's/^running=\([0-9a-f]*\) .*/\1/p' "$OUT/B3-m1.txt"); s1=$(cdhash_file "$B3/m1/iWork.app/Contents/MacOS/iwork-daemon")
r2=$(sed -n 's/^running=\([0-9a-f]*\) .*/\1/p' "$OUT/B3-m2.txt"); s2=$(cdhash_file "$B3/m2/iWork.app/Contents/MacOS/iwork-daemon")
check B3a "标记 1：running == 静态（codesign 读文件）" "$r1" "$s1"
check B3b "标记 2：running == 静态" "$r2" "$s2"
check B3c "两个标记的 cdhash 不同" "$([ -n "$r1" ] && [ "$r1" != "$r2" ] && echo differ || echo same)" differ
check B3d "两个包都整包验签通过" "$(signed_form "$B3/m1/iWork.app")/$(signed_form "$B3/m2/iWork.app")" Complete/Complete
# 真实客户端在 B 包里：运行中 cdhash == 静态 cdhash
run_bg "$B/Contents/MacOS/iwork-client"; P=$BG; sleep 3
check B3e "真实客户端（证书签名）运行中 cdhash == 静态" "$(codesign -dvvv "$P" 2>&1 | sed -n 's/^CDHash=//p' | head -1)" "$(cdhash_file "$B/Contents/MacOS/iwork-client")"
kill "$P" 2>/dev/null || true

note "### B4 整包验签耗时（记数）"
for i in 1 2 3 4 5; do "$IMG" verify "$B"; done > "$OUT/B4.txt"
note "  5 次：$(sed -n 's/.*ms=\([0-9.]*\).*/\1/p' "$OUT/B4.txt" | tr '\n' ' ')ms；包大小 $(du -sh "$B" | cut -f1)"

note "### B5 反例：有证书的包里放清单"
B5="$WORK/B5"; mkdir -p "$B5"
cp -R "$B" "$B5/after.app"; "$ROOT/scripts/write-install-manifest.sh" "$B5/after.app/Contents/MacOS" >/dev/null
check B5a "签名之后加清单：整包验签" "$("$IMG" verify "$B5/after.app" >/dev/null 2>&1 && echo ok || echo fail)" fail
# 签名之前写清单再签：要么签不上（codesign 把 Contents/MacOS 里的文件当代码），要么清单登记的是签名前的二进制
mkdir -p "$B5/before"; cp -R "$BUILT_APP" "$B5/before/iWork.app"; app="$B5/before/iWork.app"
rm -rf "$app/Contents/_CodeSignature"; cp "$BUILT_CLIENT" "$app/Contents/MacOS/iwork-client"; cp "$BUILT_DAEMON" "$app/Contents/MacOS/iwork-daemon"
"$ROOT/scripts/write-install-manifest.sh" "$app/Contents/MacOS" >/dev/null
kc_codesign -f -s "$SIGNER" -i dev.iwork.daemon "$app/Contents/MacOS/iwork-daemon" 2>/dev/null
note "  B5b0 清单放在 Contents/MacOS 里再签整包：$(kc_codesign -f -s "$SIGNER" "$app" 2>&1 | redact | tail -1)"
check B5b "签名之前写清单（若签得上）：整包验签不过或清单对不上" \
  "$({ "$IMG" verify "$app" >/dev/null 2>&1 && [ "$(manifest_form "$app/Contents/MacOS")" = Complete ]; } && echo both-ok || echo not-both)" not-both
note "  B5c 签名之前写清单：验签 $("$IMG" verify "$app" | cut -d' ' -f1)，清单那一行 $(manifest_form "$app/Contents/MacOS")"

note "### B6 最后一个文件到位后重查能过；后台程序与封印文件的 stat 不变"
B6="$WORK/B6"; mkdir -p "$B6"; cp -R "$B" "$B6/x.app"; app="$B6/x.app"
st() { stat -f '%z %m %i' "$app/Contents/MacOS/iwork-daemon" "$app/Contents/_CodeSignature/CodeResources" | tr '\n' ' '; }
before=$(st); mv "$app/Contents/Resources/icon.icns" "$B6/icon.icns"
check B6a "拿走一个资源" "$(signed_form "$app")" Incomplete
mid=$(st); mv "$B6/icon.icns" "$app/Contents/Resources/icon.icns"
check B6b "放回去再查" "$(signed_form "$app")" Complete
after=$(st)
check B6c "后台程序与封印文件的 size/mtime/inode 三个时刻相同" "$([ "$before" = "$mid" ] && [ "$mid" = "$after" ] && echo same || echo changed)" same
note "  stat：$before"

# ================================================================ D 运行映像的身份
note ""
note "## D 运行映像的身份（A、B 两种包各一遍）"
d_case() { # d_case <标签> <包> <换文件的动作>
  local tag="$1" app="$2" act="$3" exe="$2/Contents/MacOS/iwork-daemon" log="$OUT/D-$1.txt"
  "$exe" self 6 > "$log" 2>&1 & local p=$!; CHILDREN+=("$p")
  # 等它取完启动时的身份（打出 static= 那一行）再换文件；高负载下证书签名的程序首次启动可能要好几秒
  for _ in $(seq 300); do grep -q '^static=' "$log" 2>/dev/null && break; sleep 0.1; done
  (cd "$(dirname "$exe")" && eval "$act") >> "$log" 2>&1; wait "$p" || true
  redact < "$log" > "$log.tmp" && mv "$log.tmp" "$log"
  local first; first=$(sed -n 's/^running=\([0-9a-f]*\) .*/\1/p' "$log")
  check "D1/$tag" "运行映像 cdhash 取得到" "$([ -n "$first" ] && echo yes || echo no)" yes
  check "D2/$tag" "运行映像 == 自己路径的静态 cdhash（启动时）" "$(sed -n 's/^static=.* equal=\(.*\)/\1/p' "$log")" yes
  # D3：换文件后每秒重取的 running 只会是原值或报错，不会是新文件
  local bad; bad=$(sed -n 's/^tick=[0-9]* running=\([0-9a-f]*\)(0).*/\1/p' "$log" | grep -v -x "$first" | head -1 || true)
  check "D3/$tag" "换文件后运行映像从不报成新文件" "${bad:-none}" none
  note "  $tag 换文件后：$(grep '^tick=' "$log" | sed 's/^tick=[0-9]* //' | sed "s/$first/<原值>/g" | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
}
mkdir -p "$WORK/D"
for form in a b; do
  for how in mv rm; do
    app="$WORK/D/$form-$how/iWork.app"; mkdir -p "$(dirname "$app")"
    if [ $form = a ]; then make_a "$app" "$WORK/bin/imgprobe1"; else make_b "$app" "$WORK/bin/imgprobe1"; fi
    if [ $how = mv ]; then act="mv iwork-daemon iwork-daemon.old && cp $WORK/bin/imgprobe2 iwork-daemon"
    else act="rm iwork-daemon && cp $WORK/bin/imgprobe2 iwork-daemon"; fi
    d_case "$form-$how" "$app" "$act"
  done
done
# D1：同一张证书签的两个构建取到的值不同（B3 已分别取过）
check D1-cert "同一张证书签的两个构建，运行映像 cdhash 不同" "$([ -n "$r1" ] && [ "$r1" != "$r2" ] && echo differ || echo same)" differ
# D2：读静态 cdhash 有没有读全文——文件中部（远离签名）改坏一个字节后照样取到原值且耗时不随文件大小变
cp "$WORK/bin/imgprobe1" "$WORK/D/big"; dd if=/dev/zero bs=1m count=200 >> "$WORK/D/big.pad" 2>/dev/null
"$IMG" static "$WORK/bin/imgprobe1" > "$OUT/D2-static.txt"
cp "$BUILT_CLIENT" "$WORK/D/client-copy"
"$IMG" static "$WORK/D/client-copy" >> "$OUT/D2-static.txt"
orig=$(sed -n 's/^static=\([0-9a-f]*\) .*/\1/p' "$OUT/D2-static.txt" | tail -1)
printf '\x00' | dd of="$WORK/D/client-copy" bs=1 seek=$(( $(stat -f%z "$WORK/D/client-copy") / 3 )) conv=notrunc 2>/dev/null
"$IMG" static "$WORK/D/client-copy" >> "$OUT/D2-static.txt"
check D2-noread "改坏客户端文件中部一字节后读静态 cdhash（不校验）" "$(sed -n 's/^static=\([0-9a-f]*\) .*/\1/p' "$OUT/D2-static.txt" | tail -1)" "$orig"
note "  静态 cdhash 耗时：imgprobe（$(stat -f%z "$WORK/bin/imgprobe1") 字节）与客户端（$(stat -f%z "$BUILT_CLIENT") 字节）：$(sed -n 's/.*ms=\([0-9.]*\).*/\1/p' "$OUT/D2-static.txt" | tr '\n' ' ')ms"
# D2：通用二进制
UA="$WORK/D/univ/iWork.app"; mkdir -p "$(dirname "$UA")"; make_b "$UA" "$WORK/bin/imgprobe_universal"
"$UA/Contents/MacOS/iwork-daemon" self > "$OUT/D2-universal.txt" 2>&1 || true
{ echo "== lipo"; lipo -info "$UA/Contents/MacOS/iwork-daemon"; echo "== codesign -dvvv（默认本机架构）"; codesign -dvvv "$UA/Contents/MacOS/iwork-daemon" 2>&1 | grep -E 'CDHash=|Architecture|format'
  echo "== codesign -dvvv --arch x86_64"; codesign -dvvv --arch x86_64 "$UA/Contents/MacOS/iwork-daemon" 2>&1 | grep -E 'CDHash=|format'; } >> "$OUT/D2-universal.txt" 2>&1
redact < "$OUT/D2-universal.txt" > "$OUT/D2u.tmp" && mv "$OUT/D2u.tmp" "$OUT/D2-universal.txt"
check D2-universal "通用二进制：运行映像 == 自己路径的静态 cdhash" "$(sed -n 's/^static=.* equal=\(.*\)/\1/p' "$OUT/D2-universal.txt")" yes
check D2-universal-arch "取的是本机架构那一片（== codesign 默认架构的 CDHash）" \
  "$(sed -n 's/^running=\([0-9a-f]*\) .*/\1/p' "$OUT/D2-universal.txt")" \
  "$(cdhash_file "$UA/Contents/MacOS/iwork-daemon")"

note "### D4 用 iwork-ipc 的正式函数（running_image_id / file_image_ids / verify_bundle）复核"
for form in a b; do
  app="$WORK/D4/$form/iWork.app"; mkdir -p "$(dirname "$app")"
  if [ $form = a ]; then make_a "$app" "$IPC"; else make_b "$app" "$IPC"; fi
  "$app/Contents/MacOS/iwork-daemon" runid "$app" > "$OUT/D4-$form.txt" 2>&1 || true
  check "D4/$form" "运行映像 == 自己路径的文件身份（同一种）" "$(sed -n 's/^running=.* equal=\(.*\)/\1/p' "$OUT/D4-$form.txt")" true
  check "D4/$form" "running_image_id == codesign 读的 cdhash" "$(sed -n 's/^running=cdhash:\([0-9a-f]*\) .*/\1/p' "$OUT/D4-$form.txt")" "$(cdhash_file "$app/Contents/MacOS/iwork-daemon")"
done
check D4/a "verify_bundle（未封印的 A 包）" "$(grep -c 'verify_bundle=Err' "$OUT/D4-a.txt")" 1
check D4/b "verify_bundle（整包签名的 B 包）" "$(grep -o 'verify_bundle=Ok' "$OUT/D4-b.txt")" verify_bundle=Ok
# 同一张证书签的两个构建（后台签名标识不同 → cdhash 不同）：运行映像身份不同，SelfIdentity 的叶证书相同
for v in 1 2; do
  app="$WORK/D4/cert$v/iWork.app"; mkdir -p "$(dirname "$app")"; make_b "$app" "$IPC" "" "dev.iwork.daemon.build$v"
  "$app/Contents/MacOS/iwork-daemon" runid > "$OUT/D4-cert$v.txt" 2>&1 || true
done
c1=$(sed -n 's/^running=\([^ ]*\) .*/\1/p' "$OUT/D4-cert1.txt"); c2=$(sed -n 's/^running=\([^ ]*\) .*/\1/p' "$OUT/D4-cert2.txt")
l1=$(sed -n 's/^self signer=\([^ ]*\) .*/\1/p' "$OUT/D4-cert1.txt"); l2=$(sed -n 's/^self signer=\([^ ]*\) .*/\1/p' "$OUT/D4-cert2.txt")
check D4-cert "同一张证书两个构建：SelfIdentity 的签名者相同、运行映像不同" \
  "$([ -n "$c1" ] && [ "$c1" != "$c2" ] && [ "$l1" = "$l2" ] && echo "same-signer,different-image" || echo "signer=$l1/$l2 image=$c1/$c2")" "same-signer,different-image"
note "  签名者 ${l1}；运行映像 $c1 / $c2"

# ================================================================ E 两种形态互换
note ""
note "## E 两种形态互换"
E="$WORK/E"; mkdir -p "$E"
cp -R "$A" "$E/AtoB.app"; rm -rf "$E/AtoB.app"; cp -R "$B" "$E/AtoB.app"
check E1a "A 的目录整包换成 B" "$(forms "$E/AtoB.app")" "manifest=NotThisForm signed=Complete"
cp -R "$B" "$E/BtoA.app"; rm -rf "$E/BtoA.app"; cp -R "$A" "$E/BtoA.app"
check E1b "B 的目录整包换成 A" "$(forms "$E/BtoA.app")" "manifest=Complete signed=NotThisForm"
cp -R "$A" "$E/merge.app"; cp -R "$B/" "$E/merge.app/"
note "  E2 把 B 合并拷进 A（旧清单残留）：$(forms "$E/merge.app")；验签报：$("$IMG" verify "$E/merge.app" | redact)"
rm "$E/merge.app/Contents/MacOS/iwork-manifest.json"
note "  E2 删掉残留清单后：$(forms "$E/merge.app")"

note ""
note "FAIL 共 $FAILS 项。原始输出在 tools/t0/out/t04-bundle/"
[ "$FAILS" -eq 0 ]
