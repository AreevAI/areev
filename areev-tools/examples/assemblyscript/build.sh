#!/bin/sh
# Build the AssemblyScript echo module (#340). The contract is
# docs/sandbox-abi.md.
#
#   assemblyscript/build.sh OUT.wasm
#
# Installs the compiler from the committed package-lock.json (`npm ci`, so the
# version that decides the bytes is the locked one), then:
#
#   --runtime stub        a bump allocator and no garbage collector, so there
#                         is no runtime to export (`__new`, `__pin`, …) and
#                         `heap.alloc` is the whole allocator
#   --use abort=          no `env::abort` import — refused by name otherwise
#   --maximumMemory 256   DECLARE a maximum (256 pages). No maximum reads as
#                         unbounded and is refused
#   --noAssert -O3z       small; with no `abort` to call, a failed assertion
#                         could only trap, so leave them out
#
# Run from this directory with a relative source path: the compiler names
# functions after the path, and although the default build emits no names
# section, keeping the path relative keeps that from ever mattering.
set -eu
out=${1:?usage: assemblyscript/build.sh OUT.wasm}
case "$out" in /*) ;; *) out="$(pwd)/$out" ;; esac
cd "$(dirname "$0")"

npm ci --silent --no-audit --no-fund
npx --no-install asc echo.ts \
  --runtime stub --use abort= --maximumMemory 256 --noAssert -O3z \
  --outFile "$out"
