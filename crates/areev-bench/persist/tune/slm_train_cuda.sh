#!/bin/sh
# The CUDA twin of receipts/slm_train.sh: LoRA on a small Qwen with
# transformers + peft, from the corpus slm_corpus.py built out of a governed
# memory. Same command line, same controls, same manifest fields.
#
#   slm_train_cuda.sh <corpus-dir> <adapter-out> [--resume ADAPTER] [--base REPO] [--epochs N]
#
# Iterations scale with the corpus (a fixed 200 steps on 20 rows is twenty
# epochs), clamped to [40, 400]; validation every 25 steps; the lowest-val
# checkpoint is kept. Retries once at batch 1 with gradient checkpointing if
# the first attempt does not fit the 8 GB card.
#
# Areev never trains and ships no trainer; this is what a host plugs into
# `areev tune --cmd`.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
CORPUS="$1"; OUT="$2"; shift 2
BASE="${SLM_BASE:-Qwen/Qwen3-1.7B}"; RESUME=""; EPOCHS=4
PY="${PY:-$HOME/mg/local/persist-venv/bin/python}"
while [ $# -gt 0 ]; do
  case "$1" in
    --resume) RESUME="$2"; shift 2 ;;
    --base) BASE="$2"; shift 2 ;;
    --epochs) EPOCHS="$2"; shift 2 ;;
    *) echo "unknown flag $1" >&2; exit 2 ;;
  esac
done
ROWS=$(wc -l < "$CORPUS/train.jsonl" | tr -d ' ')
BATCH=2
ITERS=$(( (ROWS + BATCH - 1) / BATCH * EPOCHS ))
[ "$ITERS" -lt 40 ] && ITERS=40
[ "$ITERS" -gt 400 ] && ITERS=400
mkdir -p "$OUT"
train() {
  # shellcheck disable=SC2086
  "$PY" "$HERE/train_lora.py" --corpus "$CORPUS" --out "$OUT" --base "$BASE" --iters "$ITERS" \
    --batch "$1" --max-seq "$2" --steps-per-eval 25 --val-batches 4 --layers 8 --rank 8 --lr 1e-4 \
    --seed "${SEED:-1}" $3 ${RESUME:+--resume "$RESUME"} > "$OUT/train.stdout" 2>&1
}
if ! train "$BATCH" 3072 ""; then
  echo "first attempt failed (see $OUT/train.stdout); retrying with batch 1, shorter sequences, gradient checkpointing" >&2
  grep -iE "error|exception|out of memory" "$OUT/train.stdout" | tail -3 >&2
  train 1 2048 "--grad-checkpoint" || { echo "training failed twice; no adapter produced" >&2; exit 1; }
fi
[ -s "$OUT/adapter_model.safetensors" ] || { echo "training exited 0 but wrote no adapter_model.safetensors" >&2; exit 1; }
grep -E "^Iter|Trainable|Starting|Done" "$OUT/train.log" | tail -12
echo "adapter: $OUT  base: $BASE  iters: $ITERS  rows: $ROWS  epochs: $EPOCHS"
