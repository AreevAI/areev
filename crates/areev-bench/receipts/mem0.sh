#!/bin/sh
# The plain-memory arm (see mem0_arm.py).
#
#   PROFILE=sroie SEED=1 mem0.sh <workdir> [mem0_arm.py args...]
#
# mem0's own model calls use the LEARNER model by default, so the comparison
# with the governed arm is architecture against architecture, not two budgets.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
WORKDIR="$1"; shift
export MEM0_LLM_MODEL="${MEM0_LLM_MODEL:-$LEARNER_MODEL}"
exec "$PY" "$HERE/mem0_arm.py" --profile "$PROFILE" --dataset "$DATASET" --workdir "$WORKDIR" --seed "$SEED" "$@"
