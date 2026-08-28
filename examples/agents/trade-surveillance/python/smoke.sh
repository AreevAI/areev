#!/bin/sh
# The Python stack: `pip install areev`, one file, no other dependency.
set -eu
cd "$(dirname "$0")"
export AGENT="${PYTHON:-python3} $(pwd)/agent.py"
export AGENT_OUT="$(pwd)/out"
exec ../smoke.sh
