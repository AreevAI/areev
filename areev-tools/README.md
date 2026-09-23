# areev-tools

The blessed `wasm32-areev-io` tools, and the example connector built beside
them. What ships is `dist/*.wasm` — committed, content-addressed blobs whose
addresses `dist/blessed.json` records and
[`docs/blessed-tools.md`](../docs/blessed-tools.md) documents.

```bash
./build.sh            # build for wasm32, refresh dist/, print the addresses
./build.sh --check    # what CI asserts: dist/ describes the committed blobs
```

| Crate | Ships as | Does |
|---|---|---|
| `http-call` | `http.call` | forwards one request to the broker and the answer back — verbatim, both ways |
| `mcp-call` | `mcp.call` | JSON-RPC `tools/call` around the caller's `arguments`, `result` unwrapped |
| `a2a-call` | `a2a.call` | JSON-RPC `message/send` around a message or a line of text |
| `mailbox-poll` | `mailbox.poll` | the trigger-connector example: reads a filed feed by content address |
| `rest-poll` | `rest.poll` | polls a paginated REST source; the Definition's `config` maps items, id, cursor and pages |
| `common` | — | the guest ABI (allocator, panic handler, the three imports) and a JSON **slicer** |

`rest-poll` carries its own JSON-pointer slicer rather than adding one to
`common`, and the reason is the tier's own rule: a blessed blob's identity is
its content address, and adding even an unused function to the shared crate
changes the bytes of every blob built from it — which would re-point every pack
that pinned one. Moving a helper into `common` is a deliberate re-blessing of
all of them, not a refactor.

## Why `no_std` and hand-rolled JSON

A blessed blob is reviewed as bytes and pinned by address, so what it carries
is part of its security story. `serde_json` would pull an ecosystem into a
module whose whole job is to move four strings into a different shape, and
would multiply an artifact a human is expected to read the disassembly of.
`http.call` is 2.6 KB.

What `common::json` provides instead is a *slicer*: it finds a member of a
top-level object and hands back the raw bytes of its value, so a caller's
`arguments` object travels into the request verbatim — never reserialized, so
no round trip can change it. It is not a validating parser and does not pretend
to be: a malformed input yields a missing member, and the tool then sends a
request the broker or the upstream refuses, which are the authorities that were
going to decide anyway.

## The guest contract

Three imports, two exports, one memory
([`areev-sandbox/README.md`](../areev-sandbox/README.md) is the host side):

```
import  areev::emit(ptr, len)              hand the result back
import  areev::fetch(ptr, len) -> i32      one brokered HTTP call
import  areev::blob_get(ptr, len) -> i32   one CAS read, by address
export  alloc(len) -> ptr                  the host places input here
export  run(ptr, len)                      and calls this
export  memory                             max 256 pages, declared
```

**An import appears only when it is called.** That is load-bearing, not
incidental: the sandbox refuses an import the host did not link, by name,
before one instruction runs — so `mailbox.poll`, which declares only
`{"blob": {"read": true}}`, must not import `areev::fetch`. `manifest.py`
asserts each blob's import set against what its declaration admits, and
`build.sh` fails the build rather than shipping a blob that would be refused
at instantiation.

The declared memory maximum comes from `.cargo/config.toml`
(`--max-memory=16777216`): rustc emits no maximum unless the linker is told
one, and an absent maximum reads as unbounded — exactly what the sandbox's page
ceiling exists to refuse.

Written out without any Rust, for authors in other languages, in
[`docs/sandbox-abi.md`](../docs/sandbox-abi.md) (#340).

## Non-Rust reference modules — `examples/`

`examples/` holds one echo module per non-Rust toolchain, each built from the
contract page above rather than from `common`: C (`c/`, via `zig cc` or
`clang --target=wasm32 -nostdlib`), Zig (`zig/`, `wasm32-freestanding`) and
AssemblyScript (`assemblyscript/`, `--runtime stub`). They are examples, not
blessed tools — not in this cargo workspace, not in `dist/blessed.json`, and
nothing pins them.

```bash
examples/build.sh            # rebuild examples/dist/ (zig 0.15.2 + node/npm)
examples/build.sh --check    # what CI asserts: a rebuild is byte-identical
```

Unlike the Rust blobs, these **are** checked for byte identity on rebuild: the
toolchains are pinned (the official Zig 0.15.2 tarball, which bundles its own
clang and wasm-ld; the AssemblyScript compiler from a committed lockfile) and
reproduce the same bytes across macOS and Linux. `areev-sandbox/tests/abi_examples.rs`
runs the committed bytes; `crates/areev-conformance/tests/sandbox_abi.rs` runs
them through the sandbox binary.

## Rebuilding changes the addresses

A different rustc emits different bytes. `build.sh --check` therefore verifies
the **committed** blobs (their addresses, import sets and page ceilings) rather
than demanding byte-identical output from every toolchain — a gate that failed
honestly-unchanged trees would teach people to ignore it. A rebuild is a
deliberate commit that moves the blob, `dist/blessed.json` and
`docs/blessed-tools.md` together, and re-points every pack that pinned the old
address.

## Two example packs carry copies

`examples/grain-connector/pack/blobs/mailbox.poll.wasm` and
`examples/blessed-tools/pack/blobs/http.call.wasm` are copies of `dist/`,
because a pack is self-contained — it has to be installable from the directory
alone. Both examples assert their copy's address against `dist/blessed.json`
and fail loudly when it has fallen behind, so a rebuild is a three-file commit
(the blob, the manifest, the copies) rather than a silent divergence in which
the example keeps running last month's bytes.

## Testing

The blobs are tested where the engine is: `areev-sandbox/tests/blessed_tools.rs`
runs the committed bytes under real wasmi against a loopback broker stand-in.
Keyless — no credential, no network, no `areev` binary.
