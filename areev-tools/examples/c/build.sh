#!/bin/sh
# Build the C echo module (#340). The contract is docs/sandbox-abi.md.
#
#   c/build.sh OUT.wasm            zig cc (the pinned, hermetic path — what
#                                  produced the committed dist/echo-c.wasm)
#   c/build.sh --clang OUT.wasm    a system clang + wasm-ld (the plain LLVM
#                                  recipe; bytes depend on your LLVM version)
#
# Every flag is here for a reason the sandbox enforces:
#
#   -nostdlib / -ffreestanding   no libc — wasi-libc would import
#                                wasi_snapshot_preview1, refused by name
#   -mcpu=mvp -mbulk-memory      state the feature set instead of inheriting
#                                the compiler's default (it moves between LLVM
#                                releases); bulk-memory lets a memcpy lower to
#                                `memory.copy` rather than an `env::memcpy`
#                                import. NOT -msimd128: the sandbox's wasmi is
#                                built without SIMD and will not decode it
#   --no-entry                   a library, not a command: no `_start`
#   --max-memory=16777216        DECLARE a maximum (256 pages). No maximum
#                                reads as unbounded and is refused
#   -z stack-size=65536          one page of shadow stack
#   --strip-all                  no names/producers sections, so the bytes
#                                depend on the source and the toolchain only
#
# Exports come from `export_name` in the source; wasm-ld exports `memory` by
# default. Nothing else is exported and nothing else is imported.
set -eu
here=$(cd "$(dirname "$0")" && pwd)

mode=zig
if [ "${1:-}" = "--clang" ]; then
  mode=clang
  shift
fi
out=${1:?usage: c/build.sh [--clang] OUT.wasm}

set -- -O2 -nostdlib -ffreestanding -mcpu=mvp -mbulk-memory \
  -Wl,--no-entry -Wl,--max-memory=16777216 -Wl,-z,stack-size=65536 \
  -Wl,--strip-all -o "$out" "$here/echo.c"

if [ "$mode" = zig ]; then
  "${ZIG:-zig}" cc -target wasm32-freestanding "$@"
else
  # clang finds `wasm-ld` on PATH; WASM_LD names one that is not.
  if [ -n "${WASM_LD:-}" ]; then
    "${CLANG:-clang}" --target=wasm32 -fuse-ld="$WASM_LD" "$@"
  else
    "${CLANG:-clang}" --target=wasm32 "$@"
  fi
fi
