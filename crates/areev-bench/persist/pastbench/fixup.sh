#!/bin/sh
# After a full run: every family an arm is still missing (no
# sequence_comparison.json) is rerun, arms one after another, at port
# offset 0 — the benchmark's `x_mock` service (PC01_sop_bootstrap_06)
# ignores the offset, so that family can only run alone on the base ports.
#
#   AGENTS="areev-governed areev-passive hermes mem0" ROOT=$HOME/mg/local/areev-runs/persist/full/run1 SEED=1 sh fixup.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PB="${PASTBENCH_DIR:-$HOME/mg/local/PAST-Bench}"
AGENTS="${AGENTS:?}"; ROOT="${ROOT:?}"
ALL=$(cd "$PB/self-evolve-tasks-v2" && ls */*/family.yaml | sed 's#/family.yaml$##')
for agent in $AGENTS; do
  missing=""
  for fam in $ALL; do
    [ -f "$ROOT/$agent/$(basename "$fam")/sequence_comparison.json" ] || missing="$missing $fam"
  done
  if [ -z "$missing" ]; then echo "$agent: complete"; continue; fi
  echo "$agent: rerunning$missing"
  FAMILIES="$missing" AGENTS="$agent" ROOT="$ROOT" PORT_OFFSET=0 SEED="${SEED:-1}" sh "$HERE/pilot.sh" > "$ROOT/$agent.fixup.log" 2>&1 || true
  grep -E "exit=" "$ROOT/$agent.fixup.log" | cut -c5-80
done
echo FIXUP_DONE
