---
name: areev-testing
description: How to test Areev — the test taxonomy (unit / integration / golden / property / fuzz / perf-gate / cross-surface), the determinism rules that keep the suite non-flaky, the golden bless flow, and a combination-coverage checklist for finding bugs at feature interactions. Use when writing or changing tests, adding coverage for a new feature, chasing a flaky or failing test, or hunting for bugs by exercising edge cases and feature combinations.
---

# Testing Areev

Areev has ~950 tests across 8 layers. They are fast (the whole workspace runs
in well under a minute) and deterministic by construction. This skill is the
map: what each layer is for, where it lives, the rules that keep it green, and
how to add coverage — especially at the **feature combinations** where bugs
hide.

## The gate — run before every commit

```bash
cargo test --workspace                                   # full suite, must be green
cargo clippy --workspace --all-targets -- -D warnings    # zero warnings
```

Per-crate while iterating: `cargo test -p areev-store` (or `-cal`, `-core`, …).
Single test: `cargo test -p areev-store fork_merge -- --nocapture`.
- **Never run blanket `cargo fmt`** — the tree is intentionally not
  rustfmt-clean (~177 files differ). Format only the lines you touch.
- Touched `areev-core/src/format/`? Also run the conformance suite:
  `cargo test -p areev-core --test oms_conformance` (a manifest-hash diff there
  means you changed the frozen wire format — see [[areev-invariants]]).
- Touched a store hot path? Run the perf gates (below) — a regression there is a
  real failure, not noise.
- `spawn areev: No such file or directory` after a repo move/rename: the cached
  test binary has a stale `CARGO_BIN_EXE_areev` path. Fix:
  `touch crates/areev-cli/tests/*.rs` and re-run.

## The 8 layers — where they live, what they gate

| Layer | Lives in | Gates |
|---|---|---|
| **Unit** (`#[cfg(test)]`) | inline in `src/*.rs` (~67 modules) | pure logic close to the code |
| **Integration** | `crates/*/tests/*.rs` (real `Areev`, `TempDir`) | store/CAL/context behavior end-to-end |
| **Golden dataset** | `areev-cli/tests/golden*` + `golden/dataset/` | exact bytes: hashes, recall, CAL, renders, CLI↔MCP parity |
| **Property** | `areev-core/tests/proptest_roundtrip.rs` | the frozen serializer over arbitrary grains |
| **Fuzz** | `fuzz/fuzz_targets/*` (nightly, out of workspace) | panics/OOM on untrusted input |
| **Perf gate** | `areev-store/examples/{bench,voice_loop}.rs` | recall p50 <200µs, latest <100µs, 50ms frame cadence |
| **Cross-surface** | `mcp_smoke.rs`, `multichannel_tests.rs`, golden parity | CLI = MCP = library for the same op |
| **Smoke** | `cli_smoke.rs`, `http_smoke.rs`, `migrate_smoke.rs` | the binary actually runs |

Pick the **lowest** layer that can catch the bug. A serializer invariant → a
property test. A store method's edge case → an integration test. A user-facing
op that must match across CLI/MCP/py/js → golden + cross-surface (and see
[[areev-add-operation]]).

## Determinism rules — the #1 source of flakiness here

The suite is deterministic on purpose. Break one of these and you get a test
that passes locally and fails in CI (or vice versa):

1. **Isolate every file in a `TempDir`.** One memory = one file with an
   exclusive single-writer lock; two tests sharing a path collide under
   parallel `cargo test`. Golden tests each import their **own** copy of the
   bundle for exactly this reason (`import_golden()` in `golden/mod.rs`).
2. **Never depend on the wall clock.** Pin `created_at` to a fixed epoch when
   the value is observed or when it decides an ordering. Fork/head election is
   `max (created_at, hash)` — `fork_merge_tests.rs` sets explicit `created_at`
   so the tiebreak is testable; copy that pattern.
3. **Pin the loop's clock** with the `AREEV_LOOP_NOW_MS` env seam (epoch ms). The
   golden helpers `areev_at(now_ms, …)` set it and scrub ambient `AREEV_LOOP_POLICY`
   / `AREEV_DB` so a dev's environment can't leak into output.
4. **Property-test strings must be NFC-stable.** `proptest_roundtrip.rs` draws
   from a curated non-combining code-point pool so the serializer's `nfc()` pass
   is a no-op and values compare equal exactly after round-trip. If you widen
   the pool to real combining marks, the equality assertions must account for
   normalization (this is itself a good bug-hunting axis — see below).
5. **No ordering assumptions from HashMaps.** Assert on sorted/keyed output, not
   iteration order.

## Golden dataset + the bless flow

Golden tests import a committed, deterministic dataset and assert the *exact*
output. When you **intentionally** change the dataset or a render, re-bless:

```bash
# CLI goldens (dataset → bundle + manifest, then renders):
cargo test -p areev --test golden_tests -- --ignored bless
GOLDEN_BLESS=1 cargo test -p areev --test golden_tests render
# Areev Loop goldens:
cargo test -p areev --test golden_loop_tests -- --ignored bless
GOLDEN_BLESS=1 cargo test -p areev --test golden_loop_tests
```

Then **review the diff** and commit it. An *unintended* diff in
`manifest.json` content hashes means canonical serialization changed — that is a
frozen-format break, not a test to re-bless. Stop and confirm it is intended
([[areev-invariants]], [[areev-grain-type]]).

## Property tests & fuzz

- Property (`proptest`): the serializer round-trip must be **byte-identical**
  and **hash-identical**, and every set field must survive. When you add a grain
  type or field ([[areev-grain-type]]), add its `proptest!` arm.
- Fuzz (nightly, in `fuzz/` — detached from the workspace so stable CI never
  builds it): `cargo +nightly fuzz run deserialize_blob` (also `cal_parse`,
  `tool_schema_parse`). Run these when you touch a parser or the deserializer;
  they guard the untrusted-input surfaces.

## Perf gates

```bash
cargo run --release -p areev-store --example bench       # recall p50<200µs, latest<100µs
cargo run --release -p areev-store --example voice_loop  # holds a 50ms frame cadence
```

`voice_loop` spin-waits (never sleeps) and runs `index_text: false` (FTS costs
~150ms/write). Treat a gate regression as a real failure.

## Hunting bugs by combination — the coverage checklist

Most surviving bugs live at **feature interactions**, not single features. When
adding coverage or bug-hunting, walk these axes and ask "does the existing suite
test *this pair*?" Each row is a place bugs have hidden or plausibly could:

- **supersede × forget** — forget the *new* head (does an old version resurrect?);
  forget an already-superseded *old* grain (chain integrity?); `count()` /
  `recall()` / `heads()` agreement after each.
- **fork × forget × merge** — two tips, forget one, then `merge_heads`; merge
  with 1 tip (must error) or 0; provisional-head determinism after import.
- **add_if_novel × supersession** — novelty is by current-head `(ns,s,r,object)`
  value, not by full content hash; re-asserting a superseded old value counts as
  novel. Confirm that is intended in your test.
- **encryption × blobs** — the `.blobs` CAS sidecar is **not** encrypted
  (documented; `open_warnings()` says so). A test that writes a secret via
  `put_blob` to an encrypted store and greps the sidecar proves the *current*
  boundary; convert it to a leak test if/when blob encryption lands.
- **bundle import × idempotency** — import the same bundle twice (no dup rows,
  op-log intact); `import_bundle_until` at exactly `max_hlc` (inclusive?);
  bundles carrying supersede+forget replay in op order with tombstones intact.
- **CAL destructive gating × every entry path** — `FORGET` with
  `allow_destructive_ops=false` must refuse; `DELETE/ERASE/TRUNCATE/PURGE` must
  stay lexer-blocked non-tokens through *every* surface (raw CAL, `DROP`, a saved
  query body, ASSEMBLE, MCP `areev_cal`). `mcp_smoke.rs` asserts `DELETE …`
  fails — extend that instinct.
- **parser × unconsumed input** — a valid statement followed by trailing
  garbage must **error**, not silently drop the tail (regression fixed in
  commit 4edcc3a). Test trailing tokens, a second statement, stray punctuation.
- **rendering × budget** — budget 0 / 1 / smaller-than-one-grain across SML /
  TOON / Markdown / JSON: never exceed budget, never emit malformed output, never
  cut mid-UTF-8. Same input across all four formats should include the same
  grains.
- **hybrid recall × missing capability** — `recall_hybrid` with no embedder must
  fail *open* to structural+FTS, not error; tuning weights at extremes (0, >1,
  negative); a query-embedding dim ≠ stored dim.
- **meta reconciliation** — `open()` vs `open_with()`: honoring vs re-stamping
  `text_index` / `entity_relations` / embedding provenance, and the
  `open_warnings()` each path emits (`meta_tests.rs`).
- **unicode / NFC** — decomposed vs precomposed input through add → serialize →
  content-address → recall; FTS tokenization for CJK; budget accounting that
  must count display width / code points, not bytes. NFC must apply to map
  **keys**, not just values, or composition-variant grains get distinct hashes.
- **serialize ⇒ deserialize symmetry** — anything that serializes MUST read back
  (invariant 2). Probe the hostile floats and integers: `confidence = NaN/±Inf`
  (rejected on read → a written-but-unreadable grain), a `u64 > i64::MAX` in an
  `extra_field` (lossy float coercion). And the reverse — the reader must reject
  **non-canonical** blobs (trailing bytes, non-minimal encodings) or the same
  logical grain gets unlimited content addresses (dedup bypass). Nested
  user-JSON keys that collide with OMS short-codes (`s`/`o`/`r`/`c`/`desc`/…) in
  `State.context`, a Tool `input_schema`, or an Event tool-use `input` must
  survive a round-trip unrewritten.
- **op-log completeness** — every state change must appear in `changes_since` /
  `bundle_since`, or replicas diverge. The trap is changes made *outside* the
  `add`/`forget` paths: `merge_heads` tip closures and an **imported**
  supersession (whose paired `OP_ADD` already inserted the grain) can flip the
  index without writing an `OP_SUPERSEDE` row. Test replication at **two hops**
  (A→B→C, re-exporting from the replica), not just one — single-hop hides it.
- **public-API robustness to malformed input** — a published-crate method must
  return `Err` (or fail open), never panic. Byte-slicing a caller-supplied hex
  (`get_blob("cas://sha256:")`) or feeding a raw user string to the FTS
  query-grammar (`recall_hybrid(query=":"/`"`/`(`)`) are the live examples.

When one of these pairs has no test, that gap is either a latent bug or a
missing test — write the test that decides which. Prefer a failing test that
pins the *correct* behavior; if behavior is genuinely undecided, raise it rather
than freezing an accident into a golden file.

## Two failure shapes this suite kept missing

Both surfaced only when someone black-box-tested a published wheel against the
docs (issues #47–#55), which means the suite could not see them.

### 1. A filter that is ignored — assert what it EXCLUDES

```rust
// Passes even if the filter is dropped entirely. Useless.
assert!(subjects(q).contains("jane"));

// Fails if the filter is dropped. This is the test.
assert_eq!(subjects(q), vec!["jane"]);          // john and bob must be gone
assert!(subjects(empty_binding_q).is_empty());  // and empty selects NOTHING
```

Anything that narrows a result set — `WHERE`, `IN`, `WITH dedup`,
`conflict_resolution` — needs a negative assertion. Failing to apply such a
clause fails **open**, and the over-broad answer goes into a model's context
with no error.

Also assert byte-inequality for options that are supposed to change output:
`assert_ne!(m.cal(q), m.cal(q_with_option))`. Several `WITH` options shipped
returning output byte-identical to the query without them.

### 2. Documentation that describes a different engine

Prose is untested by default. `crates/areev-cal/tests/docs_examples.rs`
extracts every ```sql fence from `docs/cal-reference.md` and parses it, and
cross-checks the §4 stage table against the parser's own error message. Three
separate bug reports were one root cause: the reference documented shapes the
grammar does not have.

When you pin a doc claim, verify the test fails against the *old* doc — check
out the previous version under the new test. The stage-table check initially
false-passed because the parser's error ends with `…, found WHERE`, so a
substring search over the whole message matched the very stage it had just
rejected; it has to compare against the parenthesized accept-list only.

The same applies to a hand-maintained list mirroring code: `DESCRIBE`'s
`required_fields` is pinned to the `require_str` calls it mirrors by
`required_fields_match_the_validator`, which both directions — every declared
requirement is enforced, and nothing beyond the declared set is.

## Adding a test — quick recipes

- **Store behavior**: new `#[test]` in the matching `areev-store/tests/*.rs`;
  `TempDir` + `Areev::open` + the local `fact()` helper; fixed `created_at` if
  ordering matters.
- **CAL end-to-end**: `cal_integration.rs` (or `recall_tuning_cal_tests.rs`) —
  drive a query string through the facade and assert the JSON payload.
- **A new user-facing op**: golden + cross-surface parity is the real gate —
  follow [[areev-add-operation]] (store method → CAL → MCP → CLI → py → js, each
  with its test).
- **A serializer/grain change**: proptest arm + `oms_conformance` +
  re-bless goldens — follow [[areev-grain-type]].
- **Always** run the full gate (`cargo test --workspace` + clippy) before
  committing.
