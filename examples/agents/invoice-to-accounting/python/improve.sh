#!/bin/sh
set -eu
cd "$(dirname "$0")"
export AGENT="${PYTHON:-python3} $(pwd)/agent.py"
export AGENT_OUT="$(pwd)/out"
exec ../improve.sh
