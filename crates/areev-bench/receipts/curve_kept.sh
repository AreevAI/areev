#!/bin/sh
# Post-hoc: did the validation selector help or hurt? slm_train.sh keeps the
# checkpoint with the lowest validation loss -- on a four-row validation set,
# which is noise at the third decimal. Wherever it kept an EARLIER checkpoint
# than the last one saved, this reads the latest saved checkpoint on the
# unseen set too, so kept-vs-latest is a measurement. (The keep step copies
# the kept weights over the final ones, so "latest" is the last NUMBERED
# checkpoint -- iteration 50 of 72, say -- and that is stated in the record.)
# Runs after curve_tune.sh from what it persisted; touches nothing it wrote.
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 curve_kept.sh <seed-dir>
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
OUT="$1"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"
BASE="${SLM_BASE:-mlx-community/Qwen3-1.7B-4bit}"
for ck in "$OUT"/ck_*; do
  [ -d "$ck" ] || continue
  k=$(basename "$ck" | sed 's/^ck_0*//')
  if [ "$k" -ge "$EXP" ]; then db="$OUT/ledger.db"; else db="$OUT/snap_$(printf '%03d' "$k").db"; fi
  for mode in scratch continual; do
    ad="$ck/adapter_$mode"
    [ -f "$ad/adapter.manifest.json" ] || continue
    kept=$("$PY" -c "import json,sys; print(json.load(open(sys.argv[1]))['kept_checkpoint'])" "$ad/adapter.manifest.json")
    case "$kept" in 0*_adapters.safetensors) ;; *) continue ;; esac   # the last iteration was kept: nothing to compare
    latest=$(ls "$ad"/0*_adapters.safetensors | sort | tail -1)
    [ "$(basename "$latest")" != "$kept" ] || continue
    lat="$ck/adapter_${mode}_latest"
    if [ ! -s "$lat/adapters.safetensors" ]; then
      mkdir -p "$lat"; cp "$ad/adapter_config.json" "$lat/"; cp "$latest" "$lat/adapters.safetensors"
      "$PY" - "$lat" "$kept" "$(basename "$latest")" <<'PY'
import json, sys
json.dump({"latest_saved_checkpoint": sys.argv[3], "kept_by_selector": sys.argv[2],
           "note": "the final iteration's weights were overwritten by the keep step; this is the last numbered checkpoint"},
          open(sys.argv[1] + "/adapter.manifest.json", "w"), indent=1)
PY
    fi
    [ -f "$ck/eval_${mode}_latest_unseen/trials.json" ] && continue
    echo "######## seed $SEED: checkpoint $k — $mode adapter, latest saved checkpoint ($(basename "$latest") vs kept $kept) on unseen"
    SLM_BASE="$BASE" EXP="$EXP" EVAL="$EVAL" ARMS=B HOLDOUT=unseen \
      sh "$HERE/slm_eval.sh" "$db" "$ck/eval_${mode}_latest_unseen" "ck$(printf '%03d' "$k")-$mode-latest" "$lat"
  done
done
echo "######## seed $SEED: selector reads done — $OUT"
