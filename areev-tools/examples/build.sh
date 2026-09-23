#!/bin/sh
# Build the non-Rust reference modules (#340) into examples/dist/.
#
#   examples/build.sh            rebuild dist/echo-{c,zig,as}.wasm
#   examples/build.sh --check    rebuild into a scratch dir and fail unless the
#                                bytes are IDENTICAL to the committed ones
#
# The C and Zig modules come from ONE pinned toolchain, Zig $ZIG_VERSION: `zig cc`
# compiles the C module (Zig bundles its own clang and wasm-ld) and
# `zig build-exe` the Zig one. That is deliberate — a self-contained compiler
# at a pinned version is what makes the committed bytes reproducible on
# another machine, and a host pins a module by its content address. A system
# clang builds the C module too (`c/build.sh --clang`), and the sandbox
# accepts the result, but its bytes follow your LLVM version.
#
# The AssemblyScript module needs Node (>= 20) and npm: its compiler comes from
# the committed package-lock.json, and that locked version decides its bytes.
#
# The contract these modules implement is docs/sandbox-abi.md. These are
# examples, not blessed tools: nothing pins their addresses, and they are not
# part of the areev-tools cargo workspace or dist/blessed.json.
set -eu
cd "$(dirname "$0")"

ZIG_VERSION=0.15.2
ZIG=${ZIG:-zig}
export ZIG

have=$("$ZIG" version 2>/dev/null || echo none)
if [ "$have" != "$ZIG_VERSION" ]; then
  echo "need zig $ZIG_VERSION for reproducible bytes, found: $have" >&2
  echo "(set ZIG=/path/to/zig, or build one module by hand with c/build.sh or zig/build.sh)" >&2
  exit 1
fi

if [ "${1:-}" = "--check" ]; then
  out=$(mktemp -d)
  trap 'rm -rf "$out"' EXIT
else
  out=dist
  mkdir -p "$out"
fi

c/build.sh "$out/echo-c.wasm"
zig/build.sh "$out/echo-zig.wasm"
assemblyscript/build.sh "$out/echo-as.wasm"

sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

status=0
for m in echo-c echo-zig echo-as; do
  if [ "$out" = dist ]; then
    echo "$m.wasm  sha256:$(sha "dist/$m.wasm")"
  elif cmp -s "$out/$m.wasm" "dist/$m.wasm"; then
    echo "$m.wasm  reproduced  sha256:$(sha "dist/$m.wasm")"
    # The ABI page quotes each address; a page quoting stale bytes is a
    # reader pinning a module nobody built.
    if ! grep -q "$(sha "dist/$m.wasm")" ../../docs/sandbox-abi.md; then
      echo "$m.wasm  address not quoted in docs/sandbox-abi.md" >&2
      status=1
    fi
  else
    echo "$m.wasm  DIFFERS: committed sha256:$(sha "dist/$m.wasm"), rebuilt sha256:$(sha "$out/$m.wasm")" >&2
    status=1
  fi
done
exit $status
