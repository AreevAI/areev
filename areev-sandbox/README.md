# areev-sandbox

Tier C for Areev: run a `wasm32` module with hard limits and a frozen import
set. Invoked as a subprocess.

```bash
areev-sandbox --module extract.wasm [--fuel N] [--max-pages N] \
              [--allow-fetch] [--max-response-bytes N] < input.json
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
allowlist and the credential broker do.

## Two runtimes

Until 1.6.0 there was one, and its rule was absolute: a Tier C module could not
make a network call at all, so a Gmail connector would never be one. That was
also why the tier half-delivered — it is the only one producing a persistable,
content-addressed tool, and it forbade exactly the I/O real agents need.

`--allow-fetch` splits it in two:

| Engine runtime | Import set | Determinism |
|---|---|---|
| `wasm32-areev` | `areev::emit` | pure — **re-execution-provable** |
| `wasm32-areev-io` | `+ areev::fetch` | deterministic **modulo journaled effects** |

The guest still gets no socket. It gets one unforgeable capability to **ask the
host**, and this binary forwards to the engine's credential broker over
loopback, holding a revocable broker token and never a credential:

```text
guest wasm ──areev::fetch(req)──▶ THIS BINARY ──loopback──▶ engine broker ──▶ upstream
  no socket, no env, no clock      broker token only         holds credentials,
                                                             enforces policy,
                                                             journals everything
```

The engine passes `--allow-fetch` only for a **manifest-pinned**
`wasm32-areev-io` runtime whose Definition declared a matching `capabilities`
set — a blob cannot talk its way into the gate.

## The guest contract

Export two functions and one memory; import one, or two under `--allow-fetch`:

```wat
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) ...)   ;; host places input here
  (func (export "run") (param i32) (param i32) ...))     ;; ptr, len of the input
```

Anything else in the import section is refused **at instantiation**, by name,
rather than trapped later where the reason is harder to see. A module asking for
`wasi_snapshot_preview1` is told it is the wrong shape — and so is one asking
for `areev::fetch` without `--allow-fetch`, which is what extends the frozen-
import philosophy from "which imports" to "which capabilities".

### `areev::fetch(ptr, len) -> i32`

**In**: `[ptr, ptr+len)` is a UTF-8 JSON request — exactly the shape the broker
takes, forwarded rather than translated, so there is no place for a translation
bug:

```json
{ "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
  "method": "GET", "credential": "gmail", "body": null,
  "headers": { "X-Goog-User-Project": "my-project" } }
```

The guest names *which* credential; it can never name a value it was not given,
and no value ever crosses back.

`headers` (optional) carries non-credential request headers the module
declared in `capabilities.http.headers` — the quota-project, API-version, and
tenant headers enterprise APIs require. The broker refuses `Authorization`,
`Proxy-Authorization`, `Cookie`, `Host`, and any header a configured
credential rides in: those are its to set, and a guest that could write them
would hold the credential channel this whole boundary exists to keep it out
of.

**Out**: a non-negative return is a pointer to `[u32 little-endian length][JSON
bytes]` in guest memory, allocated through the guest's own `alloc` export. One
`i32` cannot carry a pointer and a length, and a second import would be a second
gate, so the response is self-describing instead.

A **negative** return means the host could not place a response at all. Every
other outcome — a policy refusal, a broker failure, an upstream 500 — arrives as
ordinary JSON, so the guest has one shape to handle:

```json
{ "status": 200, "body": "…" }          // it worked
{ "error": "…", "code": "RUN-E022" }    // policy said no
```

One call at a time, synchronous. No concurrency in v1: completion-order
nondeterminism is what durable-execution engines spend enormous machinery
taming, and it would add an ordering side channel to a boundary whose whole
point is that it leaks nothing.

## Limits

| Limit | Default | Stops |
|---|---|---|
| module bytes, checked **before decode** | 16 MiB | a parse bomb, which does its damage inside the decoder |
| fuel | 200M | an infinite loop, deterministically |
| memory pages (declared max) | 256 (16 MiB) | a guest ballooning linear memory |
| payload | 8 MiB | an oversized input or result |
| response bytes (`--max-response-bytes`) | 1 MiB | an upstream ballooning the guest's memory |

Overruns are typed errors, never truncation — a tool handed half a response
computes a wrong answer with nothing to show for it.

Fuel use is deterministic for a given module and input, which is what makes a
**pure** Tier C tool re-execution-provable. A module that made brokered calls is
deterministic only modulo those calls; the outcome's `fetches` count is how you
tell the two apart.

## Why this is a standalone package

Not a workspace member, on purpose. It carries `wasmi`, and its dependency tree
and MSRV must not reach the workspace's `cargo deny`, MSRV check, or test time —
the same reasoning that keeps `areev-js` standalone.

`wasmi` on the **stable** line (0.51.x): five required dependencies, `no_std`
capable, MSRV below the workspace's. Compare `wasmtime`: 50 direct dependencies
and an MSRV above the workspace's. The 2.x line is beta-only at time of writing,
and shipping a pre-release runtime as a security boundary is the wrong trade.

The broker leg keeps that property: it is a hand-rolled ~100-line HTTP/1.1 POST
in `src/http.rs`, not a client dependency. The destination is always loopback,
so there is no TLS — no certificates, no roots, no negotiation — which makes an
HTTP client a poor trade for the part you trust to hold a line. Same reasoning
as `areev-server`'s std-only console.

## Build and test

```bash
cd areev-sandbox && cargo test
```
