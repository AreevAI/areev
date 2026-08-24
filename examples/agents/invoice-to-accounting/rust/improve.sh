#!/bin/sh
set -eu
cd "$(dirname "$0")"
cargo build --release --quiet
export AGENT="${CARGO_TARGET_DIR:-$(pwd)/target}/release/agent"
export AGENT_OUT="$(pwd)/out"
exec ../improve.sh
