#!/bin/sh
# The tuning learning curve: does a small model tuned on a governed memory
# keep improving as the deployment grows, or level off — and does it matter
# whether each checkpoint starts from the plain base or from the last one?
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 CKPTS=20,40,80,160 curve_tune.sh <runs-root>
#
# One seed, end to end:
#   1. the governed loop runs EXP documents in filing-date order, snapshotting
#      the memory at each checkpoint (rules as they stood, corrections so far);
#   2. at every checkpoint the memory becomes a corpus and TWO adapters:
#        scratch     the plain base on everything up to here
#        continual   the previous checkpoint's continual adapter, on only the
#                    documents since it
#   3. each adapter reads two held-out sets of EVAL documents — UNSEEN
#      (registrants the agent never saw) and SEEN (registrants it learned
#      from) — and so does the LLM carrying that checkpoint's rules;
#   4. once, the untuned base under the final rules, the control.
#
# Every held-out read is metered; the local model's latency is in the ledger.
# Overfitting is handled in the trainer (validation-selected checkpoints,
# epochs scaled to corpus size) and in the split (unseen is by organisation,
# not by document) — see slm_train.sh and dataset.split_entity.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
ROOT="$1"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"; CKPTS="${CKPTS:-20,40,80,160}"
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
OUT="$ROOT/seed$SEED"
rm -rf "$OUT"; mkdir -p "$OUT"
echo "######## seed $SEED: governed experience, $EXP documents, snapshots at $CKPTS + $EXP"
sh "$HERE/learn.sh" "$OUT" --experience "$EXP" --eval "$EVAL" --learn-every 2 \
  --snapshot-at "$CKPTS" --measure --journal-baseline

prev=0; prev_cont=""
for k in $(echo "$CKPTS" | tr ',' ' ') "$EXP"; do
  tag=$(printf '%03d' "$k")
  ck="$OUT/ck_$tag"; mkdir -p "$ck"
  if [ "$k" -ge "$EXP" ]; then db="$OUT/ledger.db"; else db="$OUT/snap_$tag.db"; fi
  [ -f "$db" ] || { echo "no memory at checkpoint $k ($db) — skipping"; continue; }
  echo "######## seed $SEED: checkpoint $k — corpora"
  "$PY" "$HERE/slm_corpus.py" --profile "$PROFILE" --learned-db "$db" --dataset "$DATASET" --seed "$SEED" \
    --experience "$EXP" --eval "$EVAL" --upto-seq "$k" --out "$ck/corpus_all"
  "$PY" "$HERE/slm_corpus.py" --profile "$PROFILE" --learned-db "$db" --dataset "$DATASET" --seed "$SEED" \
    --experience "$EXP" --eval "$EVAL" --since-seq "$prev" --upto-seq "$k" --out "$ck/corpus_delta"

  echo "######## seed $SEED: checkpoint $k — tune from scratch"
  SLM_BASE="$BASE" sh "$HERE/slm_train.sh" "$ck/corpus_all" "$ck/adapter_scratch"
  if [ -z "$prev_cont" ]; then
    echo "######## seed $SEED: checkpoint $k — continual = scratch at the first checkpoint"
    cp -R "$ck/adapter_scratch" "$ck/adapter_continual"
  else
    echo "######## seed $SEED: checkpoint $k — tune continually from $prev_cont"
    SLM_BASE="$BASE" sh "$HERE/slm_train.sh" "$ck/corpus_delta" "$ck/adapter_continual" --resume "$prev_cont"
  fi

  for mode in scratch continual; do for h in unseen seen; do
    echo "######## seed $SEED: checkpoint $k — $mode adapter on $h"
    # At the final checkpoint the scratch adapter reads the unseen set twice:
    # the local model's noise floor. The trial found it is not zero -- one
    # adapter, two reads, 9 of 79 trials apart -- so it is measured, not assumed.
    arms=B; [ "$k" -ge "$EXP" ] && [ "$mode" = scratch ] && [ "$h" = unseen ] && arms=B,B2
    SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" ARMS="$arms" HOLDOUT="$h" \
      sh "$HERE/slm_eval.sh" "$db" "$ck/eval_${mode}_$h" "ck$tag-$mode" "$ck/adapter_$mode" 8081
  done; done
  for h in unseen seen; do
    echo "######## seed $SEED: checkpoint $k — the LLM with this checkpoint's rules, $h"
    "$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$db" \
      --workdir "$ck/eval_llm_$h" --seed "$SEED" --experience "$EXP" --eval "$EVAL" --arms B --holdout "$h"
  done
  prev="$k"; prev_cont="$ck/adapter_continual"
done

for h in unseen seen; do
  echo "######## seed $SEED: the untuned base under the final rules, $h"
  SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" ARMS=B HOLDOUT="$h" \
    sh "$HERE/slm_eval.sh" "$OUT/ledger.db" "$OUT/eval_base_$h" "base" none 8081
done
echo "######## seed $SEED: done — $OUT"
