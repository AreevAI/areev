# areev-loop

The governed self-improvement engine for AI-agent memory — the reference
implementation of the Areev Loop layer described in
[`docs/loop-proposal.md`](../../docs/loop-proposal.md).

Areev Loop turns an agent's own history into **recommendations** — evidence-cited,
reviewable, undoable, measured — and governs every change through four gates
(propose → review → apply → verify). The deterministic core produces useful
recommendations with **zero model calls** by computing over declared grain
semantics, never raw prose.

## What's here (build-order item 1)

A **standalone engine over an `OmsSubstrate`** (CAL text + grains) with **zero
Areev dependencies** — serde only. Areev is the first substrate; the in-repo
`ReferenceSubstrate` lets tests run with no store at all and doubles as the
conformance kit for third-party substrates.

- `OmsSubstrate` / `SubstrateRead` — the store protocol (read split out so
  analyzers get a read-only view, enforced by the type system).
- The recommendation model (OMS 0x0C): `RecDraft` → engine-stamped
  `Recommendation`, deterministic `Summary` templates, `dedup_key`
  (family-excluding-major ⟂ target ⟂ action), the lifecycle state machine, and
  hash-chained `AuditRecord`s.
- `Engine`: the analyze → validate/dedup → store pipeline with the
  run-outcome contract (`RunResult`: outcome / skip-reason / counts), plus
  `review` / `apply` / `rollback` with scopes, the mandatory BECAUSE, the
  self-approval block, and destructive gating.
- The six default analyzers: tool-failure clustering, duplicate sweep,
  contradiction sweep, fork surfacing, staleness, outcome review.
- `LOP` error domain (see the repo's `ERROR_CODES.md`).

## Status

Workspace member during the churn phase; lifted to its own repo when semantics
freeze (proposal §10). **Not published** from this workspace (`publish = false`).

Not yet in this crate: the Areev substrate adapter, CLI/MCP/bindings surfaces,
the LLM enrichment layer, auto-apply execution (the gate is present but
conservative — nothing auto-applies), and the console. Those are later
build-order items.

```rust
use areev_loop::{Engine, ReferenceSubstrate, RunOptions};

let mut store = ReferenceSubstrate::new();
let engine = Engine::with_builtins();
let result = engine.run(&mut store, &RunOptions::default(), 1_000).unwrap();
assert!(result.ran());
```

## Test

```bash
cargo test -p areev-loop
cargo clippy -p areev-loop --all-targets -- -D warnings
```

Licensed under MIT OR Apache-2.0.
