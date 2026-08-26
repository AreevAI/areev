#!/usr/bin/env bash
# Areev Loop --llm-cmd backend using a local model via `ollama`.
# Protocol: one JSON request on stdin, one JSON response on stdout (see README).
# Requires: ollama, jq. Model via $AREEV_LOOP_LLM_MODEL (default llama3.1).
set -euo pipefail

model="${AREEV_LOOP_LLM_MODEL:-llama3.1}"
req="$(cat)"
op="$(printf '%s' "$req" | jq -r '.op')"

if [ "$op" = "probe" ]; then
  printf '{"model":"%s"}\n' "$model"
  exit 0
fi

instructions="$(printf '%s' "$req" | jq -r '.instructions')"
payload="$(printf '%s' "$req" | jq -c 'del(.instructions)')"

prompt="$instructions

Respond with ONLY the JSON object described above.
REQUEST:
$payload"

# `--format json` asks ollama to constrain output to a JSON object.
ollama run "$model" --format json "$prompt"
