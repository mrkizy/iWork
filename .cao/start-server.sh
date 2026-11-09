#!/bin/sh
set -eu
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export CAO_HOME_DIR="$PROJECT_ROOT/.cao"
export CAO_API_PORT=9895
# Deliver inbox messages while a worker works when its CLI takes mid-turn input (catalog midTurnInput).
export CAO_EAGER_INBOX_DELIVERY=true
# CAO's learning is opt-in: without this, report_outcome and store_lesson only answer "disabled" and record nothing.
export CAO_MEMORY_LEARNING_ENABLED=true
cd "$PROJECT_ROOT"
exec cao-server --port 9895 "$@"
