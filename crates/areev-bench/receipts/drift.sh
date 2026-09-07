#!/bin/sh
# The non-stationary ledger: does a governed loop track a business that
# changes its mind, and does it beat simply remembering everything?
#
#   PROFILE=sroie_drift SEED=1 drift.sh <runs-root> [experience 160] [eval 60] [every 40]
#
# The timeline is the profile's (`sroie_drift`): requirements arrive at
# documents 2 and 41, and at 81 the ledger switches from DD/MM/YYYY to ISO —
# announced once, never repeated. Every date rule learned before 81 is wrong
# after it, so improving means RETRACTING, which is the thing a store-and-
# recall memory cannot do.
#
# At each checkpoint the same held-out set is read three ways, scored against
# the convention in force at that point:
#
#   B  the governed loop — proposed, reviewed, applied, measured, reverted
#   C  the same memory rendered UNGOVERNED — every correction verbatim,
#      nothing retracted. The store-everything baseline, deliberately
#      generous: it gets the whole record without paying for governance.
#   A  every rule rolled back through the API — the frozen day-one floor
#
# Finally a verify-only pass (regress.py --no-plant): nothing is planted,
# because the run's own rules went stale when the business changed, and
# whether the gate says so is the experiment.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
ROOT="$1"
EXP="${2:-160}"
EVAL="${3:-60}"
SNAP="${4:-40}"
OUT="$ROOT/seed$SEED"

rm -rf "$OUT"
mkdir -p "$OUT"
echo "######## seed $SEED: A0 + experience ($EXP documents, profile $PROFILE)"
sh "$HERE/learn.sh" "$OUT" --experience "$EXP" --eval "$EVAL" --learn-every 2 \
  --snapshot-every "$SNAP" --measure --journal-baseline

n="$SNAP"
while [ "$n" -le "$EXP" ]; do
  tag=$(printf '%03d' "$n")
  # Every checkpoint journals its governed arm INTO the primary memory, so
  # the run leaves a time series of eval runs rather than one reading at the
  # end. That series is what any judgement about staleness has to be made
  # from: a rule can be good for eighty documents and wrong for the next
  # eighty, and a single before/after pair cannot see that.
  if [ "$n" -ge "$EXP" ]; then
    db="$OUT/ledger.db"; arms="B,B2,C,A"; extra="--journal B=eval-b"
  else
    db="$OUT/snap_$tag.db"; arms="B,C,A"; extra="--journal B=eval-at-$tag --journal-into $OUT/ledger.db"
  fi
  if [ -f "$db" ]; then
    echo "######## seed $SEED: checkpoint $n — arms $arms, scored as of document $n"
    # shellcheck disable=SC2086
    sh "$HERE/eval.sh" "$db" "$OUT/at_$tag" --experience "$EXP" --eval "$EVAL" \
      --arms "$arms" --as-of "$n" $extra
  fi
  n=$((n + SNAP))
done

echo "######## seed $SEED: verify — did the gate notice its own rules go stale?"
sh "$HERE/regress.sh" "$OUT/ledger.db" "$OUT/verify" --experience "$EXP" --eval "$EVAL" \
  --no-plant || true
echo "######## seed $SEED: done — $OUT"
