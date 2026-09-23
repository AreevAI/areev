# The sandbox guest ABI — writing a `wasm32-areev` module in any language

This page is the whole contract between a Tier C tool and `areev-sandbox`,
written so that you can build a conforming module **without reading any Rust**
([#340](https://github.com/AreevAI/areev/issues/340)). It describes what the
sandbox binary actually enforces — every rule below is checked in
`areev-sandbox/src/lib.rs` and proven by the tests named in
[Where this is proven](#where-this-is-proven).

Reference modules — one per toolchain, each echoing its input as its output —
live in [`areev-tools/examples/`](../areev-tools/examples/):

| Module | Language | Toolchain | Bytes | `sha256` of the committed file |
|---|---|---|---|---|
| `echo-c.wasm` | C | `zig cc` (Zig 0.15.2) — or any `clang --target=wasm32` + `wasm-ld` | 218 | `73998450edc0fa727b09d40c27908cc39721a5ff246c27a4e3125dc6b97e9457` |
| `echo-zig.wasm` | Zig | Zig 0.15.2, `-target wasm32-freestanding` | 210 | `da43b903410d6049014706a7873b9fe91526dbdad5e1641615443ecd3066c924` |
| `echo-as.wasm` | AssemblyScript | `assemblyscript` 0.28.20 (lockfile-pinned) | 231 | `fbf7fa80721c2b8ae3e56ebb68f323c62fb8b7dbda979d154853c578402107b3` |

For the host side — the two runtimes, the credential broker, and what the
sandbox defends — see [`areev-sandbox/README.md`](../areev-sandbox/README.md).
For how a Definition names a module and how a host pins it, see
[`docs/run.md`](run.md) ("Code-carrying tools", "Declared runtimes").

## The module in one screen

A **core** WebAssembly module (not a component), with 32-bit memory. It
exports one memory and two functions, and imports one function:

```wat
(module
  ;; the ONE import — module "areev", field "emit"
  (import "areev" "emit" (func $emit (param i32 i32)))

  ;; one exported linear memory, named "memory", with a DECLARED maximum
  (memory (export "memory") 1 256)

  ;; the host asks for `len` bytes and writes the input there
  (func (export "alloc") (param $len i32) (result i32) ...)

  ;; the host calls this once, with where the input is and how long it is
  (func (export "run") (param $ptr i32) (param $len i32) ...))
```

A complete, working module — an echo, the same thing the reference modules
do — is small enough to write by hand (`wat2wasm echo.wat`):

```wat
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 256)
  (global $top (mut i32) (i32.const 1024))
  (func (export "alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $top))
    (global.set $top (i32.add (local.get $p) (local.get $len)))
    ;; grow if the new top is past the current end (pages of 64 KiB)
    (if (i32.gt_u (global.get $top) (i32.mul (memory.size) (i32.const 65536)))
      (then (if (i32.eq (memory.grow
                          (i32.add (i32.div_u (i32.sub (global.get $top)
                                                       (i32.mul (memory.size) (i32.const 65536)))
                                              (i32.const 65536))
                                   (i32.const 1)))
                        (i32.const -1))
              (then unreachable))))
    (local.get $p))
  (func (export "run") (param $ptr i32) (param $len i32)
    (call $emit (local.get $ptr) (local.get $len))))
```

## Exports

| Export | Signature | Contract |
|---|---|---|
| `memory` | `(memory min max)` | **Required**, named exactly `memory`, and it must **declare a maximum** at or below the host's page ceiling (default 256 pages = 16 MiB). A memory with no maximum reads as unbounded and is refused. The check is on the *declared* maximum, not on current use. |
| `alloc` | `(func (param i32) (result i32))` | Return a pointer to `len` writable bytes in your own memory. The host calls it **once** for the input before `run`; under the `-io` runtime it calls it again to place each brokered response (see [`areev::fetch`](#the--io-imports-areevfetch-and-areevblob_get)). Never return `0` or a negative number: a response placed at `<= 0` is refused, and an input pointer is written where you said. |
| `run` | `(func (param i32 i32))` | Called **once**, with `(ptr, len)` — the pointer `alloc` returned and the input's length. Returns nothing; the result goes through `areev::emit`. |

Any other exports are ignored. Nothing is called by name except these three.
If the module has a `start` function it runs at instantiation — under the
same fuel budget, so it is not a free pass.

## Imports

**Everything you import must come from module `areev`, and must be on this
list.** Anything else is refused at instantiation, by name, before one
instruction runs — see [Refusals](#refusals).

### `areev::emit` — always linked

```wat
(import "areev" "emit" (func (param i32 i32)))   ;; ptr, len
```

Hands back the result: the host copies `[ptr, ptr+len)` out of your memory.

- The bytes must be **UTF-8 JSON** — any JSON value. They are parsed by the
  host; bytes that do not parse fail the run ("guest output is not JSON").
- **Call it once.** Each call replaces the previous one — the last call before
  `run` returns wins. A run that returns without calling it fails ("guest
  never called areev::emit"): silence is never an empty result.
- A `len` above the 8 MiB payload cap fails the run with a payload error; a
  range outside your memory is not copied (so an earlier emit, if any, stands).
- The host **re-serializes** what you emit. On stdout you get the same JSON
  value, not necessarily the same bytes: insignificant whitespace is gone,
  numbers are normalized (`1.50` → `1.5`), and in the current build object keys
  come out sorted. Emit JSON whose *value* is your answer.

### The `-io` imports: `areev::fetch` and `areev::blob_get`

Linked **only** when the host opted in — the engine does so only for a
manifest-pinned `wasm32-areev-io` runtime (`--allow-fetch`) or a Definition
that declared `{"blob": {"read": true}}` (`--allow-blob`). Importing either
anywhere else is refused by name. A pure `wasm32-areev` module imports neither.

```wat
(import "areev" "fetch"    (func (param i32 i32) (result i32)))  ;; one brokered HTTP call
(import "areev" "blob_get" (func (param i32 i32) (result i32)))  ;; one CAS read, by address
```

Both take a UTF-8 JSON request at `[ptr, ptr+len)`:

```json
{ "url": "https://api.vendor.example/v2/invoices/7", "method": "GET",
  "credential": "vendor", "body": null, "headers": { "X-Api-Version": "2" } }
```
```json
{ "uri": "cas://sha256:<64 hex>" }
```

Both answer the same way. A **non-negative** return is a pointer, obtained by
the host calling *your* `alloc(n + 4)`, to `[n as u32 little-endian][n bytes]`.
For `fetch` the bytes are JSON — `{"status": 200, "body": "…"}` when the call
was made, `{"error": "…", "code": "…"}` when policy or the broker refused it. For
`blob_get` they are the blob itself (raw bytes, not base64), or a JSON
`{"error": "…"}` object on failure. A **negative** return means the host could
not place an answer at all (no usable `alloc`, allocation refused, memory not
writable).

One call at a time, synchronous. Calling `fetch` or `blob_get` from inside
your own `alloc` (while a response is being placed) returns `-1` immediately.
Responses are capped (`fetch`: 1 MiB by default, `blob_get`: 8 MiB); an
overrun is an error, never truncation. The request/response shapes, the
header rules and the broker are specified in
[`areev-sandbox/README.md`](../areev-sandbox/README.md).

## Input and output framing

1. The host serializes the tool's input as compact UTF-8 JSON (the input the
   engine hands any tool — see [`docs/run.md`](run.md); the CLI reads it from
   stdin, and empty stdin is `null`). It is never empty: the smallest input is
   one byte (`0`).
2. It calls `alloc(len)` and writes exactly `len` bytes at the returned
   pointer. There is no terminator and no length prefix — the length is the
   second argument to `run`.
3. It calls `run(ptr, len)`.
4. After `run` returns, it takes the bytes of the last `emit` as the result.

Input and output are each capped at **8 MiB**.

## Limits

| Limit | Default | Set by | On overrun |
|---|---|---|---|
| module size, checked **before decode** | 16 MiB | fixed | refused: "module rejected" |
| fuel | 200,000,000 | `--fuel N`; Definition `runtime_limits.fuel` | "guest exhausted its fuel" |
| memory — declared maximum | 256 pages (16 MiB) | `--max-pages N`; `runtime_limits.max_pages` | refused at instantiation |
| input / output payload | 8 MiB | fixed | payload error |
| `fetch` response | 1 MiB | `--max-response-bytes N` | JSON error to the guest |
| `blob_get` blob | 8 MiB | `--max-blob-bytes N` | JSON error to the guest |

Growing memory past your declared maximum fails the `memory.grow` (it returns
`-1`), exactly as WebAssembly specifies; trap rather than hand the host a
pointer you could not back.

### Fuel

Fuel bounds execution **deterministically**: the same module on the same input
spends the same fuel, every time, on every machine — which is also how a
runaway loop is stopped without a clock. The sandbox runs `wasmi` (0.51.x, the
stable line) with its default cost model:

- roughly **one unit per executed instruction** — but of wasmi's *internal*
  instruction set, which it translates your module into, so the count is not
  the number of WebAssembly instructions and can change with the wasmi version;
- bulk memory operations and `memory.grow` cost **one unit per 64 bytes**
  (growing one 64 KiB page costs 1,024);
- the `start` function, `alloc` (including the calls that place `fetch`
  answers) and `run` all draw on one budget. Host-side work — copying the
  input in, copying the emit out, the broker round trip — is not charged.

`areev-sandbox` reports what was spent on stderr (`areev-sandbox: fuel used
N`); the library returns it as `fuel_used`. The reference echoes spend about
1,900 units on a small input, most of it growing memory by one page. Compare
fuel across runs of **one sandbox build**; do not budget to the last unit
across wasmi upgrades.

## What "deterministic" requires

A `wasm32-areev` tool is **pure — re-execution-provable**: a host can re-run it
and must get the same bytes and the same fuel. That is what lets a rehearsal
re-execute a pure module (`areev run shadow --reexecute pure`) instead of
replaying its journaled answer. The sandbox makes most of this structural:

- **No clock, no randomness, no environment, no filesystem, no network.**
  These are not forbidden calls; there is nothing to import that could reach
  them. A toolchain that tries — `Date.now`, `Math.random`, `getenv`,
  `printf` through a libc — produces an import, and the import is refused.
- **No threads.** There is no shared memory and nothing to spawn.
- Memory starts **zero-filled**, so reading memory you did not write is still
  deterministic.

What remains is yours:

- **Seed nothing from addresses or timing** — there is no timing to read, but a
  hash map seeded from a pointer is still a program whose output depends on its
  allocator. Constant seeds only.
- **Keep NaN bit patterns out of your output.** WebAssembly leaves the payload
  of a NaN produced by arithmetic nondeterministic. JSON cannot carry a NaN
  anyway; do not smuggle one through as bits.
- An `-io` module (`wasm32-areev-io`) is deterministic **modulo its journaled
  effects**: the same inputs *and* the same brokered answers give the same
  output. Its `fetch` answers are recorded by the engine, which is why it is a
  separate runtime name.

## Refusals

The import section is checked **before instantiation** — before a `start`
function, before `alloc`, before one instruction. Every import must be
`areev::emit`, or `areev::fetch` / `areev::blob_get` on a host that linked
them. Anything else — `wasi_snapshot_preview1::*`, `env::abort`,
`env::memcpy`, `env::seed` — is refused, and the refusal names the import and
this page:

```text
areev-sandbox: module imports wasi_snapshot_preview1::fd_write, which is not in
the frozen import set — a Tier C module may import only areev::emit, plus
areev::fetch / areev::blob_get where the host links them (areev::alloc is a
guest EXPORT the host calls, not an import); WASI is not provided — build for a
freestanding target (no libc, no wasi-libc). The guest ABI is
docs/sandbox-abi.md (github.com/AreevAI/areev)
```

There is no flag that admits WASI. A module built for `wasm32-wasi` /
`wasm32-wasip1` is the wrong shape and stays refused.

Other refusals, all before `run`: a module over 16 MiB (before decoding); bytes
that do not decode (including SIMD, which this sandbox's wasmi is built
without); a declared memory maximum above the ceiling, or none; no `alloc`
with the signature above; no `run` with the signature above; no exported
`memory`.

## Toolchain recipes

The pattern is the same everywhere: **freestanding target, no libc, no entry
point, the import placed under module `areev`, a declared memory maximum.**
Each reference module's `build.sh` carries its flags with the reason for each.

### C — `clang --target=wasm32 -nostdlib`

[`areev-tools/examples/c/`](../areev-tools/examples/c/)

```c
__attribute__((import_module("areev"), import_name("emit")))
extern void areev_emit(const unsigned char *ptr, int len);

__attribute__((export_name("alloc"))) unsigned char *alloc(int len) { /* bump + memory.grow */ }
__attribute__((export_name("run")))   void run(const unsigned char *p, int len) { areev_emit(p, len); }
```

```sh
clang --target=wasm32 -O2 -nostdlib -ffreestanding -mcpu=mvp -mbulk-memory \
  -Wl,--no-entry -Wl,--max-memory=16777216 -Wl,-z,stack-size=65536 -Wl,--strip-all \
  -o echo-c.wasm echo.c
```

- Without `import_module`, clang imports from `env` — refused.
- `-Wl,--max-memory` is the declared maximum; without it there is none.
- Bring your own `memcpy`/`memset` or keep `-mbulk-memory` so the compiler
  lowers them to instructions; otherwise it emits an `env::memcpy` import.
- Not `-msimd128`: SIMD modules do not decode here.
- Needs `wasm-ld` (Debian/Ubuntu: `apt install clang lld`). Apple's Xcode clang
  has the wasm32 backend but ships no `wasm-ld`; use Homebrew `llvm` + `lld`,
  or `zig cc -target wasm32-freestanding` with the same flags, which is how the
  committed `echo-c.wasm` is built.

### Zig — `-target wasm32-freestanding`

[`areev-tools/examples/zig/`](../areev-tools/examples/zig/)

```zig
extern "areev" fn emit(ptr: [*]const u8, len: i32) void;
export fn alloc(len: i32) [*]u8 { ... @wasmMemoryGrow(0, pages) ... }
export fn run(ptr: [*]const u8, len: i32) void { emit(ptr, len); }
```

```sh
zig build-exe echo.zig -target wasm32-freestanding -mcpu mvp+bulk_memory -O ReleaseSmall \
  -fno-entry -rdynamic -fstrip --stack 65536 --max-memory=16777216 -femit-bin=echo-zig.wasm
```

- `extern "areev"` names the import module. `-rdynamic` exports the
  `export fn`s; `-fno-entry` drops `_start`.
- Do not target `wasm32-wasi`: the standard library then imports WASI.

### AssemblyScript — `--runtime stub`, no `abort`

[`areev-tools/examples/assemblyscript/`](../areev-tools/examples/assemblyscript/)

```ts
@external("areev", "emit")
declare function emit(ptr: usize, len: i32): void;
export function alloc(len: i32): usize { return heap.alloc(<usize>len); }
export function run(ptr: usize, len: i32): void { emit(ptr, len); }
```

```sh
npx asc echo.ts --runtime stub --use abort= --maximumMemory 256 --noAssert -O3z \
  --outFile echo-as.wasm
```

- `--use abort=` removes the default `env::abort` import. `--runtime stub` is
  a bump allocator with no collector, so no runtime helpers are exported.
- `Date.now()`, `Math.random()` and `trace()` each add an `env` import and are
  refused — which is the determinism rule enforcing itself.

### Rust

`#![no_std]` for `wasm32-unknown-unknown`, with the linker told the maximum
(`-C link-arg=--max-memory=16777216`). The blessed tools are the worked
example: [`areev-tools/common`](../areev-tools/common/) is the guest half of
this contract, and [`areev-tools/README.md`](../areev-tools/README.md) explains
why they avoid `serde_json`.

### Checking a module before you ship it

```sh
echo '{"hello":"world"}' | areev-sandbox --module my-tool.wasm --fuel 1000000 --max-pages 256
```

It prints your output on stdout and the fuel spent on stderr, or refuses with
the reason. `wasm-objdump -x` or `wasm-tools print` shows the import section:
it should list `areev.emit` and nothing else.

## Reproducible bytes, and the address a host pins

A host runs a module only after pinning its **content address** —
`cas://sha256:<hex>`, the SHA-256 of the `.wasm` file's bytes, named by the
Definition's `executor_uri` and pinned with `--allow-executor`
([`docs/run.md`](run.md)). So "the same source" is not enough to share a tool;
the same *bytes* are, and that depends on the compiler version.

What the reference modules show:

- **Zig 0.15.2** (the official tarball) builds `echo-c.wasm` (via `zig cc`,
  which bundles its own clang and `wasm-ld`) and `echo-zig.wasm`
  **byte-identically** on macOS arm64, Linux x86_64 and Linux arm64 — verified
  on all three, and re-asserted by CI on Linux x86_64. A self-contained
  compiler at a pinned version is the reproducible C path.
- **AssemblyScript 0.28.20** (with the `binaryen` its committed
  `package-lock.json` pins) builds `echo-as.wasm` byte-identically on macOS
  arm64 and Linux x86_64 (Node 22).
- **A system clang** is accepted but not reproducible *across* LLVM versions:
  Ubuntu 24.04's clang 18 + lld 18 builds a working 219-byte `echo-c.wasm`
  with a different address from the committed 218-byte one. CI builds that
  recipe too and asserts only that the sandbox accepts it and it echoes.

Rebuild and compare with:

```sh
areev-tools/examples/build.sh           # rebuild dist/ (needs zig 0.15.2 + node/npm)
areev-tools/examples/build.sh --check   # rebuild to a scratch dir; fail unless identical
```

To publish your own module's address, pin your compiler the same way —
exact version, stripped output (no name or producers sections), and a
relative source path — and record the SHA-256 beside the source.

## Where this is proven

- [`areev-sandbox/tests/abi_examples.rs`](../areev-sandbox/tests/abi_examples.rs)
  runs each committed reference module through the sandbox library under an
  explicit fuel limit and memory ceiling: output equals input (including
  non-ASCII UTF-8 and a 300 KiB payload that forces `memory.grow`), two runs
  spend identical fuel and emit identical bytes, fuel is a hard stop at exactly
  the budget, a ceiling one page below the declared maximum refuses the
  module, each module's imports are exactly `areev::emit`, the CLI accepts
  each one, and a WASI import (and an `env::abort` import) is refused naming
  this page.
- [`crates/areev-conformance/tests/sandbox_abi.rs`](../crates/areev-conformance/tests/sandbox_abi.rs)
  (`--features sandbox`) runs the same modules through the **shipped
  `areev-sandbox` binary** — the process the engine spawns — twice each,
  comparing fuel and output bytes, and checks the WASI refusal names this page.
  It reaches the sandbox as a subprocess because `areev-sandbox` is
  deliberately not a workspace member (it carries `wasmi`). It skips locally
  without the binary and **fails under `CI=true`** without it.
- CI's `sandbox` job installs Zig 0.15.2 (pinned tarball + SHA-256),
  Node's AssemblyScript (lockfile) and Ubuntu's `clang` + `lld`, rebuilds all
  three modules from source, asserts the Zig/`zig cc`/AssemblyScript bytes are
  identical to the committed ones, runs the system-clang build through the
  sandbox, and runs the conformance case.

## Why a freestanding ABI, not a WASI shim or an interpreter

#340 offered three ways to open the tier to non-Rust authors. This page is the
first — a written freestanding ABI plus a reference module per toolchain — and
the other two were declined for reasons worth keeping:

**A `wasi_snapshot_preview1` shim** (link a stub WASI only when a Definition
declares `abi: wasi-minimal`: `fd_write` captured as the emit, `clock_time_get`
returning zero, `random_get` returning a fixed seed) would let an ordinary
`wasm32-wasi` build run unchanged — and that is the problem. A shim *fakes*
syscalls: a program that reads the clock gets `0` and a program that asks for
randomness gets the same bytes forever, and neither finds out. That silently
changes the program's semantics (a UUID generator that always returns one
UUID, a retry loop that never sees time pass) while the module runs "fine",
which is a worse failure than a refusal. It would also widen the import
surface the sandbox's by-name refusal exists to keep closed — from one
function to the WASI table, each entry a place to get determinism wrong — and
make "which WASI calls are faked, and how" part of every pinned tool's
meaning. A freestanding module has no such calls to fake: what it cannot do,
it cannot import, and the refusal says so before one instruction runs.

**An interpreter module** (QuickJS or MicroPython compiled as a
`wasm32-areev` module, taking the tool's script as part of its input) leaves
the sandbox contract untouched, but moves the tool out of reach of the thing
that makes Tier C worth having. The pinned, content-addressed artifact would
be the *interpreter* — a large module with its own supply chain to review —
while the tool's actual logic rides inside the input, where no
`--allow-executor` pin covers it and no address names it. Two tools would
share one address and differ only in data. Its fuel would measure the
interpreter's work rather than the tool's. It remains possible for anyone to
build one against this page — nothing here forbids it — but it is not the
authoring path Areev ships.
