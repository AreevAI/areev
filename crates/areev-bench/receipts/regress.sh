#!/bin/sh
# The verify-then-revert leg over a learned memory (see regress.py).
#
#   regress.sh <learned-db> <workdir> [regress.py args...]
#
# Requires: run.py ran with --measure --journal-baseline, and eval.sh
# journaled arm B (`--journal B=eval-b`). Only the agent leg is paid; the
# harmful lesson comes from a fixture through the keyless mock loop model.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
LEARNED="$1"; WORKDIR="$2"; shift 2
exec "$PY" "$HERE/regress.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$LEARNED" \
  --workdir "$WORKDIR" --seed "$SEED" "$@"
