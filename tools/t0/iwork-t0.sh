#!/usr/bin/env bash
# iWork T0 实测脚本：验证 Claude Code 的三项行为（Q3 额度、Q4 登录检测、Q5 hook 优先级）
# 只读取 Claude Code 的公开命令与输出，不读取任何凭据文件。
# 会发起 3 次极短的 claude -p 调用（各一句话），消耗很少的额度。
# 结果写到 ~/iwork-t0-results/，跑完后把 ~/iwork-t0-results.tar.gz 发给 Claude。

set -u
OUT="$HOME/iwork-t0-results"
rm -rf "$OUT"; mkdir -p "$OUT"
WORK="$(mktemp -d)"
log() { echo "== $*" | tee -a "$OUT/summary.txt"; }

log "date: $(date)"
log "os: $(uname -a)"
log "claude path: $(command -v claude || echo NOT_FOUND)"
claude --version > "$OUT/version.txt" 2>&1; log "version: $(cat "$OUT/version.txt")"
claude --help > "$OUT/help.txt" 2>&1
claude auth --help > "$OUT/auth-help.txt" 2>&1

# ---------- Q4 登录状态检测 ----------
log "Q4: claude auth status"
claude auth status > "$OUT/q4-auth-status.txt" 2>&1; echo "exit=$?" >> "$OUT/q4-auth-status.txt"
claude auth status --json > "$OUT/q4-auth-status-json.txt" 2>&1; echo "exit=$?" >> "$OUT/q4-auth-status-json.txt"

# ---------- Q3 额度信息：看 stream-json 输出里有哪些字段 ----------
log "Q3: stream-json 输出"
( cd "$WORK" && claude -p "Reply with the single word: ok" \
    --output-format stream-json --verbose > "$OUT/q3-stream.jsonl" 2> "$OUT/q3-stderr.txt" )
echo "exit=$?" >> "$OUT/q3-stderr.txt"
grep -o '"[a-z_]*\(rate\|limit\|quota\|usage\|reset\)[a-z_]*"' "$OUT/q3-stream.jsonl" | sort | uniq -c > "$OUT/q3-fields.txt"

# ---------- Q5 hook 是否先于用户放行规则 ----------
mkdir -p "$WORK/.claude"
cat > "$WORK/.claude/settings.json" <<'JSON'
{ "permissions": { "allow": ["Bash(echo:*)", "Bash(echo hello-iwork)"] } }
JSON

HOOK="$WORK/hook.sh"
cat > "$HOOK" <<SH
#!/usr/bin/env bash
cat >> "$OUT/q5-hook-calls.log"
echo >> "$OUT/q5-hook-calls.log"
echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"iwork-t0 deny"}}'
SH
chmod +x "$HOOK"

cat > "$WORK/iwork-settings.json" <<JSON
{ "hooks": { "PreToolUse": [ { "matcher": "*", "hooks": [ { "type": "command", "command": "$HOOK" } ] } ] } }
JSON

log "Q5-A: 只有放行规则（预期 echo 被执行）"
( cd "$WORK" && claude -p "Use the Bash tool to run exactly: echo hello-iwork . Then report the output." \
    --output-format stream-json --verbose > "$OUT/q5a-stream.jsonl" 2> "$OUT/q5a-stderr.txt" )

log "Q5-B: 放行规则 + 注入的 deny hook（预期 hook 被调用且命令被拒）"
( cd "$WORK" && claude -p "Use the Bash tool to run exactly: echo hello-iwork . Then report the output." \
    --settings "$WORK/iwork-settings.json" \
    --output-format stream-json --verbose > "$OUT/q5b-stream.jsonl" 2> "$OUT/q5b-stderr.txt" )

log "Q5 hook 调用次数: $(grep -c 'tool_name' "$OUT/q5-hook-calls.log" 2>/dev/null || echo 0)"
log "Q5-A 输出里出现 hello-iwork: $(grep -c 'hello-iwork' "$OUT/q5a-stream.jsonl")"
log "Q5-B 输出里出现 iwork-t0 deny: $(grep -c 'iwork-t0 deny' "$OUT/q5b-stream.jsonl")"

# ---------- 选项是否存在 ----------
for f in --permission-prompt-tool --settings --setting-sources --input-format --resume --append-system-prompt --mcp-config; do
  if grep -q -- "$f" "$OUT/help.txt"; then log "flag $f: yes"; else log "flag $f: no"; fi
done

rm -rf "$WORK"
tar -czf "$HOME/iwork-t0-results.tar.gz" -C "$HOME" iwork-t0-results
log "完成。请把 $HOME/iwork-t0-results.tar.gz 发给 Claude。"
