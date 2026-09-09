# Every leg this track runs, in one place. Source it; do not run it.
#
#   sh -c '. ./env.sh; ...'
#
# The model legs themselves are pinned in the jsonnet configs (model, provider,
# temperature, seed) -- what lives here is only how to reach them.

# The AppWorld checkout: pinned at commit 42b5bcf (0.2.0.dev0, data 0.2.0).
export APPWORLD_ROOT="${APPWORLD_ROOT:-$HOME/mg/local/appworld}"
export APPWORLD_VENV="${APPWORLD_VENV:-$HOME/mg/local/appworld-venv}"
export PY="$APPWORLD_VENV/bin/python"
export APPWORLD="$APPWORLD_VENV/bin/appworld"

# The agent leg goes to OpenRouter. GPT legs (GROUND, the rubric reviewer)
# go to OpenAI DIRECTLY -- same models cost several times more billed through
# OpenRouter, and keeping them off that wallet leaves it for the agent.
[ -f "$HOME/mg/local/dev-areev.env" ] && . "$HOME/mg/local/dev-areev.env"
# OPENAI_API_KEY comes from the office bench env. dev-openai.env held a key
# api.openai.com answered 401 to, and was deleted on 2026-09-08.
[ -f "$HOME/mg/local/dev-office-areev-bench.env" ] && . "$HOME/mg/local/dev-office-areev-bench.env"
export OPENROUTER_API_KEY OPENAI_API_KEY

# The legs, named once. The reviewer is a GPT model and so is called at
# api.openai.com directly -- the same model billed through OpenRouter costs
# several times more. The learner and GROUND legs are open-weights models
# OpenRouter is the route to, so they stay there and stay provider-pinned.
# $0 is the SOURCING script, not this file, so ../scripts resolved only when
# env.sh was sourced from run.sh. Anchored on the repo instead.
AREEV_BENCH_SCRIPTS="${AREEV_BENCH_SCRIPTS:-$(cd "$(git -C "${BASH_SOURCE%/*}" rev-parse --show-toplevel 2>/dev/null || echo "$HOME/mg/products/areev-appworld")/crates/areev-bench/scripts" && pwd)}"
export AREEV_BENCH_SCRIPTS
export AREEV_REVIEW_CMD="${AREEV_REVIEW_CMD:-$PY $AREEV_BENCH_SCRIPTS/openrouter_toolcall.py gpt-4o --base-url https://api.openai.com/v1 --key-env OPENAI_API_KEY}"
export AREEV_LOOP_LLM_CMD="${AREEV_LOOP_LLM_CMD:-$PY $AREEV_BENCH_SCRIPTS/openrouter_loop.py openai/gpt-oss-120b --provider deepinfra --seed 100}"
export AREEV_LOOP_GROUND_CMD="${AREEV_LOOP_GROUND_CMD:-$PY $AREEV_BENCH_SCRIPTS/openrouter_loop.py openai/gpt-oss-120b --provider deepinfra --seed 100}"

# Two quirks of the harness, not choices of ours:
#  - any config carrying a base_url is templated through $MODEL_SERVER_URL,
#    so the variable has to exist even when the URL has no placeholder;
#  - LanguageModel builds an OpenAI() client just to read a signature, so
#    OPENAI_API_KEY must be set even for a run that never calls OpenAI.
export MODEL_SERVER_URL="https://openrouter.ai/api/v1"
