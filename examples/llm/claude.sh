#!/usr/bin/env bash
# Areev Loop --llm-cmd backend using the Claude Code CLI (`claude -p`).
# Protocol: one JSON request on stdin, one JSON response on stdout (see README).
# Requires: claude, jq.
set -euo pipefail

req="$(cat)"
op="$(printf '%s' "$req" | jq -r '.op')"

# Probe is answered locally — no model call.
if [ "$op" = "probe" ]; then
  printf '{"model":"claude-code"}\n'
  exit 0
fi

# Keep instructions (the system prompt) separate from evidence text. Forward
# every other key: GROUND sends `claims`, so a fixed key list would empty it.
instructions="$(printf '%s' "$req" | jq -r '.instructions')"
payload="$(printf '%s' "$req" | jq -c 'del(.instructions)')"

prompt="$instructions

Respond with ONLY the JSON object described above — no markdown, no prose.
REQUEST:
$payload"

# `claude -p` prints the model's text. The instructions ask for raw JSON;
# the loop drops anything that doesn't parse, so a stray wrapper is safe (yields
# no drafts). Strip a ```json fence if the model adds one.
claude -p "$prompt" | sed -e 's/^```json//' -e 's/^```//' -e '/^```$/d'
