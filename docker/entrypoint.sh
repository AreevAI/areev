#!/bin/sh
# Entrypoint for the areev image. One pseudo-verb, `heartbeat`, is provided
# by the IMAGE (a loop of one-shot `areev trigger run` — see heartbeat.sh);
# every other argument vector is handed to the `areev` binary verbatim, so
# `docker run areev ui …` and `docker run areev recall john` behave exactly
# like the CLI.
set -eu
if [ "${1:-}" = "heartbeat" ]; then
  shift
  exec areev-heartbeat "$@"
fi
exec areev "$@"
