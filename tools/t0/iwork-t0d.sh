#!/usr/bin/env bash
# iWork T0d：收集 Codex 与 agy 的机器接口说明（只读，不发起任何模型调用，约 10 秒）
# 结果：~/iwork-t0d-results.tar.gz
set -u
OUT="$HOME/iwork-t0d-results"
rm -rf "$OUT"; mkdir -p "$OUT"
run() { local f="$1"; shift; { echo "\$ $*"; "$@"; echo "(exit=$?)"; } > "$OUT/$f.txt" 2>&1; }

run codex-version      codex --version
run codex-help         codex --help
run codex-exec-help    codex exec --help
run codex-appserver-help codex app-server --help
run codex-mcpserver-help codex mcp-server --help
run codex-sandbox-help codex sandbox --help
mkdir -p "$OUT/codex-appserver-schema"
run codex-schema-json  codex app-server generate-json-schema --out "$OUT/codex-appserver-schema"
run codex-schema-ts    codex app-server generate-ts --out "$OUT/codex-appserver-schema/ts"

run agy-version        agy --version
run agy-help           agy --help
run agy-acp-help       agy acp --help

run claude-help        claude --help

tar -czf "$HOME/iwork-t0d-results.tar.gz" -C "$HOME" iwork-t0d-results
echo "完成。请把 $HOME/iwork-t0d-results.tar.gz 发给 Claude。"
