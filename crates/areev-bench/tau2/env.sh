#!/bin/sh
# Shared environment for the τ² retail harness. Sourced by the drivers.
#
#   TAU2      the tau2-bench checkout (default ~/mg/local/tau2-bench)
#   TAU2_PY   a python with tau2 AND the areev binding importable
#   AGENT_MODEL / USER_LLM / LEARNER_MODEL / GROUND_MODEL / REVIEW_MODEL
#
# Every leg is provider-pinned: unpinned routing moved 5 of 60 held-out tasks
# between two byte-identical runs of the synthetic bench, and an effect you
# cannot reproduce is not an effect.
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
TAU2="${TAU2:-$HOME/mg/local/tau2-bench}"
TAU2_PY="${TAU2_PY:-$HOME/mg/local/tau2-venv/bin/python3}"
SCRIPTS="$REPO/crates/areev-bench/scripts"
SEED="${SEED:-300}"

AGENT_MODEL="${AGENT_MODEL:-qwen/qwen3-30b-a3b-instruct-2507}"
AGENT_PIN="${AGENT_PIN:-coreweave/bf16}"
LEARNER_MODEL="${LEARNER_MODEL:-openai/gpt-oss-120b}"
LEARNER_PIN="${LEARNER_PIN:-deepinfra/bf16}"
GROUND_MODEL="${GROUND_MODEL:-openai/gpt-4o-mini}"
GROUND_PIN="${GROUND_PIN:-openai}"
REVIEW_MODEL="${REVIEW_MODEL:-openai/gpt-4o}"
REVIEW_PIN="${REVIEW_PIN:-openai}"

if [ -f "$HOME/mg/local/dev-areev.env" ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  set -a
  . "$HOME/mg/local/dev-areev.env"
  set +a
fi

# The customer is τ²-bench's own user simulator, driven through litellm. It is
# a model too, which is why the B-vs-B2 noise floor is load-bearing here.
export USER_LLM="${USER_LLM:-openrouter/qwen/qwen3-30b-a3b-instruct-2507}"
DEFAULT_USER_ARGS='{"temperature":0,"extra_body":{"provider":{"order":["coreweave/bf16"],"allow_fallbacks":false}}}'
export USER_LLM_ARGS="${USER_LLM_ARGS:-$DEFAULT_USER_ARGS}"

export AGENT_CMD="${AGENT_CMD:-$TAU2_PY $SCRIPTS/openrouter_toolcall.py $AGENT_MODEL --provider $AGENT_PIN --seed $SEED}"
export LOOP_LLM_CMD="${LOOP_LLM_CMD:-$TAU2_PY $SCRIPTS/openrouter_loop.py $LEARNER_MODEL --provider $LEARNER_PIN --seed $SEED}"
export LOOP_GROUND_CMD="${LOOP_GROUND_CMD:-$TAU2_PY $SCRIPTS/openrouter_loop.py $GROUND_MODEL --provider $GROUND_PIN --seed $SEED}"
export REVIEW_CMD="${REVIEW_CMD:-$TAU2_PY $SCRIPTS/openrouter_toolcall.py $REVIEW_MODEL --provider $REVIEW_PIN --seed $SEED}"
# Assigned in two steps: `${VAR:-{...}}` ends at the first unescaped `}`.
DEFAULT_LOOP_POLICY='{"discover_objective":"learner"}'
export LOOP_POLICY="${LOOP_POLICY:-$DEFAULT_LOOP_POLICY}"
export TAU2_DATA_DIR="${TAU2_DATA_DIR:-$TAU2/data}"
export PY="$TAU2_PY"
export HERE="$REPO/crates/areev-bench/tau2"
