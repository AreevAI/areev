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
LEARNED="$1"; WORKDIR="$2"; NAME="$3"; ADAPTER="${4:-none}"; PORT="${5:-}"
# A fixed port let a second evaluation silently talk to another run's server:
# its own server failed to bind, the health check found the other one alive,
# and a different adapter's answers were scored under this one's name. Pick
# a free port per invocation unless the caller insists.
[ -n "$PORT" ] || PORT=$((8200 + $$ % 700))
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
mkdir -p "$WORKDIR"
if [ "$ADAPTER" != "none" ] && [ ! -s "$ADAPTER/adapters.safetensors" ]; then
  echo "slm_eval: $ADAPTER has no adapters.safetensors -- refusing to evaluate an empty adapter as a tuned model" >&2
  exit 1
fi
start_server() {
  if [ "$ADAPTER" = "none" ]; then
    python3 -m mlx_lm server --model "$BASE" --port "$PORT" --chat-template-args '{"enable_thinking": false}' > "$WORKDIR/server.log" 2>&1 &
  else
    python3 -m mlx_lm server --model "$BASE" --adapter-path "$ADAPTER" --port "$PORT" --chat-template-args '{"enable_thinking": false}' > "$WORKDIR/server.log" 2>&1 &
  fi
  SRV=$!
}
trap 'kill $SRV 2>/dev/null || true' EXIT
up=0; attempt=0
while [ "$up" = 0 ] && [ "$attempt" -lt 3 ]; do
  attempt=$((attempt + 1)); start_server
  for i in $(seq 1 90); do
    if grep -q "Address already in use" "$WORKDIR/server.log" 2>/dev/null; then
      # A caller pinning one port for back-to-back evaluations can hit the previous
      # server's release window; wait and retry before calling it fatal -- but never
      # score against whatever is on the port, which is what happened once.
      kill "$SRV" 2>/dev/null || true; sleep 4; break
    fi
    if ! kill -0 "$SRV" 2>/dev/null; then echo "slm_eval: server exited before it was ready (see $WORKDIR/server.log)" >&2; exit 1; fi
    curl -s -o /dev/null "http://127.0.0.1:$PORT/v1/models" && { up=1; break; }
    sleep 1
  done
done
[ "$up" = 1 ] || { echo "slm_eval: could not bind port $PORT after $attempt attempt(s) -- refusing to score against another server" >&2; exit 1; }
export AGENT_CMD="$PY $SCRIPTS/slm_serve.py $NAME --port $PORT --seed $SEED --model $BASE"
"$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$LEARNED" \
  --workdir "$WORKDIR" --seed "$SEED" --experience "${EXP:-40}" --eval "${EVAL:-60}" --arms "${ARMS:-B,B2}" --holdout "${HOLDOUT:-unseen}" --upto-seq "${UPTO:-0}"
