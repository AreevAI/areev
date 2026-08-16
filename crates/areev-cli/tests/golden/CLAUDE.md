# Golden dataset tests

Deterministic integration tests: import a committed, known dataset and
validate the *exact* data Areev produces — content hashes, recall sets and
ordering, CAL payloads, render text, loop runs, cross-surface parity.
Two layers share the plumbing here:

- **Memory stack** (`golden_tests.rs` + `generator.rs` + `golden.bundle`) — a
  deliberately *clean* dataset (zero loop findings).
- **Areev Loop** (`golden_loop_tests.rs` + `loop_generator.rs` +
  `loop.bundle`) — a dataset in which every deterministic analyzer has a
  deliberately seeded target, driven through the real binary with the engine
  clock pinned.

## Why golden tests (vs the existing suites)

| Existing tests | Golden dataset tests |
|---|---|
| Create grains inline per test | Import one committed dataset with known hashes |
| "did I get results?" | "did I get the RIGHT grains, in the RIGHT order?" |
| One surface at a time | Same assertion across CLI and MCP |
| Ephemeral values | Frozen content addresses — the OMS conformance canary |

## Layout

```
tests/
├── golden_tests.rs         ← 33 memory-stack tests + 5 known-bug regressions + bless
├── golden_loop_tests.rs  ← 34 loop E2E tests + bless (suites W1–W15)
└── golden/
    ├── mod.rs              ← paths, `areev`/`areev_at` runners, imports, assert_golden
    ├── generator.rs        ← memory-stack dataset (pinned epoch 2026-01-15)
    ├── loop_generator.rs ← loop dataset (same epoch; seed table in its docs)
    └── dataset/
        ├── golden.bundle + manifest.json   ← memory stack (39 grains)
        ├── renders/                        ← recall/ASSEMBLE golden text
        ├── loop.bundle + loop-manifest.json  ← areev-loop (21 grains incl. a fork)
        └── loop/                         ← areev-loop output goldens (runs, queue,
                                              show payloads, outcomes, policy…)
```

## Running

```bash
cargo test -p areev --test golden_tests
cargo test -p areev --test golden_loop_tests
```

Each test imports its own copy of the bundle into a temp dir — Areev is
single-writer-per-file and every `areev` call is its own process, so tests
cannot share one memory file under parallel execution.

## The dataset (39 grains, base epoch 2026-01-15 UTC)

| Slice | ns | Purpose |
|---|---|---|
| 10 john facts | personal | entity recall, relation filters, renders |
| 8 bob facts | work | namespace isolation, cross-ns ASSEMBLE |
| 1 unicode fact (rené/café münchen) | personal | NFC canonicalization anchor |
| 10 events (2 sessions × 5) | shared | thread indexing, seeded BM25 tokens |
| 2 goals | work | non-triple grain type |
| kim status ×3 (supersession chain) | personal | HISTORY, head-only recall |
| dave/erin/fay drinks + acme industry | personal | WITH-option targets (dedup, graph hop) |
| 1 forgotten grain | work | tombstone survives export/import |

Every `created_at` is a fixed offset from the base epoch — no `now()`
anywhere — so every content hash is reproducible on any machine.

## Semantics the suite pins (learned, not assumed)

- Recall ordering is **insertion recency (op_seq desc)**, not created_at.
- A forgotten grain ships in the bundle as a zero-length blob and does
  **not** materialize as a row on import (38 rows from 39 generated).
- NFC: composed and decomposed spellings of the same text produce the
  **same** content address.
- Clause order matters: `LIMIT` before `WITH`, `BUDGET` before `FORMAT`.
- ASSEMBLE `FORMAT json` returns a `grains` payload, not rendered text.

## The loop layer

`AREEV_LOOP_NOW_MS` (read by `areev_loop_adapter::now_ms`, so CLI, MCP serve, and the
console all honor it) pins the engine clock; the substrate stamps
recommendation/audit grains from engine time (`created_at_ms`/`at_ms`), so a
loop run through the real binary is a **pure function of (file, policy,
now)** — recommendation content addresses included. That is what lets the
suite byte-pin `run`/`list`/`show`/`outcomes` output and step time across
outcome horizons and rejection cooldowns without sleeping. Garbled
`AREEV_LOOP_NOW_MS` fails loud (never silently falls back to wall time).

What the suites cover: the analyzer registry + default-closed policy pins;
first-run findings byte-exact (11 recs; run at `--telemetry off` so the
capability-skip ladder is pinned too); dedup idempotency, `reflect`, and the
`--min-new`/`--if-stale` gates; approve→apply→real-memory-effect→rollback→
honest re-proposal; the mandatory BECAUSE, self-approval block, and the
destructive (`FORGET`) gate; outcome measurement across 1d/7d horizons (held
and regressed→revert); rejection cooldown expiry; auto-apply under a granting
policy vs the trust floor under a maximal one; `--fail-on` exit codes; the
`recall-hook --with-loop` context block; scripted-fake LLM reflection
(DISCOVER→GROUND→VERIFY) and `--analyzer-cmd` external analyzers (both
python-gated, skip when absent); live telemetry-fed analyzers; CLI↔MCP hash
parity across separate imports; and `loop-manifest.json` regeneration as a
frozen-format canary over Tool/Skill/Observation/Goal/valid_to shapes.

Semantics the loop suite pins (learned, not assumed):

- **Import UNIONs heads**, so seeds written as repeated plain ADDs to one
  (subject, relation) — the duplicate and contradiction targets — are genuine
  multi-head forks in the *imported* file even though the source store showed
  one head. `fork_surfacing` firing on them is correct; the dataset therefore
  yields 3 fork findings (2 union + 1 engineered divergent supersession).
- **Applying a contradiction resolve creates an exact-duplicate pair** (the
  winning value + the replacement grain carrying the same value), which
  `duplicate_sweep` flags on the next run — see `run-after-regression.json`'s
  `stored: 2`.
- Hybrid recall is deadline-bounded fail-open, so `recall-hook`'s memory half
  is **not** byte-stable under load — only the loop block is pinned.
- The hook injection caps at the top 3 by severity; LLM drafts are always
  stamped `low`, so the `[llm]` badge is asserted in a minimal memory where
  the llm finding tops the queue.

## Known bugs found by combination probing (Suite 9, #[ignore]d)

Each has an ignored regression test asserting the *correct* behavior —
un-ignore it as part of the fix:

1. Pipeline stages (`COUNT`) and `LIMIT` written **after a WITH clause**
   are silently dropped (EXPLAIN confirms they vanish from the plan).
2. `BUDGET` written **after FORMAT** in ASSEMBLE is silently dropped
   (works in the documented BUDGET-then-FORMAT order).
3. `OR` across subject equalities silently returns only the first subject.

Fixed, with the ignored test promoted to a permanent guard:

- `WITH superseded` was a silent no-op on every anchored recall (all three legs
  were hard-wired to heads). Now `golden_with_superseded_surfaces_the_chain`
  pins the widened result *and* its `superseded_by` labels, and
  `golden_without_superseded_stays_heads_only` pins the default — the more
  load-bearing half, since it is what keeps stale values out of context.

## Changing a dataset

Memory stack:

1. Edit `generator.rs` (keep every timestamp pinned).
2. `cargo test -p areev --test golden_tests -- --ignored bless`
3. `GOLDEN_BLESS=1 cargo test -p areev --test golden_tests render`
4. Review and commit the diff in `dataset/` — the diff IS the review.

Areev Loop:

1. Edit `loop_generator.rs` (timestamps stay offsets from the base epoch).
2. `cargo test -p areev --test golden_loop_tests -- --ignored bless`
3. `rm -rf golden/dataset/loop/` (drops orphaned goldens), then
   `GOLDEN_BLESS=1 cargo test -p areev --test golden_loop_tests`
4. Review and commit the diff — expected-count asserts inside
   `golden_loop_tests.rs` (stored totals, cold counts) may need the same
   edit; they exist so a bless can't silently absorb a semantic change.

If `golden_manifest_hashes_stable` / `loop_manifest_hashes_stable` fails
**without** a dataset edit, canonical serialization changed — that is a
frozen-format / OMS conformance break (root CLAUDE.md invariant #2), not a
test to appease. A `loop/` golden diff without a dataset edit means
analyzer semantics, engine stamping, or a CLI surface changed — review it as
a behavior change, then bless deliberately.
