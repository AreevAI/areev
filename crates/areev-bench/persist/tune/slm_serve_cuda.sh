#!/bin/sh
# Serve a small Qwen, tuned or not, behind an OpenAI-compatible endpoint with
# vLLM on the box, for `scripts/slm_serve.py` (the bench's JSON-on-stdio
# agent leg) and for `areev eval run --model`.
#
#   slm_serve_cuda.sh [adapter-dir|none] [port]      # foreground; Ctrl-C stops it
#
# With an adapter the model is served under the name `slm` with the LoRA
# applied (vLLM --enable-lora); with `none` the plain base is served as
# `slm` so the untuned control uses the identical client path. The GPU is
# shared with training only by turns — never start this while a trainer runs.
set -eu
ADAPTER="${1:-none}"; PORT="${2:-8300}"
BASE="${SLM_BASE:-Qwen/Qwen3-1.7B}"
PY="${PY:-$HOME/mg/local/persist-venv/bin/python}"
if [ "$ADAPTER" != "none" ] && [ ! -s "$ADAPTER/adapter_model.safetensors" ]; then
  echo "slm_serve_cuda: $ADAPTER has no adapter_model.safetensors -- refusing to serve an empty adapter as a tuned model" >&2
  exit 1
fi
if [ "$ADAPTER" = "none" ]; then
  exec "$PY" -m vllm.entrypoints.openai.api_server --model "$BASE" --served-model-name slm --port "$PORT" \
    --dtype bfloat16 --max-model-len 8192 --gpu-memory-utilization 0.85 --seed "${SEED:-1}"
fi
exec "$PY" -m vllm.entrypoints.openai.api_server --model "$BASE" --served-model-name slm-base --port "$PORT" \
  --dtype bfloat16 --max-model-len 8192 --gpu-memory-utilization 0.85 --seed "${SEED:-1}" \
  --enable-lora --max-lora-rank 16 --lora-modules "slm=$ADAPTER"
