#!/bin/sh
# The governed learning run over public receipts.
#
#   learn.sh <workdir> [run.py args...]
#
# Experience phase only: the agent reads receipts, the accountant corrects,
# the loop proposes, the reviewer decides, approved lessons render into the
# prompt. Leaves <workdir>/ledger.db (and snapshots) for eval.sh.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
WORKDIR="$1"; shift
exec "$PY" "$HERE/run.py" --profile "$PROFILE" --dataset "$DATASET" --workdir "$WORKDIR" --seed "$SEED" "$@"
