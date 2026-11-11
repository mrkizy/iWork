#!/usr/bin/env bash
# iWork T03 N1（方案 8.1）：macOS 审计令牌、动态 SecCode、自签证书、换文件反例。约 2 分钟，只在 macOS 跑。
# 不碰登录钥匙串：证书放临时钥匙串，二进制、私钥都在临时目录，退出时一并删除。
# 结果：tools/t0/out/t03-macos/（summary.txt 是结论；其余是各步原始输出）。
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t03-macos"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/iwork-t03-macos.XXXXXX")
KC="$WORK/t03.keychain-db"
# 临时钥匙串：建好后关掉自动上锁（默认空闲 5 分钟、睡眠就锁，之后 codesign 会弹框找人要密码）；
# 每次签名前再解锁一次。整个过程不该出现要人输密码的框。
kc_codesign() { security unlock-keychain -p t "$KC"; codesign --keychain "$KC" "$@"; }
SOCK="$WORK/probe.sock"
CHILDREN=()

cleanup() {
  for pid in "${CHILDREN[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
  security delete-keychain "$KC" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT
# 被 timeout 或 Ctrl-C 结束时也走 EXIT，删掉临时钥匙串
trap 'exit 143' TERM INT

rm -rf "$OUT"; mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
note() { echo "$*" | tee -a "$SUMMARY"; }

note "# T03 N1 macOS 实测  $(date '+%F %T')"
note "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"

# ---------------------------------------------------------------- 0. 探针
(cd "$ROOT" && cargo build -q -p iwork-ipc --features dev --example peer-probe 2>"$OUT/build.txt")
PROBE="$ROOT/target/debug/examples/peer-probe"
BIN="$WORK/bin"; mkdir -p "$BIN"

# ---------------------------------------------------------------- 1. 两张自签代码签名证书 + 临时钥匙串
security create-keychain -p t "$KC"
security unlock-keychain -p t "$KC"
security set-keychain-settings "$KC"
for who in A B; do
  cat > "$WORK/$who.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=iWork T03 Signer $who
OU=IWORKTEST0$who
[v3]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
EOF
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$WORK/$who.cnf" \
    -keyout "$WORK/$who.key" -out "$WORK/$who.pem" 2>/dev/null
  # -legacy：security import 不认 OpenSSL 3 默认的 PKCS#12 加密算法。
  openssl pkcs12 -export -legacy -inkey "$WORK/$who.key" -in "$WORK/$who.pem" \
    -out "$WORK/$who.p12" -passout pass:t
  security import "$WORK/$who.p12" -k "$KC" -P t -T /usr/bin/codesign >/dev/null
done
security set-key-partition-list -S apple-tool:,apple: -s -k t "$KC" >/dev/null
leaf_sha256() { openssl x509 -in "$WORK/$1.pem" -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1; }
leaf_sha1()   { openssl x509 -in "$WORK/$1.pem" -outform DER | openssl dgst -sha1 -r | cut -d' ' -f1; }
LEAF_A=$(leaf_sha256 A); LEAF_B=$(leaf_sha256 B)
note "leaf A sha256=$LEAF_A"
note "leaf B sha256=$LEAF_B"

# ---------------------------------------------------------------- 2. 被测二进制（都是 peer-probe 的副本）
sign() { kc_codesign -f -s "iWork T03 Signer $1" -i "$3" "$2" 2>/dev/null; }
cp "$PROBE" "$BIN/linker"                                   # 链接器自动做的 ad-hoc（cargo 产物原样）
cp "$PROBE" "$BIN/adhoc";    codesign -f -s - "$BIN/adhoc" 2>/dev/null
cp "$PROBE" "$BIN/signedA";  sign A "$BIN/signedA"  t03.probe
cp "$PROBE" "$BIN/signedA2"; sign A "$BIN/signedA2" t03.probe.second   # 同一张证书、不同 cdhash
cp "$PROBE" "$BIN/signedB";  sign B "$BIN/signedB"  t03.probe
cp "$PROBE" "$BIN/unsigned"; codesign --remove-signature "$BIN/unsigned"
# 改一字节：文件中部，落在 __TEXT 代码页里
cp "$BIN/signedA" "$BIN/tampered"
printf '\x00' | dd of="$BIN/tampered" bs=1 seek=$(( $(stat -f%z "$BIN/tampered") / 2 )) conv=notrunc 2>/dev/null
# arm64 上未签名的程序内核不让跑；x86_64 在 Rosetta 下可以，拿它测「运行中的未签名对端」。
cat > "$WORK/client.c" <<'EOF'
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
int main(int argc, char **argv) {
  if (argc != 3 || strcmp(argv[1], "--connect")) return 2;
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = {0}; a.sun_family = AF_UNIX;
  strncpy(a.sun_path, argv[2], sizeof a.sun_path - 1);
  if (connect(fd, (struct sockaddr *)&a, sizeof a)) { perror("connect"); return 1; }
  printf("connected pid=%d\n", getpid()); fflush(stdout);
  for (;;) pause();
}
EOF
cc -arch x86_64 "$WORK/client.c" -o "$BIN/unsigned_x86"
# 独占一页的函数：连接时不调入，收到 SIGUSR1 才执行。用来看「改了还没调入的页」时动态校验与内核各自怎么反应。
cat > "$WORK/late.c" <<'EOF'
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
__asm__(".section __TEXT,__late,regular,pure_instructions\n.globl _late\n.p2align 2\n_late:\n"
        "add w0, w0, w0, lsl #1\nadd w0, w0, #1\nret\n.space 0x4000\n.text\n");
int late(int);
static volatile sig_atomic_t go;
static void on_usr1(int s) { (void)s; go = 1; }
int main(int argc, char **argv) {
  if (argc != 3 || strcmp(argv[1], "--connect")) return 2;
  signal(SIGUSR1, on_usr1);
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = {0}; a.sun_family = AF_UNIX;
  strncpy(a.sun_path, argv[2], sizeof a.sun_path - 1);
  if (connect(fd, (struct sockaddr *)&a, sizeof a)) { perror("connect"); return 1; }
  char pidfile[1024]; snprintf(pidfile, sizeof pidfile, "%s.pid", argv[0]);
  FILE *f = fopen(pidfile, "w"); fprintf(f, "%d\n", getpid()); fclose(f);
  printf("connected pid=%d\n", getpid()); fflush(stdout);
  for (;;) { pause(); if (go) { go = 0; printf("late=%d\n", late(2)); fflush(stdout); } }
}
EOF
cc "$WORK/late.c" -o "$BIN/lateA" -Wl,-sectalign,__TEXT,__late,0x4000
sign A "$BIN/lateA" t03.late
LATE_OFF=$(otool -l "$BIN/lateA" | awk '/sectname __late/{f=1} f&&/ offset/{print $2; exit}')
codesign --remove-signature "$BIN/unsigned_x86" 2>/dev/null || true

# ---------------------------------------------------------------- 3. codesign 自验
{
  for b in linker adhoc signedA signedA2 signedB unsigned tampered unsigned_x86; do
    echo "== $b"
    codesign -dvvv "$BIN/$b" 2>&1 | grep -E 'Identifier|Signature|CDHash=|Authority|flags' || true
    codesign --verify --verbose=2 "$BIN/$b" 2>&1 || echo "verify failed: $?"
  done
  SHA1_A=$(leaf_sha1 A)
  codesign --verify -R "=certificate leaf = H\"$SHA1_A\"" "$BIN/signedA"  && echo "signedA  satisfies leaf A requirement"
  codesign --verify -R "=certificate leaf = H\"$SHA1_A\"" "$BIN/signedA2" && echo "signedA2 satisfies leaf A requirement"
  codesign --verify -R "=certificate leaf = H\"$SHA1_A\"" "$BIN/signedB"  || echo "signedB  rejected by leaf A requirement (expected)"
  codesign --verify -R "=certificate leaf = H\"$SHA1_A\"" "$BIN/adhoc"    || echo "adhoc    rejected by leaf A requirement (expected)"
  codesign --verify -R "=certificate leaf = H\"$LEAF_A\"" "$BIN/signedA"  || echo "SHA-256 leaf hash in requirement rejected (requirement language takes SHA-1 only)"
} > "$OUT/codesign-static.txt" 2>&1

# ---------------------------------------------------------------- 4. 运行中对端的身份 + 换文件反例
# observe <名字> <可执行文件> [动作]：起 peer-probe 服务端；对端连上 2 秒后执行动作；再看 4 秒。
observe() {
  local name="$1" bin="$2" action="${3:-true}" log="$OUT/run-$1.txt"
  rm -f "$SOCK"
  : > "$log"; "$PROBE" --server "$SOCK" --hold 7 >> "$log" 2>&1 & local server=$!; CHILDREN+=("$server")
  for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
  "$bin" --connect "$SOCK" > "$OUT/run-$name.client.txt" 2>&1 & local client=$!; CHILDREN+=("$client")
  sleep 2
  echo "-- action: $action" >> "$log"
  (cd "$(dirname "$bin")" && eval "$action") >> "$log" 2>&1 || echo "-- action failed: $?" >> "$log"
  sleep 4.5
  kill "$client" 2>/dev/null || true
  local rc=0; wait "$client" 2>/dev/null || rc=$?; echo "-- client exit=$rc" >> "$log"
  wait "$server" 2>/dev/null || true
  # 按时间顺序列出出现过的身份（相邻去重），换文件前后的变化一眼可见。写日志的几方都用追加模式，互不覆盖。
  local seq
  seq=$(grep -E '^tick=|^-- ' "$log" | sed -e 's/.*signer=//' -e 's/^tick=.*error=/error=/' | uniq | sed 's/^/    /' || true)
  note "$name"; note "${seq:-    <none>}"
}
# 换文件用的工作副本放在单独目录，被测进程就从这里跑。
live() { local d="$WORK/live-$1"; rm -rf "$d"; mkdir -p "$d"; cp "$BIN/$2" "$d/live"; echo "$d/live"; }

note ""
note "## ②③④⑤ 各种签名的对端（每秒重算一次）"
note "期望：signedA/A2 = certificate:${LEAF_A}；signedB = certificate:${LEAF_B}；adhoc/linker = cdhash"
for b in linker adhoc signedA signedA2 signedB unsigned tampered unsigned_x86; do
  observe "$b" "$BIN/$b"
done

note ""
note "## ⑥ 换文件反例（对端连着时动它的可执行文件）"
note "（a）诚实对端 A，mv 走再放 B 到原路径：期望始终是 A"
observe a-mv-honest       "$(live a1 signedA)" 'mv live live.old && cp '"$BIN"'/signedB live'
note "（a'）诚实对端 A，rm 再放 B 到原路径：期望 A 或 unavailable，不得是 B"
observe a-rm-honest       "$(live a2 signedA)" 'rm live && cp '"$BIN"'/signedB live'
note "（a''）攻击：对端跑 B，mv 走再放 A 到原路径：期望 B 或 unavailable，不得是 A"
observe a-mv-attack       "$(live a3 signedB)" 'mv live live.old && cp '"$BIN"'/signedA live'
note "（a'''）攻击：对端跑 B，rm 再放 A 到原路径：期望 B 或 unavailable，不得是 A"
observe a-rm-attack       "$(live a4 signedB)" 'rm live && cp '"$BIN"'/signedA live'
note "（b）原地 dd 覆盖 A 中部一字节：期望 unavailable"
observe b-dd-inplace      "$(live b1 signedA)" 'printf "\x00" | dd of=live bs=1 seek=$(( $(stat -f%z live) / 2 )) conv=notrunc'
note "（b-late）A 对端运行中，原地改一个还没调入的代码页（__late，偏移 ${LATE_OFF}），2 秒后让它执行那一页：期望改完即 unavailable（对端打印 late=7 = 执行的是原始指令；被改的指令算不出 7）"
observe b-late-page       "$(live b4 lateA)" 'printf "\xff" | dd of=live bs=1 seek='"$LATE_OFF"' conv=notrunc 2>/dev/null; echo "-- tampered"; sleep 2; kill -USR1 $(cat live.pid); echo "-- SIGUSR1 sent"; sleep 1; sed "s/^/-- peer: /" "$OUT/run-b-late-page.client.txt"'
note "（b'）攻击：对端跑 B，原地整份覆盖成 A（同一 inode）：期望 B 或 unavailable，不得是 A"
observe b-overwrite-attack "$(live b2 signedB)" 'cat '"$BIN"'/signedA > live'
note "（b''）ad-hoc 对端原地整份覆盖成另一个 ad-hoc：期望原 cdhash 或 unavailable"
observe b-overwrite-adhoc "$(live b3 adhoc)" 'cat '"$BIN"'/linker > live'

# ---------------------------------------------------------------- 5. 两步之间换文件（C 探针，与 macos.rs 同一串调用）
# pin 模式 = macos.rs 的顺序：读签名信息 → 暂停（期间换文件）→ 钉住读到的身份做 SecCodeCheckValidity。
# naive 模式 = 方案草案的顺序：SecCodeCheckValidity（不带要求）→ 暂停（期间换文件）→ 读签名信息。
cat > "$WORK/race.c" <<'EOF'
#include <CommonCrypto/CommonDigest.h>
#include <Security/Security.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
static void hex(const UInt8 *p, CFIndex n, char *out) { for (CFIndex i = 0; i < n; i++) sprintf(out + 2 * i, "%02x", p[i]); }
/* 打印签名信息里声称的身份，并返回钉住它的要求文本 */
static int claim(SecCodeRef code, char *req, size_t reqlen) {
  CFDictionaryRef info = NULL;
  OSStatus st = SecCodeCopySigningInformation(code, kSecCSSigningInformation, &info);
  printf("SecCodeCopySigningInformation=%d\n", (int)st);
  if (st) return -1;
  CFArrayRef certs = CFDictionaryGetValue(info, kSecCodeInfoCertificates);
  CFDataRef uniq = CFDictionaryGetValue(info, kSecCodeInfoUnique);
  char h[200] = {0};
  if (certs && CFArrayGetCount(certs) > 0) {
    CFDataRef der = SecCertificateCopyData((SecCertificateRef)CFArrayGetValueAtIndex(certs, 0));
    UInt8 d256[32], d1[20];
    CC_SHA256(CFDataGetBytePtr(der), (CC_LONG)CFDataGetLength(der), d256);
    CC_SHA1(CFDataGetBytePtr(der), (CC_LONG)CFDataGetLength(der), d1);
    hex(d256, 32, h); printf("claim certificate:%s\n", h);
    hex(d1, 20, h); snprintf(req, reqlen, "certificate leaf = H\"%s\"", h);
    CFRelease(der);
  } else if (uniq) {
    hex(CFDataGetBytePtr(uniq), CFDataGetLength(uniq), h);
    printf("claim cdhash:%s\n", h);
    snprintf(req, reqlen, "cdhash H\"%s\"", h);
  } else {
    printf("claim unsigned\n");
  }
  CFRelease(info);
  return 0;
}
int main(int argc, char **argv) {
  if (argc != 4) { fprintf(stderr, "race <sock> pin|naive <pause>\n"); return 2; }
  int s = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = {0}; a.sun_family = AF_UNIX; strncpy(a.sun_path, argv[1], sizeof a.sun_path - 1);
  unlink(argv[1]);
  if (bind(s, (struct sockaddr *)&a, sizeof a) || listen(s, 1)) { perror("bind"); return 1; }
  printf("listening\n"); fflush(stdout);
  int c = accept(s, NULL, NULL);
  audit_token_t tok; socklen_t len = sizeof tok;
  int rc = getsockopt(c, SOL_LOCAL, LOCAL_PEERTOKEN, &tok, &len);
  printf("getsockopt(LOCAL_PEERTOKEN) rc=%d len=%u (sizeof audit_token_t=%zu)\n", rc, len, sizeof tok);
  CFDataRef td = CFDataCreate(NULL, (const UInt8 *)&tok, sizeof tok);
  const void *k[] = {kSecGuestAttributeAudit}, *v[] = {td};
  CFDictionaryRef attrs = CFDictionaryCreate(NULL, k, v, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
  SecCodeRef guest = NULL;
  OSStatus st = SecCodeCopyGuestWithAttributes(NULL, attrs, kSecCSDefaultFlags, &guest);
  printf("SecCodeCopyGuestWithAttributes=%d\n", (int)st);
  if (st) return 1;
  char req[200] = {0};
  if (!strcmp(argv[2], "watch")) {
    /* 每秒一次：按令牌重新取动态对象（SecCode / SecStaticCode 会缓存校验结果，复用旧对象看不到变化），
       分别做动态校验（不带要求）与映像文件全量静态校验 */
    printf("PAUSE\n"); fflush(stdout);
    for (int i = 0; i < atoi(argv[3]); i++) {
      SecCodeRef g = NULL; SecStaticCodeRef sc = NULL;
      OSStatus gs = SecCodeCopyGuestWithAttributes(NULL, attrs, kSecCSDefaultFlags, &g);
      OSStatus dyn = gs ? gs : SecCodeCheckValidity(g, kSecCSDefaultFlags, NULL);
      OSStatus cs = gs ? gs : SecCodeCopyStaticCode(g, kSecCSDefaultFlags, &sc);
      OSStatus st2 = cs ? cs : SecStaticCodeCheckValidity(sc, kSecCSDefaultFlags, NULL);
      printf("dyn=%d static=%d\n", (int)dyn, (int)st2); fflush(stdout);
      if (sc) CFRelease(sc);
      if (g) CFRelease(g);
      sleep(1);
    }
  } else if (!strcmp(argv[2], "naive")) {
    printf("SecCodeCheckValidity(no requirement)=%d\n", (int)SecCodeCheckValidity(guest, kSecCSDefaultFlags, NULL));
    printf("PAUSE\n"); fflush(stdout); sleep(atoi(argv[3]));
    claim(guest, req, sizeof req);
  } else {
    claim(guest, req, sizeof req);
    printf("PAUSE\n"); fflush(stdout); sleep(atoi(argv[3]));
    SecRequirementRef r = NULL;
    CFStringRef rs = CFStringCreateWithCString(NULL, req, kCFStringEncodingUTF8);
    printf("SecRequirementCreateWithString=%d (%s)\n", (int)SecRequirementCreateWithString(rs, kSecCSDefaultFlags, &r), req);
    printf("SecCodeCheckValidity(pinned)=%d\n", (int)SecCodeCheckValidity(guest, kSecCSDefaultFlags, r));
  }
  fflush(stdout);
  return 0;
}
EOF
cc -framework Security -framework CoreFoundation "$WORK/race.c" -o "$WORK/race" 2>"$OUT/race-build.txt"

# race <名字> <模式> <对端> <动作> [秒]：对端连上后，探针在两步之间停 N 秒（默认 4；watch 模式是观察 N 次），期间执行动作。
race() {
  local name="$1" mode="$2" bin="$3" action="$4" secs="${5:-4}" log="$OUT/race-$1.txt"
  rm -f "$SOCK"
  : > "$log"; "$WORK/race" "$SOCK" "$mode" "$secs" >> "$log" 2>&1 & local server=$!; CHILDREN+=("$server")
  for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
  "$bin" --connect "$SOCK" > "$(dirname "$bin")/client.out" 2>&1 & local client=$!; CHILDREN+=("$client")
  for _ in $(seq 50); do grep -q PAUSE "$log" && break; sleep 0.1; done
  echo "-- action: $action" >> "$log"
  (cd "$(dirname "$bin")" && eval "$action") >> "$log" 2>&1 || echo "-- action failed: $?" >> "$log"
  wait "$server" 2>/dev/null || true
  kill "$client" 2>/dev/null || true; wait "$client" 2>/dev/null || true
  note "$(printf '%-28s %s' "$name" "$(grep -E '^(getsockopt|claim|SecCodeCheckValidity|dyn=|-- )' "$log" | uniq | tr '\n' ' ')")"
}
note ""
note "## ①（c）两步之间换文件"
note "pin：macos.rs 的顺序。期望：攻击方向 SecCodeCheckValidity(pinned) 只会对运行中的真实身份返回 0"
race c-pin-honest-mv   pin   "$(live c1 signedA)" 'mv live live.old && cp '"$BIN"'/signedB live'
race c-pin-attack-mv   pin   "$(live c2 signedB)" 'mv live live.old && cp '"$BIN"'/signedA live'
race c-pin-attack-rm   pin   "$(live c3 signedB)" 'rm live && cp '"$BIN"'/signedA live'
race c-pin-attack-cat  pin   "$(live c4 signedB)" 'cat '"$BIN"'/signedA > live'
note "naive：草案顺序（先验后读）。看读到的身份会不会变成换上去的文件"
race c-naive-attack-mv  naive "$(live c5 signedB)" 'mv live live.old && cp '"$BIN"'/signedA live'
race c-naive-attack-rm  naive "$(live c6 signedB)" 'rm live && cp '"$BIN"'/signedA live'
race c-naive-attack-cat naive "$(live c7 signedB)" 'cat '"$BIN"'/signedA > live'

note "watch：改还没调入的页 → 让它执行。看动态校验、全量静态校验、对端进程各自的反应"
race c-watch-late-page  watch "$(live c8 lateA)" 'printf "\xff" | dd of=live bs=1 seek='"$LATE_OFF"' conv=notrunc 2>/dev/null; echo "-- tampered"; sleep 3; kill -USR1 $(cat live.pid); echo "-- SIGUSR1 sent"; sleep 1; sed "s/^/-- peer: /" client.out; kill -0 $(cat live.pid) 2>/dev/null && echo "-- peer alive" || echo "-- peer dead"' 8

note ""
note "完成。原始输出在 $OUT"
