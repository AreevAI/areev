---
name: areev-bindings
description: Playbook for the Python (areev-py, PyO3) and Node (areev-js, napi-rs) bindings — the "scalars in, JSON strings out" FFI convention, keeping the two in lockstep with the core API, and the per-language build/test (maturin/pytest, napi/node --test). Use whenever you add or change a binding method, or need to build/test either binding. Critical gotcha: areev-js is a STANDALONE package, not a cargo workspace member, so `cargo test --workspace` never touches it and a Rust change can silently break it.
---

# Python + Node bindings

Both wrap the same core: `#[pyclass] Areev` (`areev-py/src/lib.rs`) and
`#[napi]` methods (`areev-js/src/lib.rs`), each over a `AreevFacade`. They are
**thin** — the logic lives in the memory stack; a binding only marshals types.

## The FFI convention (do not deviate)

- **Scalars in, JSON strings out.** Arguments are primitives (str, int, float,
  bool, `Option`); anything structured is returned as a **JSON string** the
  caller parses. Don't invent per-language struct returns — it breaks the
  cross-language symmetry and the docs.
- **Errors** → the language's exception: Python maps every core error via
  `err()` → `PyValueError` (`lib.rs:44`); Node returns a napi `Error`. A core
  `AreevError` must never escape as a panic.
- **Parity is the contract.** A method exists in one binding iff it exists in
  the other, with the same name shape and the same JSON payload. Changing one
  without the other is the #1 binding bug.

## Adding or changing a method — the fan-out

1. **`areev-py/src/lib.rs`** — add the `fn` inside the `#[pymethods] impl
   Areev` block; scalars in, `PyResult<String>` (JSON) out; map errors with
   `err(...)`. Reuse the `parse_hash`/`parse_duration_ms`/`status_from_str`
   helpers.
2. **`areev-js/src/lib.rs`** — add the matching `#[napi]` method, same
   signature shape, same JSON return.
3. **Both smoke tests** — extend `areev-py/tests/test_areev.py` (pytest) and
   `areev-js/__test__/smoke.mjs` (`node --test`). These are the only gate that
   proves parity, since neither is in `cargo test --workspace`.
4. If this is a brand-new user-facing operation (not just a binding of an
   existing one), it also fans out across store/CAL/MCP/CLI — see
   [[areev-add-operation]].

## Build & test

**Python** (`crate-type = ["cdylib"]`, `pyo3` `abi3-py39` → one wheel for
py39+):
```bash
cd crates/areev-py
maturin develop            # build + install into the current venv
python -m pytest tests/    # or: pytest test_areev.py
```
`build.rs` adds macOS `-undefined dynamic_lookup` so a bare `cargo build` links
without a Python lib present (extension-module resolves symbols at load time).

**Node** (napi-rs, native addon — **not** wasm):
```bash
cd crates/areev-js
npx napi build --platform --release   # areev.<platform>.node + index.js/.d.ts
node --test __test__/smoke.mjs
```

> **`--platform` is not optional.** Without it napi writes `areev.node`, but
> `index.js` only ever `require`s the platform-suffixed
> `./areev.<triple>.node`. So a build that omits the flag leaves the OLD addon
> in place, `node --test` runs green against **stale native code**, and your
> Rust change appears to have no effect — or a test asserts a pre-fix error
> string and passes. `package.json`'s `build` script and CI both pass
> `--platform`; match them, never call bare `napi build --release` by hand.

## Node has no deterministic drop — the facade slot and `close()`

Rust and Python release a handle when the last reference goes; **Node releases
it whenever GC runs**. That matters because the store refuses a second handle
on a file this process already has open (`STO-E002`, single-writer-per-file):
without an explicit release, an ordinary open → use → reopen fails against a
handle the caller had already finished with and had no way to let go of.

So `areev-js` holds the facade in a *closable slot* rather than directly:

```rust
type FacadeSlot = Arc<Mutex<Option<Arc<AreevFacade>>>>;

fn take_facade(slot: &FacadeSlot) -> napi::Result<Arc<AreevFacade>> { … }
```

Every method clones the **slot**, not the facade, and fetches inside the
spawned job:

```rust
pub fn some_method(&self) -> AsyncTask<StringJob> {
    let slot = self.facade.clone();          // NOT self.facade.clone() -> facade
    StringJob::spawn(move || {
        let facade = take_facade(&slot)?;    // errors if the handle is closed
        …
    })
}
```

**A new napi method written the old way (`let facade = self.facade.clone();`)
compiles fine and silently keeps working after `close()`** — it captured the
facade instead of the slot. Route every method through `take_facade`. Python
needs none of this (refcounting drops the handle), which is why the two
bindings legitimately differ here.

Tests that reopen a file must call `m.close()` first; a JS block scope does
not release it.

## The standalone-JS gotcha (read this)

`areev-js` is deliberately **outside the cargo workspace** (its `Cargo.toml`
declares its own `[workspace]`; napi cdylibs resolve Node-API symbols at load
time and need their own build). Consequences:

- `cargo test --workspace` and `cargo build --workspace` **skip it entirely** —
  a change to `areev-core`/`areev-store`/`areev-cal` can break the JS binding
  and the workspace suite stays green. CI's separate **`node` job** is what
  catches it (`napi build --platform --release` + `node --test`); run that locally after
  any core API change that the JS binding surfaces.
- The Python binding **is** a workspace member (`areev-py`, `publish = false`),
  so `cargo build -p areev-py` compiles with the workspace, but its Python
  behavior is only proven by pytest, not `cargo test`.

## Naming & publish

Python module is `areev` (PyPI `areev`); npm package `areev` with a
`areev-<platform>` native subpackage per target. `areev-py`/`areev-bench`
stay `publish = false` in cargo. Version is inherited from
`[workspace.package]`; releasing is [[areev-release]].
