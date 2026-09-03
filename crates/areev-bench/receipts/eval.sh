#!/bin/sh
# Paired evaluation over held-out receipts.
#
#   eval.sh <learned-db> <workdir> [evaluate.py args...]
#
# Only the agent leg is needed here: no learning happens during evaluation,
# which is the point — the rule set is frozen before this starts.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
LEARNED="$1"; WORKDIR="$2"; shift 2
DATASET="${DATASET:-$HERE/data/sroie.jsonl}"
exec "$PY" "$HERE/evaluate.py" --dataset "$DATASET" --learned-db "$LEARNED" \
  --workdir "$WORKDIR" --seed "$SEED" "$@"
