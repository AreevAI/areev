#!/bin/sh
# Post-hoc prequential reads: at every checkpoint, both adapters and the LLM
# carrying that checkpoint's rules read the NEXT window of stream documents --
# the ones the deployment met next, which no adapter at that checkpoint has
# seen and which share its era. The stream is chronological (filing date), so
# the fixed all-era held-out sets measure how much of the eventual
# distribution a checkpoint covers; this measures each checkpoint against its
# own present. Runs after curve_tune.sh from what it persisted; touches
# nothing the main run produced. The governed agent's own record over the
# same window is already in journal.jsonl (curve_stats.py reads it).
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 curve_next.sh <seed-dir> [window 20]
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
OUT="$1"; N="${2:-20}"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
for ck in "$OUT"/ck_*; do
  [ -d "$ck" ] || continue
  k=$(basename "$ck" | sed 's/^ck_0*//')
  [ "$k" -lt "$EXP" ] || continue   # nothing comes after the last document
  db="$OUT/snap_$(printf '%03d' "$k").db"
  [ -f "$db" ] || { echo "no memory at checkpoint $k ($db) -- skipping"; continue; }
  for mode in scratch continual; do
    [ -s "$ck/adapter_$mode/adapters.safetensors" ] || continue
    [ -f "$ck/eval_${mode}_next/trials.json" ] && continue
    echo "######## seed $SEED: checkpoint $k — $mode adapter on the next $N documents"
    SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" EVAL_ROWS="$N" ARMS=B HOLDOUT=next UPTO="$k" \
      sh "$HERE/slm_eval.sh" "$db" "$ck/eval_${mode}_next" "ck$(printf '%03d' "$k")-$mode-next" "$ck/adapter_$mode"
  done
  if [ ! -f "$ck/eval_llm_next/trials.json" ]; then
    echo "######## seed $SEED: checkpoint $k — the LLM with this checkpoint's rules on the next $N documents"
    "$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$db" \
      --workdir "$ck/eval_llm_next" --seed "$SEED" --experience "$EXP" --eval "$EVAL" --arms B \
      --holdout next --upto-seq "$k" --rows "$N"
  fi
done
echo "######## seed $SEED: prequential reads done — $OUT"
