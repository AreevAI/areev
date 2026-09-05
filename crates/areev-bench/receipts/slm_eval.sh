#!/bin/sh
# Evaluate a small model — tuned or not — as the agent, on a seed's held-out set,
# with the governed rules in the prompt exactly as arm B has them.
#
#   SEED=1 slm_eval.sh <learned-db> <workdir> <name> [adapter-path|none] [port 8081]
#
# Starts mlx_lm.server on the base (plus the adapter when given), waits for it,
# runs evaluate.py with AGENT_CMD pointed at slm_serve.py, stops the server.
# `none` evaluates the UNTUNED base with the same prompt: that is the control
# that separates "a small model with rules" from "a small model tuned on the
# governed corpus", which is the claim the arm exists to test.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
LEARNED="$1"; WORKDIR="$2"; NAME="$3"; ADAPTER="${4:-none}"; PORT="${5:-8081}"
BASE="${SLM_BASE:-mlx-community/Qwen3.5-2B-4bit}"
mkdir -p "$WORKDIR"
if [ "$ADAPTER" = "none" ]; then
  python3 -m mlx_lm server --model "$BASE" --port "$PORT" > "$WORKDIR/server.log" 2>&1 &
else
  python3 -m mlx_lm server --model "$BASE" --adapter-path "$ADAPTER" --port "$PORT" > "$WORKDIR/server.log" 2>&1 &
fi
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
for i in $(seq 1 60); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/v1/models" && break
  sleep 1
done
export AGENT_CMD="$PY $SCRIPTS/slm_serve.py $NAME --port $PORT --seed $SEED --model $BASE"
"$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$LEARNED" \
  --workdir "$WORKDIR" --seed "$SEED" --experience "${EXP:-40}" --eval "${EVAL:-60}" --arms "${ARMS:-B,B2}" --holdout "${HOLDOUT:-unseen}"
