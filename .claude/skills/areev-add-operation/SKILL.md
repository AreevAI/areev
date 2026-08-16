---
name: areev-add-operation
description: End-to-end playbook for adding or changing a user-facing Areev operation across every public surface — store method, CAL, MCP tool, CLI verb, Python, and Node bindings — with the test that gates each. Use whenever a new capability (or a changed signature) needs to be reachable by users, not just internal to one crate.
---

# Adding a Areev operation across all surfaces

A user-facing capability fans out across up to **six surfaces**. The failure
mode this skill prevents: adding it in one place (usually the store) and
forgetting the bindings, so Python/JS/MCP silently lag behind. Decide the reach
first, then touch each surface in dependency order.

## Decide the reach (do this before editing)

- **Core primitive?** New store behavior lives in `areev-store` `Areev`
  (`crates/areev-store/src/lib.rs`). Read the store CLAUDE.md first — the
  single-writer, immutable-blob, and fail-open invariants constrain what a new
  method may do.
- **Reachable from CAL?** Only if a query should trigger it. That is a
  **separate, heavier task** — use the `areev-cal-feature` skill, and remember
  CAL *syntax* is an OMS conformance contract.
- **Public binding?** If agents/users call it directly, it needs MCP + CLI +
  Python + JS. If it is internal plumbing, stop after the store (+ CAL).

## Surface map — touch in dependency order

1. **`areev-store/src/lib.rs`** — the `Areev` method. This is the source of
   truth; every surface below is a thin adapter over it. Gate:
   `crates/areev-store/tests/store_tests.rs` (copy the nearest existing test;
   fork/merge tests use *fixed* `created_at` for deterministic tiebreaks).
2. **`areev-mcp/src/lib.rs`** — TWO edits: add the arm to the `call_tool`
   match (~`lib.rs:114`) **and** the schema entry in `tool_defs()` (~`lib.rs:236`).
   Convention: tool *failures* are `isError: true` **results**, only protocol
   faults are JSON-RPC errors; notifications (no id) get no response. Gate:
   `crates/areev-cli/tests/mcp_smoke.rs` (drives the real binary over real
   stdio — there are no in-crate MCP tests).
3. **`areev-cli/src/main.rs`** — add an arm to `match cmd.as_str()`
   (~`main.rs:232`); flags come from `parse_args` (~`main.rs:116`) as a
   `HashMap`, no clap. Gate: `crates/areev-cli/tests/cli_smoke.rs`.
4. **`areev-py/src/lib.rs`** — a `#[pymethods]` fn with a `#[pyo3(signature=…)]`.
   FFI convention: **scalars in, JSON strings out**; errors → `PyValueError`.
   Gate: `crates/areev-py/tests/test_areev.py` (CI runs `maturin develop`
   then pytest).
5. **`areev-js/src/lib.rs`** — a `#[napi]` method (native Node addon, **not**
   wasm). Same scalars-in / JSON-out shape; `err()`/`parse_hash()` helpers
   already exist. Gate: `crates/areev-js/__test__/smoke.mjs` (`node --test`).
   NOTE: `areev-js` is a *standalone* napi package, **not** a workspace member —
   `cargo test --workspace` does not build it; CI's `node` job does.

6. **Docs — same commit, not a follow-up.** Sweep the Docs contract table
   in the root `CLAUDE.md`: CLI help + `docs/cookbook.md` for a new verb;
   `docs/mcp-reference.md` (incl. the pinned tool count) for an MCP tool;
   `docs/cal-reference.md` for payload/behavior changes (its ```sql fences
   are executable — CI runs them); `ERROR_CODES.md` for codes;
   `docs/security-model.md` for anything touching auth/crypto/keys; the
   subsystem's own reference doc (`docs/run.md`, `docs/loop.md`, …); and an
   `ARCHITECTURE.md` §10 named decision when the operation introduces one.
   An operation that reached all six code surfaces but zero docs is not
   done.

## Verify

```bash
cargo test --workspace          # store + mcp_smoke + cli_smoke + py-less Rust
cargo clippy --workspace --all-targets -- -D warnings
```

- Python/JS are **not** in `cargo test --workspace`. If you touched them, run
  their gates the way CI does (maturin develop + pytest; napi build + node --test).
- Keep the six surfaces in parity: a reviewer should see the same operation
  named and shaped consistently across MCP tool, CLI verb, `py`, and `js`.
- Before committing, run the **areev-invariants** gate.
