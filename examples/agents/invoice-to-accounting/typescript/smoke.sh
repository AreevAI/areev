#!/bin/sh
# The TypeScript stack: `npm install @areev/areev`, one file, no build step
# (node >= 22.6 strips types natively). $AREEV_JS points at a checkout of
# crates/areev-js to run against the tree instead of the npm release.
set -eu
cd "$(dirname "$0")"
[ -n "${AREEV_JS:-}" ] || { [ -f ../../../../crates/areev-js/index.js ] && export AREEV_JS="$(cd ../../../../crates/areev-js && pwd)"; } || true
export AGENT="${NODE:-node} --experimental-strip-types $(pwd)/agent.mts"
export AGENT_OUT="$(pwd)/out"
exec ../smoke.sh
