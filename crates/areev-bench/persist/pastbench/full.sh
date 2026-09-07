#!/bin/sh
# The full run: every v2 family for a list of agents, each agent in its own
# tmux session with its own port offset so the three arms run side by side,
# RUN tagging the repetition (the A0R noise floor and the paired contrasts
# need three of each).
#
#   AGENTS="areev-governed areev-passive hermes" RUN=1 ROOT=$HOME/mg/local/areev-runs/persist/full sh full.sh
#   OFFSET_BASE=30000 starts the port offsets there (a second run beside the first);
#   streams are spaced 10,000 apart because families bind ports 3200–3300 and 9105–9210
#
# Each agent's pilot.sh loop lands in $ROOT/run$RUN/<agent>/<family>/ and
# skips families already done, so a stopped run resumes with the same
# command. The key-usage bound in spend.jsonl covers whatever else was on
# the key during the run and is labelled so in the report.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PB="${PASTBENCH_DIR:-$HOME/mg/local/PAST-Bench}"
AGENTS="${AGENTS:?set AGENTS}"; RUN="${RUN:-1}"; ROOT="${ROOT:?set ROOT}"
# only directories that ARE families (family.yaml), not _shared/fixtures etc.
FAMILIES=$(cd "$PB/self-evolve-tasks-v2" && ls */*/family.yaml | sed 's#/family.yaml$##' | tr '\n' ' ')
n=$(echo "$FAMILIES" | wc -w | tr -d ' ')
echo "$n families: $FAMILIES"
offset=${OFFSET_BASE:-0}
for agent in $AGENTS; do
  out="$ROOT/run$RUN"
  mkdir -p "$out"
  tmux kill-session -t "full-$RUN-$agent" 2>/dev/null || true
  tmux new -d -s "full-$RUN-$agent" \
    "FAMILIES=\"$FAMILIES\" AGENTS=\"$agent\" ROOT=\"$out\" PORT_OFFSET=$offset SEED=$RUN sh \"$HERE/pilot.sh\" > \"$out/$agent.log\" 2>&1"
  echo "started full-$RUN-$agent (port offset $offset) -> $out/$agent.log"
  offset=$((offset + 10000))
done
