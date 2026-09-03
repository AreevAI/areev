#!/bin/sh
# One seed, end to end: experience with snapshots, then the paired held-out
# evaluation at the final memory and a learning curve with the TASK HELD
# CONSTANT at each snapshot.
#
#   SEED=1 curve.sh <runs-root> [experience N] [eval N] [snapshot-every N]
#
# The running score of the experience phase is NOT a learning curve: the
# accountant adds required fields as it proceeds, so the denominator grows
# and the running average falls even while the agent improves. It measures
# the goalpost moving. This measures the same held-out receipts against
# memory as it stood after 0, 10, 20, ... experience receipts.
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
echo "######## seed $SEED: experience phase ($EXP receipts, snapshot every $SNAP)"
sh "$HERE/learn.sh" "$OUT" --experience "$EXP" --eval "$EVAL" --learn-every 2 --snapshot-every "$SNAP"

echo "######## seed $SEED: paired evaluation at the final memory ($EVAL held-out)"
sh "$HERE/eval.sh" "$OUT/ledger.db" "$OUT/eval" --experience "$EXP" --eval "$EVAL"

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
