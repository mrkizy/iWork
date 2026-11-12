#!/usr/bin/env bash
# 测试用：在给定目录里现建一个临时钥匙串，导入一张自签的代码签名证书（macOS）。T04p 方案 4.4、R10 的测试与 CI 的
# 形态 B 打包共用这一个建法（做法照 tools/t0/iwork-t04-bundle-probe.sh 155–172 行，N5 量过）。
#
# 用法：scripts/test-signing-identity.sh <目录> <证书名>
# 输出四行（stdout）：
#   keychain=<钥匙串文件的绝对路径>
#   identity=<证书名>
#   leaf_sha256=<叶证书 DER 的 SHA-256，64 个十六进制位>
#   sha1=<叶证书 DER 的 SHA-1，40 个十六进制位>：签名时用它当 codesign -s 的身份，指定的恰好是这一张（不怕同名）
#
# 默认只动 <目录> 里的东西：钥匙串不加进当前用户的搜索列表（`security create-keychain` 不改搜索列表，本机实测），
# 不设默认钥匙串，不改登录钥匙串。签名时由调用方给 `codesign --keychain <钥匙串>`。
# 环境变量 IWORK_TEST_SIGNING_SEARCH_LIST=1（只有 CI 设）时另把钥匙串加进用户的搜索列表：CI 的 macOS 上 codesign
# 只在搜索列表里的钥匙串找身份，只给 --keychain 时按名字、按 SHA-1 都报 no identity found；加进列表后不受信任的自签
# 证书也照用（探针 CI run 43302065465）。本机（macOS 27.2）只给 --keychain 就能签，所以本机默认不碰搜索列表。
# 用完删（删钥匙串同时把它移出搜索列表）：
#   security delete-keychain <钥匙串>
# 钥匙串关了自动上锁（默认空闲 5 分钟、睡眠就锁，之后 codesign 会弹框要密码）。密码是固定的 `t`，签名前可再解锁一次：
#   security unlock-keychain -p t <钥匙串>
# 退出码：0 成功；2 用法错或不是 macOS；其他是建证书或导入失败（stderr 写明哪一步）。
set -euo pipefail

[ $# -eq 2 ] || { echo "用法：$0 <目录> <证书名>" >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "$0：只在 macOS 上用" >&2; exit 2; }
dir=$(cd "$1" && pwd -P)
name=$2
kc="$dir/signing.keychain-db"

security create-keychain -p t "$kc"
security unlock-keychain -p t "$kc"
security set-keychain-settings "$kc"   # 不带参数 = 不自动上锁

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
  -keyout "$dir/cert.key" -out "$dir/cert.pem" 2>/dev/null
openssl pkcs12 -export -legacy -inkey "$dir/cert.key" -in "$dir/cert.pem" \
  -out "$dir/cert.p12" -passout pass:t
security import "$dir/cert.p12" -k "$kc" -P t -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k t "$kc" >/dev/null
# 私钥已经进了钥匙串，目录里的明文私钥与 p12 不留
rm -f "$dir/cert.key" "$dir/cert.p12"
if [ "${IWORK_TEST_SIGNING_SEARCH_LIST:-}" = 1 ]; then
  # 读出现有列表、把新钥匙串放到最前再写回；测试会并发建钥匙串，读改写之间加锁
  lockf -k /tmp/iwork-keychain-search.lock /bin/bash -c '
    list=()
    while IFS= read -r k; do k=${k#*\"}; k=${k%\"*}; [ -n "$k" ] && list+=("$k"); done < <(security list-keychains -d user)
    security list-keychains -d user -s "$1" ${list[@]+"${list[@]}"}' sh "$kc"
fi

leaf=$(openssl x509 -in "$dir/cert.pem" -outform der | shasum -a 256 | cut -d' ' -f1)
sha1=$(openssl x509 -in "$dir/cert.pem" -outform der | shasum -a 1 | cut -d' ' -f1)
echo "keychain=$kc"
echo "identity=$name"
echo "leaf_sha256=$leaf"
echo "sha1=$sha1"
