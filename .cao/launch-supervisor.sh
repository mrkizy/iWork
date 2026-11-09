#!/bin/sh
set -eu
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export CAO_HOME_DIR="$PROJECT_ROOT/.cao"
export CAO_API_PORT=9895
cd "$PROJECT_ROOT"
if [ -n "${TMUX:-}" ]; then
  SESSION="cao-supervisor-$(date +%Y%m%d-%H%M%S)"
  cao launch --agents code_supervisor --provider claude_code --headless --session-name "$SESSION" "$@"
  tmux switch-client -t "$SESSION" || {
    echo "supervisor 会话 $SESSION 已创建，但没能切换过去。在 tmux 外运行：tmux attach -t $SESSION" >&2
    exit 1
  }
  exit 0
fi
exec cao launch --agents code_supervisor --provider claude_code "$@"
