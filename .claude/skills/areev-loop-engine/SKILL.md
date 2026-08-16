---
name: areev-loop-engine
description: Playbook for the Areev Loop self-improvement engine (crates/areev-loop + areev-loop-adapter adapter) — adding or changing an analyzer, the four gates, the recommendation lifecycle/state machine, the DISCOVER→GROUND→VERIFY LLM verifier, and outcome measurement. Use before touching crates/areev-loop/src/{analyzer,engine,recommendation,policy,llm}.rs, anything under crates/areev-loop/src/analyzers/, or the areev-loop-adapter substrate adapter — the engine is substrate-agnostic and gate-governed, so a lone analyzer that skips the metric/gate wiring silently half-works.
---

# The Areev Loop engine

Areev Loop is a **substrate-agnostic**, **deterministic-core** self-improvement
engine: it reads typed grains (never raw prose), proposes governed
recommendations, and measures their outcomes. The `areev-loop` crate has **no
Areev dependency** — it speaks the `OmsSubstrate`/`SubstrateRead` traits
(`substrate.rs`) and `LlmBackend` (`llm.rs`); `areev-loop-adapter` is the adapter
that implements `OmsSubstrate` over a `AreevFacade` (+ the recall-telemetry
sidecar). Design source of truth: `docs/loop.md` (the four gates + the
analyzer table) and `docs/loop-reflection.md` (the LLM verifier).

Keep the core crate Areev-free — if you reach for a `areev-*` type inside
`areev-loop/`, it belongs in `areev-loop-adapter` instead.

## The four gates (don't weaken them)

1. **Propose** — only `RecDraft`s enter the queue: a versioned analyzer id +
   params, a template-rendered summary (no free prose), bounded evidence
   hashes, a severity, and a reproducible metric snapshot where applicable.
2. **Review** — separation of duties (`write` grants neither `review` nor
   `apply`), a mandatory reason (BECAUSE) on every decision, self-approval
   blocked against the creating actor.
3. **Apply** — needs the `apply` scope; destructive applies additionally need
   `admin` + `allow_destructive`; every apply records its inverse.
4. **Verify** — `outcome_review` re-runs the stored metric after `review_after`
   and proposes a **revert on regression**.

The audit trail **is grains**: one immutable Observation per transition,
hash-chained per recommendation. Auto-apply is granted **only** through
`Engine::with_policy` and **only** for the `StructuralCuration` auto-apply
class — a lossy/destructive op (merge, FORGET) never auto-applies.

## Adding or changing an analyzer — the fan-out

Eleven built-ins live in `crates/areev-loop/src/analyzers/` (`tool_failure`,
`duplicate_sweep`, `contradiction_sweep`, `fork_surfacing`, `staleness`,
`skill_stall`, `goal_stagnation`, `cold_grains`, `coverage_gap`,
`budget_pressure`, `outcome_review`). Copy the closest one and touch, in order:

1. **`analyzers/<name>.rs`** — the struct + `impl Analyzer`:
   - `manifest(&self) -> &AnalyzerManifest` — id, family, `default_on`,
     default severity, and the **`AutoApplyClass`** (`StructuralCuration` iff a
     provably information-preserving structural edit; otherwise never
     auto-appliable). This is a governance decision, not a detail.
   - `analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>>` — compute over
     **typed grains** read through `ctx` (`SubstrateRead`), emit `RecDraft`s.
     `dedup_key`/`origin`/params snapshot are stamped by the engine afterward —
     don't set them. Returning `Err` drops only this analyzer's findings.
     Guard every denominator (rate/ratio/Jaccard) — degenerate input
     (empty telemetry, one data point) must not divide by zero or emit NaN.
2. **`analyzers/mod.rs`** — add the `pub mod <name>;`.
3. **`analyzer.rs` `builtin_analyzers()`** — add the `Box::new(...)` row **iff
   default-registered**. The count and unique-ids are **test-pinned** (`§11`,
   `builtins_have_unique_ids`) — update those. (A host-registered analyzer uses
   `Engine::register`; an external process uses `CommandAnalyzer`,
   `external.rs`.)
4. **If the recommendation is measurable** — wire its metric into
   `engine.rs::measure_metric` so **Verify** can re-run it. Scope the metric to
   the *exact* thing the recommendation claims to fix (a `MetricSnapshot` that
   only stores `subject` will re-measure too broadly — this is the class of the
   `tool_failure`-recurrence bug: an unrelated later failure of the same tool
   read as a regression and reverted a valid lesson). Baseline vs. current must
   compare like for like.
5. **Policy/gate** — confirm the `default_on` and `AutoApplyClass` choices
   against `policy.rs`; a default (closed) policy grants nothing.
6. **Tests** — end-to-end in `integration.rs` using `testkit.rs::TestSubstrate`
   (`add_fact`/`add_tool_call`/`add_fork`/`add_skill`/`add_goal` to seed,
   `set_outcome_inputs`/`telemetry_*` for the telemetry-fed analyzers, then
   `sub.analyze(&MyAnalyzer::new(), now_ms)` or `Engine::with_builtins().run(...)`).
   Pin the manifest id/family and cover the degenerate-input path.

## The recommendation lifecycle

`RecStatus` (`recommendation.rs`): `Pending → Approved | Rejected`,
`Pending → Applied` **only** `by_policy` (auto-apply), `Approved → Applied`,
`Applied → RolledBack`, `Pending|Approved → Expired`. All transitions go
through `can_transition_to(to, by_policy)` — never mutate status directly, and
never widen the matrix (e.g. `Rejected → Applied` must stay illegal). Every
transition writes a hash-chained audit Observation.

## The LLM verifier (optional, enrichment-only)

`with_llm(backend)` adds **cited** draft recommendations (`origin = llm`,
**never** auto-applied) and whitelisted guidance — it can never gate or rewrite
deterministic output. The pipeline is DISCOVER → GROUND → VERIFY → ROUTE
(pre-queue filter): the proposer and scorer are separate *calls* (and GROUND
can use a separate `with_ground_llm` backend); VERIFY checks **soundness, not
novelty**; abstention is honored (empty → early return); a confidence floor
(≥0.75 on the verifier's clamped confidence) applies. Keep proposer ≠ scorer and
the abstention/floor intact.

## The gate — before you commit

```bash
cargo test -p areev-loop              # core engine, analyzers, lifecycle, verifier
cargo test -p areev-loop-adapter       # the substrate adapter over AreevFacade
cargo test -p areev --test golden_loop_tests   # exact CLI-surface output
```

Regenerate the loop golden dataset only on an intended change:
`cargo test -p areev --test golden_loop_tests -- --ignored bless` then
`GOLDEN_BLESS=1 cargo test -p areev --test golden_loop_tests` (the clock is
pinned by `AREEV_LOOP_NOW_MS`; see [[areev-testing]]). Then run the
[[areev-invariants]] gate. New user-facing surface (CLI verb, `/api/loop/*`,
MCP) also follows [[areev-add-operation]].
