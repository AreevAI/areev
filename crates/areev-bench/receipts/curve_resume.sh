#!/bin/sh
# Resume the checkpoint loop of curve_tune.sh for a seed whose governed phase
# already ran: skips adapters that have weights and evaluations that have
# trials, so an aborted run continues from where it stopped instead of
# re-spending the governed phase.
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 CKPTS=20,40,80,160 curve_resume.sh <seed-dir>
#
# Never edit a driver or slm_eval.sh while a run is live: sh reads scripts
# incrementally, and a write under a running invocation resumes at a stale
# offset -- which is how seed 1 died with "--upto-seq: command not found".
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
OUT="$1"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"; CKPTS="${CKPTS:-20,40,80,160}"
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
[ -f "$OUT/ledger.db" ] || { echo "no governed memory at $OUT/ledger.db" >&2; exit 1; }
prev=0; prev_cont=""
for k in $(echo "$CKPTS" | tr ',' ' ') "$EXP"; do
  tag=$(printf '%03d' "$k"); ck="$OUT/ck_$tag"; mkdir -p "$ck"
  if [ "$k" -ge "$EXP" ]; then db="$OUT/ledger.db"; else db="$OUT/snap_$tag.db"; fi
  [ -f "$db" ] || { echo "no memory at checkpoint $k — skipping"; continue; }
  [ -f "$ck/corpus_all/train.jsonl" ] || "$PY" "$HERE/slm_corpus.py" --profile "$PROFILE" --learned-db "$db" --dataset "$DATASET" --seed "$SEED" --experience "$EXP" --eval "$EVAL" --upto-seq "$k" --out "$ck/corpus_all"
  [ -f "$ck/corpus_delta/train.jsonl" ] || "$PY" "$HERE/slm_corpus.py" --profile "$PROFILE" --learned-db "$db" --dataset "$DATASET" --seed "$SEED" --experience "$EXP" --eval "$EVAL" --since-seq "$prev" --upto-seq "$k" --out "$ck/corpus_delta"
  if [ ! -s "$ck/adapter_scratch/adapters.safetensors" ]; then
    echo "######## seed $SEED: checkpoint $k — tune from scratch"
    rm -rf "$ck/adapter_scratch"; SLM_BASE="$BASE" sh "$HERE/slm_train.sh" "$ck/corpus_all" "$ck/adapter_scratch"
  fi
  if [ ! -s "$ck/adapter_continual/adapters.safetensors" ]; then
    rm -rf "$ck/adapter_continual"
    if [ -z "$prev_cont" ]; then echo "######## seed $SEED: checkpoint $k — continual = scratch at the first checkpoint"; cp -R "$ck/adapter_scratch" "$ck/adapter_continual"
    else echo "######## seed $SEED: checkpoint $k — tune continually from $prev_cont"; SLM_BASE="$BASE" sh "$HERE/slm_train.sh" "$ck/corpus_delta" "$ck/adapter_continual" --resume "$prev_cont"; fi
  fi
  for mode in scratch continual; do for h in unseen seen; do
    [ -f "$ck/eval_${mode}_$h/trials.json" ] && continue
    echo "######## seed $SEED: checkpoint $k — $mode adapter on $h"
    arms=B; [ "$k" -ge "$EXP" ] && [ "$mode" = scratch ] && [ "$h" = unseen ] && arms=B,B2
    rm -rf "$ck/eval_${mode}_$h"
    SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" ARMS="$arms" HOLDOUT="$h" sh "$HERE/slm_eval.sh" "$db" "$ck/eval_${mode}_$h" "ck$tag-$mode" "$ck/adapter_$mode"
  done; done
  for h in unseen seen; do
    [ -f "$ck/eval_llm_$h/trials.json" ] && continue
    echo "######## seed $SEED: checkpoint $k — the LLM with this checkpoint's rules, $h"
    rm -rf "$ck/eval_llm_$h"
    "$PY" "$HERE/evaluate.py" --profile "$PROFILE" --dataset "$DATASET" --learned-db "$db" --workdir "$ck/eval_llm_$h" --seed "$SEED" --experience "$EXP" --eval "$EVAL" --arms B --holdout "$h"
  done
  prev="$k"; prev_cont="$ck/adapter_continual"
done
for h in unseen seen; do
  [ -f "$OUT/eval_base_$h/trials.json" ] && continue
  echo "######## seed $SEED: the untuned base under the final rules, $h"
  rm -rf "$OUT/eval_base_$h"
  SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" ARMS=B HOLDOUT="$h" sh "$HERE/slm_eval.sh" "$OUT/ledger.db" "$OUT/eval_base_$h" "base" none
done
echo "######## seed $SEED: resumed to completion — $OUT"
