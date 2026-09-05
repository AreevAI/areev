#!/bin/sh
# One Horizon run for one agent: the public dataset (default) or one task,
# every model leg pinned and seeded, spend metered.
#
#   AGENT=areev-governed OUT=$HOME/runs/x sh run.sh                    # all public tasks
#   AGENT=trace_rag TASK=evals/01-example-catering-vendor OUT=... sh run.sh
#
#   AGENT      areev-governed | areev-passive | trace_rag | tools_only | perfect_context | hermes
#   TASK       a task dir under the Horizon checkout (default: the whole public dataset)
#   OUT        Harbor job output directory
#   SEED       request seed for every leg (default 1)
#   AGENT_MODEL / AGENT_PIN / JUDGE not used here: Horizon's reward is deterministic
#   HORIZON_DIR the checkout (default ~/mg/local/horizon)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
HZ="${HORIZON_DIR:-$HOME/mg/local/horizon}"
AGENT="${AGENT:?set AGENT}"; OUT="${OUT:?set OUT}"
SEED="${SEED:-1}"
MODEL="${AGENT_MODEL:-qwen/qwen3-30b-a3b-instruct-2507}"
SCRIPTS="$REPO/crates/areev-bench/scripts"
# streamlake, not siliconflow/fp8 — see pastbench/evolve.sh for why
export AREEV_AGENT_PIN="${AGENT_PIN:-streamlake}"
export AREEV_LOOP_LLM_CMD="${AREEV_LOOP_LLM_CMD:-python3 $SCRIPTS/openrouter_loop.py $MODEL --provider $AREEV_AGENT_PIN --seed $SEED}"
export AREEV_LOOP_GROUND_CMD="${AREEV_LOOP_GROUND_CMD:-python3 $SCRIPTS/openrouter_loop.py openai/gpt-4o-mini --provider openai --seed $SEED}"
export AREEV_REVIEW_CMD="${AREEV_REVIEW_CMD:-python3 $SCRIPTS/openrouter_toolcall.py openai/gpt-4o --provider openai --seed $SEED}"
export SEED PYTHONUNBUFFERED=1
for f in "$HOME/mg/local/dev-areev.env" "$HOME/mg/local/dev-office-areev-bench.env"; do
  [ -f "$f" ] && { set -a; . "$f"; set +a; }
done
# the reference agents mint a per-trial sub-key from a management key; the
# office env file names it differently
export OPENROUTER_MANAGEMENT_KEY="${OPENROUTER_MANAGEMENT_KEY:-${OPENROUTER_MANAGEMENT_API_KEY:-}}"
export PATH="$HOME/.local/bin:$PATH"
mkdir -p "$OUT"
case "$AGENT" in
  areev-governed) IMPORT="areev_agent.agent:AreevGovernedAgent" ;;
  areev-passive)  IMPORT="areev_agent.agent:AreevPassiveAgent" ;;
  trace_rag)      IMPORT="trace_rag.agent:TraceRagAgent" ;;
  trace_rlm)      IMPORT="trace_rlm.agent:TraceRlmAgent" ;;
  tools_only)     IMPORT="tools_only.agent:ToolsOnlyAgent" ;;
  perfect_context) IMPORT="perfect_context.agent:PerfectContextAgent" ;;
  hermes)         IMPORT="hermes.agent:HermesAgent" ;;
  *) echo "unknown AGENT $AGENT" >&2; exit 2 ;;
esac
cd "$HZ"
# The public set is run from the LOCAL evals/ copies, not `-d orinlabs/horizon-public`:
# the hub copies carry the unpatched judges whose reward.json Harbor 0.22
# cannot parse (the reward is still written; only Harbor's summary loses it).
# One `harbor run` per task — `-p` is single-valued — each into its own
# subdirectory of OUT.
if [ -n "${TASK:-}" ]; then
  # shellcheck disable=SC2086
  exec env PYTHONPATH=agents harbor run -p "$TASK" --agent-import-path "$IMPORT" -m "$MODEL" \
    --ae OPENROUTER_API_KEY="$OPENROUTER_API_KEY" -o "$OUT" "$@"
fi
rc=0
for t in evals/*/; do
  t=${t%/}
  name=$(basename "$t")
  echo "### $AGENT $name"
  # shellcheck disable=SC2086
  env PYTHONPATH=agents harbor run -p "$t" --agent-import-path "$IMPORT" -m "$MODEL" \
    --ae OPENROUTER_API_KEY="$OPENROUTER_API_KEY" -o "$OUT/$name" "$@" || rc=$?
done
exit $rc
