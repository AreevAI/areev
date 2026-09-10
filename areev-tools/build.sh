#!/bin/sh
# Build the blessed wasm32-areev-io tools and refresh dist/.
#
#   areev-tools/build.sh            # build, copy to dist/, rewrite blessed.json
#   areev-tools/build.sh --check    # fail if dist/ does not match the tree
#
# What ships is dist/*.wasm — committed, content-addressed, and pinned by
# address wherever it runs. A rebuild on a different rustc will produce
# different bytes and therefore a different address, which is why the built
# artifact is the thing under review and not a build recipe: `--check` compares
# the COMMITTED blobs against the manifest, and a deliberate rebuild is a
# commit that moves both together (and every pack pinning the old address).
set -eu
cd "$(dirname "$0")"

if ! rustc --print target-list | grep -qx wasm32-unknown-unknown; then
  echo "this toolchain does not know wasm32-unknown-unknown" >&2
  exit 1
fi
if [ "${1:-}" = "--check" ]; then
  python3 manifest.py --check
  exit $?
fi

rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true
cargo build --release --target wasm32-unknown-unknown
python3 manifest.py
