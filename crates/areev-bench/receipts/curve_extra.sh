#!/bin/sh
# Post-hoc reads for the overfitting record: every checkpoint adapter reads a
# sample of the documents it was TRAINED on. Train-set accuracy against
# unseen-set accuracy is the memorisation gap; for the continual path, the
# rows keep their seq, so accuracy on rows from earlier checkpoints against
# the latest delta is forgetting, measured directly. Runs after curve_tune.sh
# from the artifacts it persisted; touches nothing the main run produced.
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 curve_extra.sh <seed-dir> [rows 40]
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
OUT="$1"; ROWS="${2:-40}"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
for ck in "$OUT"/ck_*; do
  [ -d "$ck" ] || continue
  k=$(basename "$ck" | sed 's/^ck_0*//')
  if [ "$k" -ge "$EXP" ]; then db="$OUT/ledger.db"; else db="$OUT/snap_$(printf '%03d' "$k").db"; fi
  n="$ROWS"; [ "$k" -lt "$n" ] && n="$k"
  for mode in scratch continual; do
    [ -s "$ck/adapter_$mode/adapters.safetensors" ] || continue
    [ -f "$ck/eval_${mode}_train/trials.json" ] && continue
    echo "######## seed $SEED: checkpoint $k — $mode adapter on its own training rows ($n)"
    # EVAL stays the run's: in split_entity it decides which entities are held
    # out and so which documents form the stream. The sample size rides on
    # EVAL_ROWS, or the "training rows" read would sample another deployment.
    SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" EVAL_ROWS="$n" ARMS=B HOLDOUT=train UPTO="$k" \
      sh "$HERE/slm_eval.sh" "$db" "$ck/eval_${mode}_train" "ck$(printf '%03d' "$k")-$mode-train" "$ck/adapter_$mode"
  done
done
echo "######## seed $SEED: overfitting reads done — $OUT"
