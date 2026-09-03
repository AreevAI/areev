#!/bin/sh
# Keyless plumbing test: mock agent, mock loop legs, real Areev, real gates.
#
#   dryrun.sh [workdir] [receipts] [learn-every]
#
# Pass criterion, stated before running: the first receipts score 0 exact
# (ISO dates), a lesson is proposed and applied at the first learn point, and
# every receipt after it scores exact on the date. If the score does not
# step, the harness is not wiring memory into the prompt and no paid run
# should follow. The rule reviewer is the one leg that stays keyless too: a
# fixture judge that approves any rule naming the date format.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PY="${PY:-$REPO/.venv/bin/python3}"
WORKDIR="${1:-/tmp/receipts-dry}"
N="${2:-8}"
EVERY="${3:-2}"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

export AGENT_CMD="$PY $HERE/mock_agent.py"
export AREEV_MOCK_LLM_FIXTURE="$HERE/fixtures/lesson_dateformat.json"
export LOOP_LLM_CMD="$PY $REPO/examples/llm/mock.py"
export LOOP_GROUND_CMD="$PY $REPO/examples/llm/mock.py"
export REVIEW_CMD="$PY $HERE/mock_judge.py"
export LOOP_POLICY='{"discover_objective":"learner"}'
DATASET="${DATASET:-$HERE/data/sroie.jsonl}"

exec "$PY" "$HERE/run.py" --dataset "$DATASET" --workdir "$WORKDIR" \
  --seed 1 --experience "$N" --eval 0 --learn-every "$EVERY"
