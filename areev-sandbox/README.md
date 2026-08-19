# areev-sandbox

Tier C for Areev: run a pure `wasm32` module with hard limits and a frozen
import set. Invoked as a subprocess.

```bash
areev-sandbox --module extract.wasm [--fuel N] [--max-pages N] < input.json
```

JSON on stdin, JSON on stdout, one process per invocation — the same contract as
every other Areev seam, so it drops into `--tool-cmd` with nothing new to learn.

## What this defends

It protects **the host from the tool**. A module cannot open a socket, touch the
filesystem, read an environment variable, see a clock, or run forever. That is
real isolation for pure-compute work: parsing, extraction, classification,
scoring.

It is **not credential protection**, and the two should never be described as
substitutes. A connector that legitimately holds an OAuth token and makes
outbound calls is not made safer by being in a sandbox — that is what the egress
allowlist and the credential broker do. By this design a Tier C module cannot
make a network call at all, so a Gmail connector will never be one.

## The guest contract

Export two functions and one memory; import one:

```wat
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) ...)   ;; host places input here
  (func (export "run") (param i32) (param i32) ...))     ;; ptr, len of the input
```

Anything else in the import section is refused **at instantiation**, by name,
rather than trapped later where the reason is harder to see. A module asking for
`wasi_snapshot_preview1` is told it is the wrong shape.

## Limits

| Limit | Default | Stops |
|---|---|---|
| module bytes, checked **before decode** | 16 MiB | a parse bomb, which does its damage inside the decoder |
| fuel | 200M | an infinite loop, deterministically |
| memory pages (declared max) | 256 (16 MiB) | a guest ballooning linear memory |
| payload | 8 MiB | an oversized input or result |

Fuel use is deterministic for a given module and input, which is what makes a
Tier C tool re-execution-provable.

## Why this is a standalone package

Not a workspace member, on purpose. It carries `wasmi`, and its dependency tree
and MSRV must not reach the workspace's `cargo deny`, MSRV check, or test time —
the same reasoning that keeps `areev-js` standalone.

`wasmi` on the **stable** line (0.51.x): five required dependencies, `no_std`
capable, MSRV below the workspace's. Compare `wasmtime`: 50 direct dependencies
and an MSRV above the workspace's. The 2.x line is beta-only at time of writing,
and shipping a pre-release runtime as a security boundary is the wrong trade.

## Build and test

```bash
cd areev-sandbox && cargo test
```
