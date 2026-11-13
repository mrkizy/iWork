#!/usr/bin/env bash
# 测试用：导入一张自签的代码签名证书（macOS）。T04p 方案 4.4、R10 的测试与 CI 的形态 B 打包共用这一个建法
# （做法照 tools/t0/iwork-t04-bundle-probe.sh 155–172 行，N5 量过）。
#
# 用法：scripts/test-signing-identity.sh <目录> <证书名>
# 输出五行（stdout）：
#   keychain=<钥匙串文件的绝对路径>
#   owned=<yes|no>：yes 表示钥匙串是这一次新建的，调用方用完要 `security delete-keychain` 删掉；no 表示是别人给的，不删
#   identity=<证书名>：给 IWORK_SIGN_IDENTITY 用（按名字在钥匙串里找到恰好这一张）
#   leaf_sha256=<叶证书 DER 的 SHA-256，64 个十六进制位>
#   sha1=<叶证书 DER 的 SHA-1，40 个十六进制位>：签名时用它当 codesign -s 的身份，指定的恰好是这一张
#
# 两种用法：
#   - 默认：在 <目录> 里新建一个钥匙串（owned=yes）。只动 <目录> 里的东西：不改当前用户的钥匙串搜索列表
#     （`security create-keychain` 不改搜索列表，本机实测），不设默认钥匙串，不改登录钥匙串。签名时调用方给
#     `codesign --keychain <钥匙串>`；本机（macOS 27.2）这样就能签。
#   - 环境变量 IWORK_TEST_SIGNING_KEYCHAIN=<已有的钥匙串>（只有 CI 设）：证书导入这个钥匙串（owned=no），证书名后面
#     加一段随机后缀，免得并发的测试撞名（按名字找时要恰好一张）；导入时加锁，并发导入不互相踩。这个钥匙串由
#     .github/workflows/ci.yml 建好、解锁（密码 `t`）、加进搜索列表，跑完恢复原列表并删掉：CI 的 macOS 上 codesign
#     只在搜索列表里的钥匙串找身份，只给 --keychain 时按名字、按 SHA-1 都报 no identity found（探针 CI run
#     43302065465）。改搜索列表只在 ci.yml 里做，这个脚本和 Rust 测试一概不改（审查 2/F-2）。
# 新建的钥匙串不自动上锁；密码是固定的 `t`，签名前可再解锁一次：security unlock-keychain -p t <钥匙串>
# 退出码：0 成功；2 用法错或不是 macOS；其他是建证书或导入失败（stderr 写明哪一步）。
set -euo pipefail

[ $# -eq 2 ] || { echo "用法：$0 <目录> <证书名>" >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "$0：只在 macOS 上用" >&2; exit 2; }
dir=$(cd "$1" && pwd -P)
name=$2

if [ -n "${IWORK_TEST_SIGNING_KEYCHAIN:-}" ]; then
  kc=$IWORK_TEST_SIGNING_KEYCHAIN
  [ -f "$kc" ] && [ ! -L "$kc" ] || { echo "$0：IWORK_TEST_SIGNING_KEYCHAIN=${kc} 不是已有的钥匙串文件" >&2; exit 2; }
  owned=no
  name="$name $(uuidgen | tr -d - | cut -c1-8)"
else
  kc="$dir/signing.keychain-db"
  owned=yes
  security create-keychain -p t "$kc"
  security unlock-keychain -p t "$kc"
  security set-keychain-settings "$kc"   # 不带参数 = 不自动上锁
fi

# 证书的 CN 最长 64 个字符（openssl 超长就失败）
[ "${#name}" -le 64 ] || { echo "$0：证书名「${name}」超过 64 个字符" >&2; exit 2; }
cat > "$dir/cert.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$name
OU=IWORKTEST
[v3]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$dir/cert.cnf" \
  -keyout "$dir/cert.key" -out "$dir/cert.pem" 2>"$dir/openssl.err" \
  || { echo "$0：openssl 建证书失败：" >&2; cat "$dir/openssl.err" >&2; exit 1; }
openssl pkcs12 -export -legacy -inkey "$dir/cert.key" -in "$dir/cert.pem" \
  -out "$dir/cert.p12" -passout pass:t
# 共用的钥匙串：并发的测试一个一个导入（锁文件在钥匙串旁边）。-e：导入失败就停，后一步成功也不能盖掉它（审查 3/F-3）
lockf -k "$kc.import-lock" /bin/sh -ec '
  security import "$1" -k "$2" -P t -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k t "$2" >/dev/null' sh "$dir/cert.p12" "$kc"
# 私钥已经进了钥匙串，目录里的明文私钥与 p12 不留
rm -f "$dir/cert.key" "$dir/cert.p12"

leaf=$(openssl x509 -in "$dir/cert.pem" -outform der | shasum -a 256 | cut -d' ' -f1)
sha1=$(openssl x509 -in "$dir/cert.pem" -outform der | shasum -a 1 | cut -d' ' -f1)
echo "keychain=$kc"
echo "owned=$owned"
echo "identity=$name"
echo "leaf_sha256=$leaf"
echo "sha1=$sha1"
