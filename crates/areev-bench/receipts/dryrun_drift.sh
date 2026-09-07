#!/bin/sh
# Keyless plumbing test for the NON-STATIONARY ledger — everything drift.sh
# runs for money, run for nothing.
#
#   dryrun_drift.sh [workdir] [documents] [learn-every] [held-out] [every]
#
# Pass criteria, stated before running:
#   1. the ledger's filed values change at the flip, and the accountant
#      announces it exactly once;
#   2. arm C's prompt section differs from arm B's — the ungoverned baseline
#      is rendering the raw record, not the governed rules;
#   3. checkpoints score against the convention in force at that checkpoint,
#      not the final one;
#   4. every applied lesson still gets a verdict after the flip.
#
# What it does NOT claim: that the loop retracts the stale rule. mock_agent.py
# writes ISO only when NO date rule is in the prompt, which is why the tiny
# profile flips to ISO — the mock can only score after the flip if the rule
# really went away. Whether it does is the paid run's question.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PY="${PY:-$REPO/.venv/bin/python3}"
WORKDIR="${1:-/tmp/drift-dry}"
N="${2:-12}"
EVERY="${3:-2}"
HELD="${4:-6}"
SNAP="${5:-4}"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

export PROFILE=sroie_drift_tiny
export AGENT_CMD="$PY $HERE/mock_agent.py"
export AREEV_MOCK_LLM_FIXTURE="$HERE/fixtures/lesson_dateformat.json"
export LOOP_LLM_CMD="$PY $REPO/examples/llm/mock.py"
export LOOP_GROUND_CMD="$PY $REPO/examples/llm/mock.py"
export REVIEW_CMD="$PY $HERE/mock_judge.py"
export LOOP_POLICY='{"discover_objective":"learner"}'
DATASET="${DATASET:-$HERE/fixtures/tiny.jsonl}"

echo "######## 1. experience across the flip ($N documents, profile $PROFILE)"
"$PY" "$HERE/run.py" --profile "$PROFILE" --dataset "$DATASET" --workdir "$WORKDIR" \
  --seed 1 --experience "$N" --eval "$HELD" --learn-every "$EVERY" \
  --snapshot-every "$SNAP" --measure --journal-baseline

n="$SNAP"
while [ "$n" -le "$N" ]; do
  tag=$(printf '%03d' "$n")
  if [ "$n" -ge "$N" ]; then
    db="$WORKDIR/ledger.db"; arms="B,C,A"; extra="--journal B=eval-b"
  else
    db="$WORKDIR/snap_$tag.db"; arms="B,C,A"
    extra="--journal B=eval-at-$tag --journal-into $WORKDIR/ledger.db"
  fi
  if [ -f "$db" ]; then
    echo "######## 2. checkpoint $n — arms $arms, scored as of document $n"
    # shellcheck disable=SC2086
    "$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" \
      --learned-db "$db" --workdir "$WORKDIR/at_$tag" --seed 1 \
      --experience "$N" --eval "$HELD" --arms "$arms" --as-of "$n" $extra
  fi
  n=$((n + SNAP))
done

echo "######## 3. verify — do the run's own rules still get verdicts after the flip?"
unset AREEV_MOCK_LLM_FIXTURE
"$PY" "$HERE/regress.py" --profile "$PROFILE" --dataset "$DATASET" \
  --learned-db "$WORKDIR/ledger.db" --workdir "$WORKDIR/verify" \
  --seed 1 --experience "$N" --eval "$HELD" --no-plant || true
echo "######## done — $WORKDIR"
