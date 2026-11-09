#!/usr/bin/env bash
# iwork-domain 里不许出现厂商名与 git 字样（任务卡 T01「注意」）：
# 版本控制用 baseline / workspace / vcs_kind，运行时用不透明的 RuntimeKind，厂商差异只在适配层。
# 退出码：0 通过；1 命中；2 环境问题。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$ROOT/crates/iwork-domain"
[ -d "$TARGET/src" ] || { echo "check-domain-terms: 找不到 $TARGET/src" >&2; exit 2; }
PATTERN='\b(claude|codex|antigravity|agy|gemini|git)\b'
# src 与 tests 都查：测试里写厂商名同样会把厂商概念带进领域层
DIRS=("$TARGET/src")
[ -d "$TARGET/tests" ] && DIRS+=("$TARGET/tests")
set +e
hits="$(grep -rniE "$PATTERN" "${DIRS[@]}")"
code=$?
set -e
[ "$code" -le 1 ] || { echo "check-domain-terms: grep 出错（退出码 $code）" >&2; exit 2; }
if [ "$code" -eq 0 ]; then
  echo "check-domain-terms: iwork-domain 里出现了厂商名或 git 字样："
  echo "$hits"
  exit 1
fi
echo "check-domain-terms: 通过"
