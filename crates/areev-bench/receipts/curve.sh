#!/bin/sh
# One seed, end to end — the protocol RECEIPTS.md pre-registers:
#
#   SEED=1 curve.sh <runs-root> [experience N] [eval N] [snapshot-every N]
#
#   1. A0: the day-one agent over the held-out set, journaled as the evalset
#      baseline before a single lesson exists.
#   2. Experience: N receipts, a learn pass every 2 corrections, every applied
#      lesson carrying the held-out set as its outcome metric, a memory
#      snapshot every 10 receipts.
#   3. Paired evaluation at the final memory: B, B2, A (A by real rollback);
#      B journaled, so the loop can measure the lessons against it.
#   4. Verify → forced regression → revert (regress.py): every lesson gets a
#      verdict; a harmful lesson is admitted on purpose, measured, reverted,
#      and not re-proposed.
#   5. The learning curve with the TASK held constant: the same held-out set
#      at each snapshot (arm B only).
#
# The running score of the experience phase is NOT a learning curve: the
# accountant adds required fields as it proceeds, so the denominator grows
# and the running average falls even while the agent improves.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
ROOT="$1"
EXP="${2:-40}"
EVAL="${3:-60}"
SNAP="${4:-10}"
OUT="$ROOT/seed$SEED"

rm -rf "$OUT"
mkdir -p "$OUT"
echo "######## seed $SEED: A0 + experience ($EXP receipts, snapshot every $SNAP)"
sh "$HERE/learn.sh" "$OUT" --experience "$EXP" --eval "$EVAL" --learn-every 2 \
  --snapshot-every "$SNAP" --measure --journal-baseline

echo "######## seed $SEED: paired evaluation at the final memory ($EVAL held-out)"
sh "$HERE/eval.sh" "$OUT/ledger.db" "$OUT/eval" --experience "$EXP" --eval "$EVAL" --journal B=eval-b
"$PY" "$HERE/stats.py" "$OUT/eval/trials.json"

echo "######## seed $SEED: verify → forced regression → revert"
sh "$HERE/regress.sh" "$OUT/ledger.db" "$OUT/regress" --experience "$EXP" --eval "$EVAL"

n="$SNAP"
while [ "$n" -lt "$EXP" ]; do
  tag=$(printf '%03d' "$n")
  if [ -f "$OUT/snap_$tag.db" ]; then
    echo "######## seed $SEED: held-out at $n experience receipts (arm B only)"
    sh "$HERE/eval.sh" "$OUT/snap_$tag.db" "$OUT/at_$tag" --experience "$EXP" --eval "$EVAL" --arms B
  fi
  n=$((n + SNAP))
done
echo "######## seed $SEED: done — $OUT"
