#!/bin/sh
# The host-side trainer for the SLM arm: LoRA on a small Qwen with mlx_lm,
# from the corpus slm_corpus.py built out of a governed memory.
#
#   slm_train.sh <corpus-dir> <adapter-out> [--resume ADAPTER] [--base REPO] [--epochs N]
#
# Two ways to tune at a checkpoint, and the driver runs both:
#   from scratch   the plain base on the ACCUMULATED corpus (no --resume)
#   continual      --resume the previous checkpoint's adapter on the DELTA
#
# Overfitting is controlled rather than hoped against: iterations scale with
# the corpus (a fixed 200 steps on 20 rows is twenty epochs), validation
# loss is measured every 25 steps on rows carved from the experience set,
# and the checkpoint with the LOWEST validation loss is what gets kept --
# never the last one. The whole loss curve lands in the manifest.
#
# Areev never trains and ships no trainer; this is what a host plugs into
# `areev tune --cmd`.
set -eu
CORPUS="$1"; OUT="$2"; shift 2
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"; RESUME=""; EPOCHS=4
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
START=$(date +%s)
# No pipe to tee: a pipeline reports tee's exit status, and that is how a
# Metal crash after the first validation went unnoticed and an empty adapter
# directory was evaluated as a tuned model. Train, then show the log.
train() {
  # shellcheck disable=SC2086
  python3 -m mlx_lm lora --model "$BASE" --train --data "$CORPUS" --adapter-path "$OUT" \
    --iters "$ITERS" --batch-size "$1" --num-layers 8 --learning-rate 1e-4 \
    --mask-prompt --max-seq-length "$2" --steps-per-eval 25 --save-every 25 --val-batches 4 $3 \
    ${RESUME:+--resume-adapter-file "$RESUME/adapters.safetensors"} > "$OUT/train.log" 2>&1
}
if ! train "$BATCH" 3072 ""; then
  echo "first attempt failed (see $OUT/train.log); retrying with batch 1, shorter sequences, gradient checkpointing" >&2
  tr '\r' '\n' < "$OUT/train.log" | grep -iE "error|exception|failed" | tail -3 >&2
  BATCH=1
  train 1 2048 "--grad-checkpoint" || { echo "training failed twice; no adapter produced" >&2; exit 1; }
fi
[ -s "$OUT/adapters.safetensors" ] || { echo "training exited 0 but wrote no adapters.safetensors" >&2; exit 1; }
tr '\r' '\n' < "$OUT/train.log" | grep -E "^Iter|Trainable|Starting"
END=$(date +%s)
python3 - "$OUT" "$BASE" "$CORPUS" "$ITERS" "$((END-START))" "$RESUME" "$EPOCHS" <<'PY'
import json, sys, os, re, shutil
out, base, corpus, iters, secs, resume, epochs = sys.argv[1:]
log = open(os.path.join(out, "train.log")).read().replace("\r", "\n")  # tqdm bars share lines with the loss prints
val = [(int(i), float(v)) for i, v in re.findall(r"Iter (\d+): Val loss ([0-9.]+)", log)]
train = [(int(i), float(v)) for i, v in re.findall(r"Iter (\d+): Train loss ([0-9.]+)", log)]
# keep the checkpoint with the lowest validation loss, not the last one
best_iter, best_val = min(val, key=lambda x: x[1]) if val else (None, None)
chosen = "adapters.safetensors"
if best_iter:
    cand = os.path.join(out, "%07d_adapters.safetensors" % best_iter)
    if os.path.exists(cand):
        shutil.copy(cand, os.path.join(out, "adapters.safetensors"))
        chosen = os.path.basename(cand)
man = json.load(open(os.path.join(corpus, "corpus.manifest.json")))
json.dump({"base_model": base, "adapter_path": out, "iters": int(iters), "epochs": int(epochs),
           "train_seconds": int(secs), "resumed_from": resume or None, "corpus": man,
           "val_loss": val, "train_loss": train, "best_val_iter": best_iter, "best_val_loss": best_val,
           "kept_checkpoint": chosen,
           "fine_tune": {"type": "lora", "num_layers": 8, "batch_size": 2, "lr": 1e-4, "mask_prompt": True}},
          open(os.path.join(out, "adapter.manifest.json"), "w"), indent=1)
print("trained %s iters in %ss; best val %.3f at iter %s (kept %s) -> %s" % (iters, secs, best_val or -1, best_iter, chosen, out))
PY
