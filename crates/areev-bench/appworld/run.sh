#!/bin/sh
# Run one AppWorld experiment by config name and evaluate it.
#
#   sh run.sh simplified_react_code_agent/alibaba/qwen3-30b-a3b-instruct-2507_openrouter/dev_probe dev_probe
#
# Goes through run_experiment.py rather than `appworld run` so the OpenRouter
# upstream is pinned ($AREEV_OR_PROVIDER, default streamlake).
#
# Wall clock and the harness's own cost meter are printed at the end: the
# probe exists to measure them as much as to measure the score.
set -e
NAME="${1:?usage: run.sh <experiment-name> <dataset>}"
DATASET="${2:?usage: run.sh <experiment-name> <dataset>}"
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/env.sh"
cd "$APPWORLD_ROOT"

started=$(date +%s)
"$PY" "$HERE/run_experiment.py" "$NAME" --root "$APPWORLD_ROOT" --provider "${AREEV_OR_PROVIDER:-streamlake}"
elapsed=$(( $(date +%s) - started ))

echo
echo "=== wall clock: ${elapsed}s ==="
"$APPWORLD" evaluate "$NAME" "$DATASET"
