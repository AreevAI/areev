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
# Qwen3 emits Hermes-style <tool_call> blocks; vLLM's hermes parser turns
# them into OpenAI tool_calls so the benchmark's runner sees real calls.
# No CUDA toolkit on the box: FlashInfer's sampler JIT-compiles with nvcc
# and killed the first start ("Could not find nvcc"); the torch sampler
# needs nothing built.
export VLLM_USE_FLASHINFER_SAMPLER=0
# enable_thinking=false by default, as the trainer rendered every row: the
# benchmark's runner cannot pass chat_template_kwargs, and a thinking
# preamble in `content` is what the judge would read.
# 0.70, not 0.85: the mem0 arm's embedder (ollama, mxbai-embed-large) takes
# ~0.7 GB of the same card whenever it runs, and CUDA-graph capture at 0.85
# OOMed on the first tuned evaluation while it did. A 1.7B model in bf16
# needs 3.4 GB; 0.70 leaves the rest for its KV cache and the neighbour.
# 16K, not 8K: at 8K six families (the information-gathering ones, whose
# prompts carry the session list and whole-thread search results) hit
# "maximum context length is 8192" mid-episode and the family-run died. The
# cap has to be the MODEL's limit, not one the harness chose, or the tuned
# arm is measured against an artificial ceiling. Qwen3-1.7B is native 32K;
# 16K is what the 8 GB card's KV cache holds beside the weights, and it is
# recorded as the tuned arm's context — one twentieth of the 262K the 30B
# was served with, which is the point the tuning claim is about.
# SLM_EAGER=1 skips CUDA-graph capture: it costs some speed and saves ~1 GB.
# Graph capture is what tipped the third condition over the 8 GB card after
# the first two had come up fine at the same settings, so the caller retries
# with it rather than losing a condition to a fragmentation-dependent OOM.
EAGER=""
[ "${SLM_EAGER:-0}" = "1" ] && EAGER="--enforce-eager"
UTIL="${SLM_GPU_UTIL:-0.85}"
COMMON="--dtype bfloat16 --max-model-len 16384 --gpu-memory-utilization $UTIL --seed ${SEED:-1} --enable-auto-tool-choice --tool-call-parser hermes --default-chat-template-kwargs {\"enable_thinking\":false} $EAGER"
if [ "$ADAPTER" = "none" ]; then
  # shellcheck disable=SC2086
  exec "$PY" -m vllm.entrypoints.openai.api_server --model "$BASE" --served-model-name slm --port "$PORT" $COMMON
fi
# shellcheck disable=SC2086
exec "$PY" -m vllm.entrypoints.openai.api_server --model "$BASE" --served-model-name slm-base --port "$PORT" $COMMON \
  --enable-lora --max-lora-rank 16 --lora-modules "slm=$ADAPTER"
