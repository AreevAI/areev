#!/bin/sh
# Build the Zig echo module (#340). The contract is docs/sandbox-abi.md.
#
#   zig/build.sh OUT.wasm
#
# Every flag is here for a reason the sandbox enforces:
#
#   -target wasm32-freestanding  no OS layer: nothing can import
#                                wasi_snapshot_preview1, refused by name
#   -mcpu mvp+bulk_memory        state the feature set instead of inheriting
#                                the compiler's default; NOT simd128 — the
#                                sandbox's wasmi is built without SIMD
#   -fno-entry                   a library, not a command: no `_start`
#   -rdynamic                    export the `export fn`s (alloc, run)
#   --max-memory=16777216        DECLARE a maximum (256 pages). No maximum
#                                reads as unbounded and is refused
#   --stack 65536                one page of shadow stack
#   -fstrip                      no names section, so the bytes depend on the
#                                source and the toolchain only
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: zig/build.sh OUT.wasm}

"${ZIG:-zig}" build-exe "$here/echo.zig" \
  -target wasm32-freestanding -mcpu mvp+bulk_memory -O ReleaseSmall \
  -fno-entry -rdynamic -fstrip --stack 65536 --max-memory=16777216 \
  -femit-bin="$out"
