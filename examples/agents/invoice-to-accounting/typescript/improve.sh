#!/bin/sh
set -eu
cd "$(dirname "$0")"
[ -n "${AREEV_JS:-}" ] || { [ -f ../../../../crates/areev-js/index.js ] && export AREEV_JS="$(cd ../../../../crates/areev-js && pwd)"; } || true
export AGENT="${NODE:-node} --experimental-strip-types $(pwd)/agent.mts"
export AGENT_OUT="$(pwd)/out"
exec ../improve.sh
