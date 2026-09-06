#!/bin/sh
# Evaluate a small model — tuned or not — as the PAST-Bench agent on a list of
# families, through the same adapter and runner as every other arm.
#
#   ADAPTER=/path/to/adapter|none FAMILIES="..." ROOT=$HOME/runs/slm-eval sh eval_pastbench.sh
#
# Starts vLLM on the base (plus the LoRA when given) on a free port, waits
# for it, runs `pilot.sh` with the agent pointed at it (AGENT_BASE_URL, no
# provider pin), stops the server. The paired `--compare-no-persistence`
# run gives both readings at once: with_persistence = the tuned model with
# the memory in its prompt; without_persistence = the tuned model with NO
# persistence read — the memory only in its weights. `ADAPTER=none` is the
# untuned control on the identical path.
#
#   AGENT       areev-passive (default) — the memory rendered, no loop
#   SLM_BASE    default Qwen/Qwen3-1.7B
#   PORT        default 8300
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ADAPTER="${ADAPTER:?set ADAPTER (dir or none)}"; FAMILIES="${FAMILIES:?}"; ROOT="${ROOT:?}"
AGENT="${AGENT:-areev-passive}"; PORT="${PORT:-8300}"
mkdir -p "$ROOT"
sh "$HERE/slm_serve_cuda.sh" "$ADAPTER" "$PORT" > "$ROOT/vllm.log" 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT INT TERM
i=0
until curl -s "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1; do
  i=$((i+1)); [ $i -gt 180 ] && { echo "vLLM did not come up (see $ROOT/vllm.log)" >&2; exit 1; }
  kill -0 $SERVER 2>/dev/null || { echo "vLLM exited (see $ROOT/vllm.log)" >&2; exit 1; }
  sleep 2
done
echo "vLLM up on :$PORT (adapter: $ADAPTER)"
# Watchdog: the first attempt lost its server mid-family and the runner then
# hung on a request that never returns — silent for hours. If the server
# dies, kill this script's process group so the loss is loud and immediate.
( while kill -0 $SERVER 2>/dev/null; do sleep 30; done
  echo "vLLM exited during the run — aborting (see $ROOT/vllm.log)" >&2
  kill -TERM -$$ 2>/dev/null ) &
WATCHDOG=$!
# stop the watchdog before the trap stops the server, or a NORMAL finish
# looks like a crash and the abort message lands on a completed run
trap 'kill $WATCHDOG 2>/dev/null; kill $SERVER 2>/dev/null || true' EXIT INT TERM
AGENT_MODEL=slm AGENT_PIN="" AGENT_BASE_URL="http://127.0.0.1:$PORT/v1" AGENT_API_KEY=local \
  FAMILIES="$FAMILIES" AGENTS="$AGENT" ROOT="$ROOT" sh "$HERE/../pastbench/pilot.sh"
kill $WATCHDOG 2>/dev/null || true
