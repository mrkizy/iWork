# 回退窗口一格怎么判（T04p 方案 9.8；审查 1/F-6）。由 scripts/drill-release-bundle.sh 用 `source` 引入，只做判断、不碰外界，
# 测试直接喂证据行（crates/iwork-daemon/tests/drill_window.rs）。
#
# window_verdict <期望 connected|rejected> <t1 毫秒> <t1 到 t4 的上限毫秒> <B 版就绪 yes|no> <t4 之后的后台 A|other|unknown>
#   stdin：后台日志里 t1 之后的相关记录，一行一条「<日志行号> <ts 毫秒> <标签>」，按日志行号升序。标签：
#     connected     这个客户端连上了                rejected  这个客户端被拒
#     restart       磁盘上装齐了另一版，自行重启     reject_check  有客户端被拒，立刻做一次安装检查
#     dropped       日志里说丢了记录
#   取证：t2 = 第一条 connected/rejected；t3 = 第一条 restart；t4 = 行号在 t3 之后的第一条 connected。
#   输出一行：PASS / FAIL / INCONCLUSIVE，后面跟理由与四个时刻。
#   缺证据、证据自相矛盾（时刻不严格递增、日志里的先后和时刻对不上、日志丢了记录、B 版没就绪、没取到 t4 之后的映像）
#   → INCONCLUSIVE（这一次量不出窗口，重来）；证据齐全但行为不对 → FAIL。
window_verdict() {
  local expect=$1 t1=$2 limit=$3 ready=$4 image=$5
  local l2="" t2="" kind="" l3="" t3="" l4="" t4="" checked=no rejected=no dropped=no line ts tag
  while read -r line ts tag; do
    case "$tag" in
      connected|rejected)
        [ "$tag" = rejected ] && rejected=yes
        if [ -z "$l2" ]; then
          l2=$line t2=$ts kind=$tag
        elif [ "$tag" = connected ] && [ -n "$l3" ] && [ -z "$l4" ] && [ "$line" -gt "$l3" ]; then
          l4=$line t4=$ts
        fi
        ;;
      restart) [ -n "$l3" ] || { l3=$line t3=$ts; } ;;
      reject_check) [ -n "$l2" ] && checked=yes ;;   # 行号在 t2 之后
      dropped) dropped=yes ;;
      *) echo "INCONCLUSIVE 不认识的证据标签：${tag}"; return 0 ;;
    esac
  done
  local times="t1=${t1} t2=${t2:-无}（${kind:-无}） t3=${t3:-无} t4=${t4:-无}"
  _wv() { printf '%s %s；%s\n' "$1" "$2" "$times"; }
  [ "$ready" = yes ] || { _wv INCONCLUSIVE "回退前 B 版后台没就绪"; return 0; }
  [ -n "$l2" ] && [ -n "$l3" ] && [ -n "$l4" ] || { _wv INCONCLUSIVE "缺 t2、t3 或 t4"; return 0; }
  [ "$t1" -lt "$t2" ] && [ "$t2" -lt "$t3" ] && [ "$t3" -lt "$t4" ] \
    || { _wv INCONCLUSIVE "时刻不是严格的 t1<t2<t3<t4"; return 0; }
  [ "$l2" -lt "$l3" ] && [ "$l3" -lt "$l4" ] || { _wv INCONCLUSIVE "日志里的先后和时刻对不上"; return 0; }
  [ "$dropped" = no ] || { _wv INCONCLUSIVE "日志丢了记录"; return 0; }
  case "$image" in
    A) ;;
    unknown) _wv INCONCLUSIVE "没取到 t4 之后后台的映像"; return 0 ;;
    *) _wv FAIL "t4 之后后台不是原来那个进程跑 A 版"; return 0 ;;
  esac
  [ "$kind" = "$expect" ] || { _wv FAIL "t2 是 ${kind}，期望 ${expect}"; return 0; }
  [ $((t4 - t1)) -le "$limit" ] || { _wv FAIL "t1 到 t4 超过 ${limit} 毫秒"; return 0; }
  if [ "$expect" = rejected ]; then
    [ "$checked" = yes ] || { _wv FAIL "被拒之后没有立刻做安装检查"; return 0; }
  else
    [ "$rejected" = no ] || { _wv FAIL "客户端被拒过"; return 0; }
  fi
  _wv PASS "窗口量到了"
}
