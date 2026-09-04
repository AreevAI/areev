#!/bin/sh
# The host-side trainer for the SLM arm: LoRA on a small Qwen with mlx_lm,
# from the corpus slm_corpus.py built out of a governed memory.
#
#   slm_train.sh <corpus-dir> <adapter-out> [iters 200] [base mlx-community/Qwen2.5-1.5B-Instruct-4bit]
#
# Areev never trains and ships no trainer; this is what a host plugs into
# `areev tune --cmd`. It is a few minutes on an M-series laptop and costs
# nothing, which is the whole argument for the arm: the loop's governed
# corpus turns into a model that runs the task without the LLM.
set -eu
CORPUS="$1"; OUT="$2"; ITERS="${3:-200}"; BASE="${4:-mlx-community/Qwen2.5-1.5B-Instruct-4bit}"
mkdir -p "$OUT"
START=$(date +%s)
python3 -m mlx_lm.lora --model "$BASE" --train --data "$CORPUS" --adapter-path "$OUT" \
  --iters "$ITERS" --batch-size 2 --num-layers 8 --learning-rate 1e-4 \
  --mask-prompt --max-seq-length 2048 --save-every 100 --val-batches 2 2>&1 | tee "$OUT/train.log"
END=$(date +%s)
python3 - "$OUT" "$BASE" "$CORPUS" "$ITERS" "$((END-START))" <<'PY'
import json, sys, os
out, base, corpus, iters, secs = sys.argv[1:]
man = json.load(open(os.path.join(corpus, "corpus.manifest.json")))
json.dump({"base_model": base, "adapter_path": out, "iters": int(iters), "train_seconds": int(secs),
           "corpus": man, "fine_tune": {"type": "lora", "num_layers": 8, "batch_size": 2, "lr": 1e-4,
                                       "mask_prompt": True}},
          open(os.path.join(out, "adapter.manifest.json"), "w"), indent=1)
print("trained in %ss -> %s" % (secs, out))
PY
