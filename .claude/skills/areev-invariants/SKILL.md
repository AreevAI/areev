---
name: areev-invariants
description: Load-bearing invariants and the pre-change quality gate for the Areev codebase. Use BEFORE editing any Rust source in this repo, and ALWAYS before committing — especially when touching the .mg format, canonical serialization, content addressing, CAL, error codes, or the network/crypto surfaces.
---

# Areev invariants & quality gate

Areev is an embedded memory engine. A handful of invariants are load-bearing —
violating them silently corrupts data or breaks spec conformance. Check them
before you change code and before you commit.

## The invariants (do not break without a design discussion)

1. **Grains are immutable and content-addressed** — the hash is SHA-256 over the
   whole `.mg` blob. Never mutate a stored blob. Every edit is a *supersession*,
   every removal a *tombstone* (`forget`) or crypto-erasure. Only the index layer
   is mutated. **Corollary: re-adding a byte-identical grain is a no-op, not an
   error** — the address *is* the content, so it is already stored; `add`
   returns the existing hash, consumes no seq/op/hlc, and writes no op-log row.
   (Until 1.1.0 it raised `UNIQUE constraint failed: grains.hash`, which forced
   callers to jitter payloads — corrupting the record to satisfy the store.)
2. **Canonical serialization is frozen** — NFC normalization, sorted keys, compact
   keys, omit-defaults. Any byte change alters every content address and breaks
   OMS conformance. If you touch `areev-core/src/format/{serialize,deserialize,
   header,field_map}.rs`, assume you are changing the wire format and STOP to
   confirm it is intended. The serialize path enforces the same size/depth limits
   as the deserializer, so a grain that can be written can always be read.
3. **CAL destruction is authorization-gated, not structural** (CAL 1.3) — the
   destructive statements are `FORGET <hash>` (single-grain tombstone),
   `FORGET SUBJECT "<id>" [WITH text_mentions]` (identity erasure), and
   `PURGE OLDER THAN <n><d|h|m> [TYPE t]` (retention sweep). BECAUSE is
   mandatory on the latter two; every execution writes a Tier-2 audit
   Observation in `agent:authz`. Grants live in the file as `mg:permits`
   Facts; the session's `AuthzSet` decides, and
   `CalExecutorConfig::allow_destructive_ops` stays a process-wide restrictive
   **cap** over any grant (`--no-destructive-ops`). Destruction takes a hash,
   an identity, or an age — never a predicate: `DELETE`/`ERASE`/`TRUNCATE`/…
   stay lexer-blocked non-tokens, `FORGET USER`/`SCOPE` stay text-refused,
   `DROP` accepts only TEMPLATE/QUERY; saved-query bodies stay read-only.
   Statement classification has ONE source of truth
   (`areev_cal::classify`, exhaustive, no wildcard). Widening the
   destructive surface, or any new CAL syntax, is a spec-level
   (OMS-conformance) decision.
4. **Error codes are append-only** — every user-facing error carries a stable
   `DOMAIN-Ennn` code (see `ERROR_CODES.md`). Never renumber or reuse one; add new
   codes at the end. Format/uniqueness are test-enforced.
5. **One memory = one file, and one open handle** — single writer per file.
   Enforced in both directions: across processes by an OS file lock, and
   *within* a process by a registry of open paths, so a second `open()` fails
   at open with **`STO-E002`**. It has to be enforced rather than documented:
   each handle caches its own `next_seq`/`next_term` allocators and BM25 stats,
   so two handles drift and then collide on a write — surfacing as a bare
   `UNIQUE constraint failed: terms.id` against *the handle that did nothing
   wrong*. Sharing one handle across threads is fully supported. Rust and
   Python release the claim on drop; **Node must call `close()`** (no
   deterministic drop — see the `areev-bindings` skill). The Postgres backend
   is exempt: it admits multiple concurrent writers per memory by design.
   Cross-file queries go through `ASSEMBLE` with facade mounts, not shared
   connections. Host config (embedder, executor limits, encryption key) is
   per-process and never persisted in the file.
6. **Dependency-light by policy** — no clap, no HTTP framework, no MCP SDK, no
   workspace-wide async runtime. Justify any new dependency in the PR.

## Before you commit — run the gate

```bash
cargo test --workspace                              # full suite must be green
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
```

For the full test taxonomy (unit / integration / golden+bless / property /
fuzz / perf-gate / cross-surface), the determinism rules that keep the suite
non-flaky, and a **combination-coverage checklist** for finding bugs at feature
interactions, see [[areev-testing]] — and add a regression test for every bug
you fix.

- **Docs sweep (the Docs contract).** If the change touches a public
  surface, its canonical doc moves in the same commit — the mapping table
  lives in the root `CLAUDE.md` ("Docs contract"). Minimum check: CLI help /
  cookbook, `mcp-reference.md` (pinned tool count), `cal-reference.md`
  (executable examples), `ERROR_CODES.md`, `security-model.md` for
  auth/crypto/keys, the subsystem's reference doc, and an `ARCHITECTURE.md`
  §10 named decision for anything architecture-level.
- **Do NOT run blanket `cargo fmt`** — the tree is not uniformly rustfmt-clean by
  design. Format only the lines you touch; match the surrounding style.
- If you changed anything under `areev-core/src/format/`, also confirm the OMS
  conformance suite passes: `cargo test -p areev-core --test oms_conformance`.
- Fuzz the untrusted-input surfaces if you touched them:
  `cargo +nightly fuzz run deserialize_blob` (also `cal_parse`, `tool_schema_parse`).
- Security-sensitive change (server, crypto, deserialize, CLI bind)? Re-read
  `docs/security-model.md` and keep it accurate.

## Adding an error code

Every user-facing error carries a stable `DOMAIN-Ennn` code as the **leading
token of its `Display` string** — a permanent debugging handle. Codes are
**append-only**: never renumber, reuse, or repurpose one.

- Domains: `FMT` `MEM` `STO` `CRY` `VAL` `CAL` `SYS`. New subsystem with no
  fitting domain → add a 3-letter mnemonic domain to `ERROR_CODES.md` first.
- Pick the next free number in the right domain range. Put `DOMAIN-Ennn: ` at
  the front of the variant's `#[error]`/Display string; add the arm to the
  type's `code()` (except `areev-cal`, where the code lives *inside* the
  `#[error]` string, no separate fn). Add a row to `ERROR_CODES.md`.
- Source of truth: `AreevError`/`SchemaSubsetError` (`areev-core/src/error.rs`)
  and `CalError` (`areev-cal/src/errors.rs`).
- Keep the tests green and extend their representative-variant lists:
  `areev-core`'s `error_code_tests`; `areev-cal`'s `test_error_codes_match_display`
  and `test_all_error_codes_have_unique_codes`.

## Touching store hot paths — run the perf gates

If you changed recall, the dictionary/index layer, or anything on the add/recall
path in `areev-store`, confirm you did not regress the latency budgets:

```bash
cargo run --release -p areev-store --example bench       # recall p50 < 200µs, latest < 100µs
cargo run --release -p areev-store --example voice_loop  # holds a 50ms frame cadence
```

`voice_loop` spin-waits rather than sleeps and runs `index_text: false` (FTS
costs ~150ms/write). If a gate regresses, treat it as a real failure, not noise.

## Where things live

`areev-core` (format/grains) ← `areev-store` (Turso store) ← `areev-cal`
(query language) ← `areev-context` (rendering); `areev-mcp`, `areev-server`,
`areev`, `areev-py` sit on top. See `AGENTS.md` and `ARCHITECTURE.md`.
