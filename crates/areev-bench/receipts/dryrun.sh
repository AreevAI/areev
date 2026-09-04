#!/bin/sh
# Keyless plumbing test: mock agent, mock loop legs, real Areev, real gates —
# the whole protocol curve.sh runs for money, run for nothing.
#
#   dryrun.sh [workdir] [receipts] [learn-every] [held-out]
#
# Pass criteria, stated before running:
#   1. experience — the first receipts score 0 exact (ISO dates); a lesson is
#      proposed and applied at the first learn point; every receipt after it
#      files the date exactly.
#   2. paired eval — B beats A on every discordant pair; B2 equals B.
#   3. verify → revert — every applied lesson gets a verdict; a harmful lesson
#      admitted on purpose is measured as regressed, reverted through the API,
#      and not re-proposed; R recovers past H.
# If any step does not hold, the harness is not wiring memory into the prompt
# and no paid run should follow. The rule reviewer is keyless too: a fixture
# judge that approves any rule naming the date format.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PY="${PY:-$REPO/.venv/bin/python3}"
WORKDIR="${1:-/tmp/receipts-dry}"
N="${2:-8}"
EVERY="${3:-2}"
HELD="${4:-6}"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

export AGENT_CMD="$PY $HERE/mock_agent.py"
export AREEV_MOCK_LLM_FIXTURE="$HERE/fixtures/lesson_dateformat.json"
export LOOP_LLM_CMD="$PY $REPO/examples/llm/mock.py"
export LOOP_GROUND_CMD="$PY $REPO/examples/llm/mock.py"
export REVIEW_CMD="$PY $HERE/mock_judge.py"
export LOOP_POLICY='{"discover_objective":"learner"}'
# A SYNTHETIC corpus, committed, so the gate runs with no download, no key
# and no licence question. It is not SROIE and it is not a measurement: what
# it proves is that the governance chain fires end to end.
DATASET="${DATASET:-$HERE/fixtures/tiny.jsonl}"

echo "######## 1. experience (A0 journaled first, every lesson measured)"
"$PY" "$HERE/run.py" --dataset "$DATASET" --workdir "$WORKDIR" \
  --seed 1 --experience "$N" --eval "$HELD" --learn-every "$EVERY" \
  --measure --journal-baseline

echo "######## 2. paired evaluation (B journaled)"
"$PY" "$HERE/evaluate.py" --dataset "$DATASET" --learned-db "$WORKDIR/ledger.db" \
  --workdir "$WORKDIR/eval" --seed 1 --experience "$N" --eval "$HELD" --journal B=eval-b
"$PY" "$HERE/stats.py" "$WORKDIR/eval/trials.json"

echo "######## 3. verify → forced regression → revert"
unset AREEV_MOCK_LLM_FIXTURE
"$PY" "$HERE/regress.py" --dataset "$DATASET" --learned-db "$WORKDIR/ledger.db" \
  --workdir "$WORKDIR/regress" --seed 1 --experience "$N" --eval "$HELD"
