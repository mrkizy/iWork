#!/usr/bin/env bash
# iWork 自己的进程不开任何 TCP/UDP 端口（AGENTS.md 安全不变量；T03 方案第 4 节、第 9 节）。
#
# 查 crates/ 与 client/src-tauri/ 下全部 Rust 源码里的 TCP/UDP 类型与 std::net 用法。
# 它证明不了：依赖库自己开端口（由 scripts/deps-policy.toml 的 io_and_framework 传递禁用挡 hyper、reqwest 等，
# 新加依赖时审查看闭包）、运行时实际监听状态（归 T11 故障矩阵）、网页层（不是 Rust，也不是 iWork 的进程边界）。
#
# 用法：scripts/check-no-tcp.sh            检查仓库
#       scripts/check-no-tcp.sh --selftest 阳性对照：注入一行 TcpListener::bind 必须报红
# 退出码：0 通过；1 命中；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATTERN='\b(TcpListener|TcpStream|TcpSocket|UdpSocket)\b|std::net::|tokio::net::(tcp|udp)\b|\bsocket2::|mio::net::(Tcp|Udp)'

# scan DIR...：命中打印并返回 1，干净返回 0，grep 出错返回 2。
scan() {
  local hits code=0
  hits="$(grep -rnE --include='*.rs' --exclude-dir=target "$PATTERN" "$@")" || code=$?
  if [ "$code" -gt 1 ]; then
    echo "check-no-tcp: grep 出错（退出码 ${code}）" >&2
    return 2
  fi
  if [ "$code" -eq 0 ]; then
    echo "check-no-tcp: 发现 TCP/UDP 用法（iWork 进程只许用 Unix domain socket / 命名管道）："
    echo "$hits"
    return 1
  fi
  return 0
}

if [ "${1:-}" = "--selftest" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/clean" "$tmp/dirty"
  echo 'fn main() { let _ = 1; }' > "$tmp/clean/a.rs"
  echo 'fn main() { let _l = tokio::net::TcpListener::bind("127.0.0.1:0"); }' > "$tmp/dirty/a.rs"
  scan "$tmp/clean" >/dev/null || { echo "check-no-tcp selftest：干净目录被误报" >&2; exit 1; }
  code=0
  scan "$tmp/dirty" >/dev/null || code=$?
  [ "$code" -eq 1 ] || { echo "check-no-tcp selftest：注入 TcpListener::bind 没有报红（退出码 ${code}）" >&2; exit 1; }
  echo "check-no-tcp selftest：通过（干净目录 0 处，注入一行必红）"
  exit 0
fi

DIRS=("$ROOT/crates" "$ROOT/client/src-tauri")
for d in "${DIRS[@]}"; do
  [ -d "$d" ] || { echo "check-no-tcp: 找不到 ${d}" >&2; exit 2; }
done
code=0
scan "${DIRS[@]}" || code=$?
[ "$code" -eq 0 ] && echo "check-no-tcp: 通过（0 处）"
exit "$code"
