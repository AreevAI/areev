#!/bin/sh
# The pilot: a list of families × a list of agents, run one after another
# (the task services bind fixed ports, so runs do not overlap), with the
# OpenRouter key's usage counter read before and after every run so the
# judge — which the benchmark does not meter — is bounded per run.
#
#   FAMILIES="memory_ability/SM01_preference_adoption procedural_ability/PC01_sop_bootstrap_01" \
#   AGENTS="areev-governed areev-passive hermes" ROOT=$HOME/mg/local/areev-runs/persist/pilot sh pilot.sh
#
# Each run lands in $ROOT/<agent>/<family_id>/ with the benchmark's traces
# and sequence_comparison.json; $ROOT/spend.jsonl has one line per run:
# key usage before/after (USD, account-wide — shared with any other job on
# the key, so an upper bound), wall seconds, exit code.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT="${ROOT:?set ROOT}"; FAMILIES="${FAMILIES:?set FAMILIES}"; AGENTS="${AGENTS:?set AGENTS}"
mkdir -p "$ROOT"
if [ -z "${OPENROUTER_API_KEY:-}" ] && [ -f "$HOME/mg/local/dev-areev.env" ]; then
  set -a; . "$HOME/mg/local/dev-areev.env"; set +a
fi
usage() {
  curl -s --max-time 20 https://openrouter.ai/api/v1/key -H "Authorization: Bearer $OPENROUTER_API_KEY" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["usage"])' 2>/dev/null || echo null
}
for agent in $AGENTS; do
  for fam in $FAMILIES; do
    fid=$(basename "$fam")
    out="$ROOT/$agent/$fid"
    if [ -f "$out/sequence_comparison.json" ]; then
      echo "skip $agent $fid (done)"; continue
    fi
    rm -rf "$out"; mkdir -p "$out"
    before=$(usage); t0=$(date +%s)
    echo "### $agent $fid  (key usage before: $before)"
    set +e
    AGENT="$agent" FAMILY="$fam" OUT="$out" sh "$HERE/evolve.sh" > "$out/run.log" 2>&1
    rc=$?
    set -e
    after=$(usage); t1=$(date +%s)
    printf '{"agent":"%s","family":"%s","usage_before":%s,"usage_after":%s,"wall_s":%d,"exit":%d,"at":"%s"}\n' \
      "$agent" "$fam" "$before" "$after" "$((t1-t0))" "$rc" "$(date -u +%FT%TZ)" >> "$ROOT/spend.jsonl"
    echo "    exit=$rc wall=$((t1-t0))s usage after: $after"
  done
done
echo PILOT_DONE
