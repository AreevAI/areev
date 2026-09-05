#!/bin/sh
# One PAST-Bench family for one agent, both persistence conditions, every
# model leg pinned and seeded. The same command for every arm, so the only
# thing that differs between runs is AGENT.
#
#   AGENT=areev-governed FAMILY=memory_ability/SM01_preference_adoption OUT=$HOME/runs/x sh evolve.sh
#
#   AGENT         areev-governed | areev-passive | hermes | hermes-plus (| nanobot | zeroclaw)
#   FAMILY        <ability_dir>/<family_id> under self-evolve-tasks-v2/
#   OUT           trace dir; usage.jsonl (the loop/review legs) lands beside the traces
#   SEED          request seed for every leg (default 1)
#   AGENT_MODEL   the agent (default qwen3-30b, pinned to coreweave/bf16 via AGENT_PIN)
#   JUDGE_MODEL   default minimax/minimax-m2.7 through OpenRouter — the paper's judge
#   PASTBENCH_DIR the benchmark checkout (default ~/mg/local/PAST-Bench)
#
# Extra arguments go to `past-bench evolve` unchanged.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
PB="${PASTBENCH_DIR:-$HOME/mg/local/PAST-Bench}"
AGENT="${AGENT:?set AGENT}"; FAMILY="${FAMILY:?set FAMILY}"; OUT="${OUT:?set OUT}"
SEED="${SEED:-1}"
MODEL="${AGENT_MODEL:-qwen/qwen3-30b-a3b-instruct-2507}"
JUDGE="${JUDGE_MODEL:-minimax/minimax-m2.7}"
SCRIPTS="$REPO/crates/areev-bench/scripts"
# The receipts runs pinned coreweave/bf16; on 2026-09-06 OpenRouter listed no
# CoreWeave endpoint for this model any more (StreamLake, SiliconFlow fp8,
# Nebius fp8, Alibaba remained). siliconflow/fp8 was tried first and REJECTED:
# on the benchmark's opening request it returns finish=stop with empty content
# and exactly the 28 output tokens of the tool call the other endpoints return
# — its tool-call parsing swallows the call (replayed with pinprobe2.py:
# StreamLake and Alibaba both return `notes_list`). streamlake is the pin:
# cheapest, and the same price the cost table pinned for this model.
# Quantization undeclared by the provider — recorded per run.
export AREEV_AGENT_PIN="${AGENT_PIN:-streamlake}"
export AREEV_LOOP_LLM_CMD="${AREEV_LOOP_LLM_CMD:-python3 $SCRIPTS/openrouter_loop.py $MODEL --provider $AREEV_AGENT_PIN --seed $SEED}"
export AREEV_LOOP_GROUND_CMD="${AREEV_LOOP_GROUND_CMD:-python3 $SCRIPTS/openrouter_loop.py openai/gpt-4o-mini --provider openai --seed $SEED}"
export AREEV_REVIEW_CMD="${AREEV_REVIEW_CMD:-python3 $SCRIPTS/openrouter_toolcall.py openai/gpt-4o --provider openai --seed $SEED}"
export SEED PYTHONUNBUFFERED=1
mkdir -p "$OUT"
export AREEV_USAGE_LOG="$OUT/usage.jsonl"
if [ -z "${OPENROUTER_API_KEY:-}" ] && [ -f "$HOME/mg/local/dev-areev.env" ]; then
  set -a; . "$HOME/mg/local/dev-areev.env"; set +a
fi
cd "$PB"
. .venv/bin/activate
REG=""; PROFILE=""
case "$AGENT" in
  areev*|mem0*) REG="--registry $HERE/agents.yaml" ;;
  *) PROFILE="--agent-profile openrouter" ;;
esac
# shellcheck disable=SC2086
exec python "$HERE/run.py" evolve --family "$FAMILY" --agent "$AGENT" $REG $PROFILE \
  --config "$HERE/config.persist.yaml" --model "$MODEL" \
  --runtime local --sandbox --sandbox-tools --compare-no-persistence \
  --judge-model "$JUDGE" --trace-dir "$OUT" "$@"
