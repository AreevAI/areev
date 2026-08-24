#!/bin/sh
# The Rust stack: the same crates the bindings wrap, used directly.
# First build is a real compile; after that the binary is reused.
set -eu
cd "$(dirname "$0")"
cargo build --release --quiet
export AGENT="${CARGO_TARGET_DIR:-$(pwd)/target}/release/agent"
export AGENT_OUT="$(pwd)/out"
exec ../smoke.sh
