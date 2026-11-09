#!/usr/bin/env bash
# iWork T0 补测（Q6）：PreToolUse hook 长时间等待时，Claude Code 的超时行为
# 用例 A：hook 不设 timeout，等待 90 秒后返回 deny
# 用例 B：hook 设 timeout=7200 秒，等待 720 秒后返回 deny
# 关键看两点：hook 是否被提前打断；被打断后命令是被拒绝还是被放行（失败即开放 / 失败即关闭）。
# 全程约 14 分钟，发起 2 次一句话的 claude -p 调用。只执行无害的 echo。
# 结果写到 ~/iwork-t0b-results/，跑完把 ~/iwork-t0b-results.tar.gz 发给 Claude。

set -u
OUT="$HOME/iwork-t0b-results"
rm -rf "$OUT"; mkdir -p "$OUT"
WORK="$(mktemp -d)"
log() { echo "== [$(date +%H:%M:%S)] $*" | tee -a "$OUT/summary.txt"; }

log "claude version: $(claude --version 2>&1)"

make_case() {  # $1=名称 $2=等待秒数 $3=timeout 字段（空表示不设）
  local name="$1" wait="$2" tmo="$3"
  local hook="$WORK/hook-${name}.sh"
  cat > "$hook" <<SH
#!/usr/bin/env bash
cat > /dev/null
echo "start \$(date +%s)" >> "$OUT/hook-${name}.log"
sleep $wait
echo "end \$(date +%s)" >> "$OUT/hook-${name}.log"
echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"iwork-t0b-${name} deny"}}'
SH
  chmod +x "$hook"
  local tfield=""
  [ -n "$tmo" ] && tfield=", \"timeout\": $tmo"
  cat > "$WORK/settings-${name}.json" <<JSON
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "$hook"$tfield } ] } ] } }
JSON
}

run_case() {
  local name="$1"
  local t0=$(date +%s)
  ( cd "$WORK" && claude -p "Use the Bash tool to run exactly: echo hello-iwork-${name} . Then report the output." \
      --settings "$WORK/settings-${name}.json" \
      --output-format stream-json --verbose > "$OUT/${name}-stream.jsonl" 2> "$OUT/${name}-stderr.txt" )
  local t1=$(date +%s)
  log "用例 ${name}：总耗时 $((t1 - t0)) 秒"
  log "用例 ${name}：hook 日志 $(tr '\n' ' ' < "$OUT/hook-${name}.log" 2>/dev/null)"
  log "用例 ${name}：命令被执行（出现 hello-iwork-${name} 的 tool_result）: $(grep -c "\"content\": *\"hello-iwork-${name}" "$OUT/${name}-stream.jsonl")"
  log "用例 ${name}：hook 拒绝生效: $(grep -c "iwork-t0b-${name} deny" "$OUT/${name}-stream.jsonl")"
  log "用例 ${name}：出现 timeout 字样: $(grep -ci 'timeout\|timed out' "$OUT/${name}-stream.jsonl" "$OUT/${name}-stderr.txt" | tr '\n' ' ')"
}

make_case A 90 ""
make_case B 720 7200

log "开始用例 A（约 1.5 分钟）"
run_case A
log "开始用例 B（约 12 分钟）"
run_case B

rm -rf "$WORK"
tar -czf "$HOME/iwork-t0b-results.tar.gz" -C "$HOME" iwork-t0b-results
log "完成。请把 $HOME/iwork-t0b-results.tar.gz 发给 Claude。"
