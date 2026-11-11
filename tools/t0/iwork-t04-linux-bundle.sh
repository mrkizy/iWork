#!/usr/bin/env bash
# iWork T04 N5 的 C 组（方案第 8 节）：Linux 的 tar 包解开的目录上，带清单那一行能判装齐；两端按清单互认；
# 运行映像身份 = /proc/self/exe 的 SHA-256，换掉路径上的文件后不变；T03 的 peer_manifest 测试照常通过。
# 在 Docker（rust:1）里跑，宿主是 macOS 或 Linux 都行。约 10–20 分钟（要在容器里编译）。
#
# 后台与客户端现在都还不会说 IPC（T04 第 4、8 步才接），握手两端用 tools/t0/iwork-t04-ipcprobe.rs 冒充
# （依赖 iwork-ipc、不开 dev feature）；包里另放一份真实的 release iwork-daemon，只看它在清单里登记得对。
# Linux 的发布包形态还没定（打包归第 10 步），这里按方案的说法用「两个二进制 + 清单」打一个 tar 再解开。
# 结果：tools/t0/out/t04-linux/
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t04-linux"
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/iwork-t04-linux.XXXXXX")" && pwd -P)  # 规范化（/private/var/…），日志里的路径才脱得掉
trap 'rm -rf "$WORK"' EXIT
rm -rf "$OUT"; mkdir -p "$OUT"

# 容器里用的源码：HEAD 加上工作区里这几个探针文件（它们可能还没提交）
mkdir -p "$WORK/repo"
git -C "$ROOT" archive HEAD | tar -x -C "$WORK/repo"
cp "$ROOT/tools/t0/iwork-t04-ipcprobe.rs" "$WORK/repo/tools/t0/"

cat > "$WORK/inner.sh" <<'INNER'
#!/usr/bin/env bash
set -euo pipefail
apt-get update -qq >/dev/null && apt-get install -y -qq jq >/dev/null
cd /w/repo
export CARGO_TARGET_DIR=/w/target CARGO_INCREMENTAL=0
S=/out/summary.txt
note() { echo "$*" | tee -a "$S"; }
FAILS=0
check() { if [ "$3" = "$4" ]; then note "  PASS $1 $2：$3"; else note "  FAIL $1 $2：实际 $3，期望 $4"; FAILS=$((FAILS+1)); fi; }
note "# T04 N5 C 组 Linux  $(date '+%F %T')"
note "$(uname -srm)；$(rustc --version)"

cargo build -q --release -p iwork-daemon 2>/out/build-daemon.txt
mkdir -p /w/ipcprobe/src
cat > /w/ipcprobe/Cargo.toml <<EOF
[package]
name = "ipcprobe"
version = "0.1.0"
edition = "2024"
[workspace]
[dependencies]
iwork-ipc = { path = "/w/repo/crates/iwork-ipc" }
iwork-proto = { path = "/w/repo/crates/iwork-proto" }
tokio = { version = "1.54", features = ["rt", "macros", "time"] }
tokio-util = "0.7"
tracing-subscriber = { version = "0.3", default-features = false, features = ["fmt", "std"] }
EOF
cp tools/t0/iwork-t04-ipcprobe.rs /w/ipcprobe/src/main.rs
cp Cargo.lock /w/ipcprobe/
(cd /w/ipcprobe && CARGO_TARGET_DIR=/w/ipcprobe/target cargo build -q --release) 2>/out/build-probe.txt
IPC=/w/ipcprobe/target/release/ipcprobe

# 打包：两个二进制 + 清单 → tar → 解开
manifest_form() {
  local dir="$1" m="$1/iwork-manifest.json"
  [ -f "$m" ] || { echo NotThisForm; return; }
  jq -e '.manifest_version == 1 and (.binaries | type == "object")' "$m" >/dev/null 2>&1 || { echo Incomplete; return; }
  local name entry
  for name in $(jq -r '.binaries | keys[]' "$m"); do
    [ -f "$dir/$name" ] || { echo Incomplete; return; }
    for entry in $(jq -r --arg n "$name" '.binaries[$n][]' "$m"); do
      case "$entry" in
        sha256:*) [ "sha256:$(sha256sum "$dir/$name" | cut -d' ' -f1)" = "$entry" ] || { echo Incomplete; return; } ;;
        *) echo Incomplete; return ;;
      esac
    done
  done
  echo Complete
}
pack() { # pack <名字> <daemon> <client>
  mkdir -p "/w/pack-$1"; cp "$2" "/w/pack-$1/iwork-daemon"; cp "$3" "/w/pack-$1/iwork-client"
  scripts/write-install-manifest.sh "/w/pack-$1" >/dev/null
  tar -C "/w/pack-$1" -cf "/w/$1.tar" .
  mkdir -p "/w/inst-$1"; tar -C "/w/inst-$1" -xf "/w/$1.tar"
}
pack real /w/target/release/iwork-daemon "$IPC"
pack hs "$IPC" "$IPC"
cp /w/inst-real/iwork-manifest.json /out/manifest-real.json

note "## C 带清单那一行"
check C1a "真实后台 + 客户端位置的探针，tar 解开后" "$(manifest_form /w/inst-real)" Complete
cp -r /w/inst-real /w/inst-half; f=/w/inst-half/iwork-daemon; truncate -s $(( $(stat -c%s "$f") / 2 )) "$f"
check C1b "后台截断一半" "$(manifest_form /w/inst-half)" Incomplete
cp -r /w/inst-real /w/inst-noman; rm /w/inst-noman/iwork-manifest.json
check C1c "删掉清单" "$(manifest_form /w/inst-noman)" NotThisForm

note "## C 两端握手（两端都是探针，按清单互认）"
/w/inst-hs/iwork-daemon serve /tmp/s.sock 6 >/out/serve.txt 2>&1 & SP=$!
for _ in $(seq 50); do [ -S /tmp/s.sock ] && break; sleep 0.1; done
/w/inst-hs/iwork-client hello /tmp/s.sock >/out/hello.txt 2>&1 || true
wait $SP || true
check C2a "后台 manifest_loaded self_listed" "$(grep -c 'manifest_loaded.*self_listed=true' /out/serve.txt)" 1
check C2b "客户端 manifest_loaded self_listed" "$(grep -c 'manifest_loaded.*self_listed=true' /out/hello.txt)" 1
check C2c "握手" "$(grep -o 'handshake=[a-z]*' /out/hello.txt)" handshake=ok

note "## C 运行映像身份"
/w/inst-hs/iwork-daemon selfwatch 5 >/out/selfwatch.txt 2>&1 & WP=$!
sleep 1.5
mv /w/inst-hs/iwork-daemon /w/inst-hs/iwork-daemon.old; cp /w/target/release/iwork-daemon /w/inst-hs/iwork-daemon
wait $WP || true
orig="sha256:$(sha256sum /w/inst-hs/iwork-daemon.old | cut -d' ' -f1)"
check C3a "启动时的映像身份 == /proc/self/exe 所指文件的 SHA-256" "$(sed -n 's/^image=//p' /out/selfwatch.txt)" "$orig"
check C3b "路径上换成别的文件后，每次重取仍是原值" "$(grep '^tick=' /out/selfwatch.txt | grep -vc "image=$orig" || true)" 0
note "  换文件后重取：$(grep '^tick=' /out/selfwatch.txt | sed "s#$orig#<原值>#" | tr '\n' ';')"

note "## C T03 的 peer_manifest 测试"
cargo test -q -p iwork-ipc --test peer_manifest > /out/peer_manifest.txt 2>&1 && r=ok || r=fail
check C4 "cargo test -p iwork-ipc --test peer_manifest" "$r" ok
note "  $(grep -E '^test result' /out/peer_manifest.txt | tail -1)"
note "FAIL 共 $FAILS 项"
INNER
chmod +x "$WORK/inner.sh"

docker run --rm -v "$WORK/repo:/w/repo" -v "$WORK/inner.sh:/inner.sh:ro" -v "$OUT:/out" \
  -e CARGO_HOME=/w/cargo rust:1 bash -c 'mkdir -p /w && /inner.sh'
grep -q 'FAIL 共 0 项' "$OUT/summary.txt"
