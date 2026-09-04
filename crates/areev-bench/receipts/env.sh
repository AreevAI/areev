#!/bin/sh
# Shared environment for the receipts harness. Sourced by the drivers.
#
# Every model leg is provider-pinned and seeded, for the same reason the
# selfimprove bench pins them: unpinned routing moved 5 of 60 held-out tasks
# between two byte-identical runs, and an effect you cannot reproduce is not
# an effect. Override any leg by exporting it before calling a driver.
#
#   REPO          the areev checkout (auto-detected from this file)
#   PY            a python with the `areev` binding installed
#                 (maturin develop --release -m crates/areev-py/Cargo.toml)
#   SEED          the corpus split seed AND the request seed (default 1)
#   AGENT_MODEL   the capture agent            (default qwen3-30b, coreweave/bf16)
#   LEARNER_MODEL DISCOVER/VERIFY              (default = AGENT_MODEL)
#   GROUND_MODEL  GROUND                       (default gpt-4o-mini, openai)
#   REVIEW_MODEL  the rule reviewer            (default gpt-4o, openai)
#   LOOP_POLICY   host policy JSON             (default: learner objective)
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-$(cd "$HERE/../../.." && pwd)}"
PY="${PY:-$REPO/.venv/bin/python3}"
SCRIPTS="$REPO/crates/areev-bench/scripts"
SEED="${SEED:-1}"

AGENT_MODEL="${AGENT_MODEL:-qwen/qwen3-30b-a3b-instruct-2507}"
AGENT_PIN="${AGENT_PIN:-coreweave/bf16}"
LEARNER_MODEL="${LEARNER_MODEL:-$AGENT_MODEL}"
LEARNER_PIN="${LEARNER_PIN:-$AGENT_PIN}"
GROUND_MODEL="${GROUND_MODEL:-openai/gpt-4o-mini}"
GROUND_PIN="${GROUND_PIN:-openai}"
REVIEW_MODEL="${REVIEW_MODEL:-openai/gpt-4o}"
REVIEW_PIN="${REVIEW_PIN:-openai}"

if [ -f "$HOME/mg/local/dev-areev.env" ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  set -a
  . "$HOME/mg/local/dev-areev.env"
  set +a
fi

export AGENT_CMD="${AGENT_CMD:-$PY $SCRIPTS/openrouter_toolcall.py $AGENT_MODEL --provider $AGENT_PIN --seed $SEED}"
export LOOP_LLM_CMD="${LOOP_LLM_CMD:-$PY $SCRIPTS/openrouter_loop.py $LEARNER_MODEL --provider $LEARNER_PIN --seed $SEED}"
export LOOP_GROUND_CMD="${LOOP_GROUND_CMD:-$PY $SCRIPTS/openrouter_loop.py $GROUND_MODEL --provider $GROUND_PIN --seed $SEED}"
export REVIEW_CMD="${REVIEW_CMD:-$PY $SCRIPTS/openrouter_toolcall.py $REVIEW_MODEL --provider $REVIEW_PIN --seed $SEED}"
# Assigned in two steps on purpose: `${VAR:-{...}}` ends the expansion at the
# first unescaped `}`, so a JSON default inside one silently gains a trailing
# brace. It cost a run before anyone read the value.
DEFAULT_LOOP_POLICY='{"discover_objective":"learner"}'
export LOOP_POLICY="${LOOP_POLICY:-$DEFAULT_LOOP_POLICY}"

# The corpus. One variable picks the ledger profile and the dataset that
# goes with it, so a driver never has to know which corpus it is running.
export PROFILE="${PROFILE:-sroie}"
# A profile may re-time a corpus it does not own (sroie_drift reads sroie), so
# ask the profile which dataset it reads rather than assuming the names match.
CORPUS="$("$PY" "$HERE/ledger_profile.py" corpus "$PROFILE")"
export DATASET="${DATASET:-$HERE/data/$CORPUS.jsonl}"
