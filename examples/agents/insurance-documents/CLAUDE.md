# insurance-documents — working rules

The example whose point is **when the memory is read**. Everything below
protects that point.

1. **Two clocks, and they are set by hand.** `valid_from`/`valid_to` come
   from the document's *effective date* (world); `created_at` comes from its
   *received date* (knowledge — the store copies `created_at` into
   `system_valid_from`). Neither is inferred and neither defaults to "now".
   If you find yourself letting a coverage grain take the wall clock for
   either field, stop: you have just erased the only thing this example
   demonstrates.
2. **World and knowledge need OPPOSITE grain shapes, and this is the trap.**
   Verified empirically, not read off a doc:
   - the **world** axis (`entity_at(..., axis="world")`) selects among
     *live* grains (`system_valid_to IS NULL`) by `valid_from <= T <
     valid_to`. So two coverage windows must **coexist** as separate live
     grains — superseding one to make the other current hides it from the
     world axis forever.
   - the **knowledge** axis walks the **supersession chain** back from the
     head comparing `system_valid_from <= T`. So it can only see values that
     are actually *linked* by supersession. Two coexisting windows are two
     chain roots, and the knowledge axis returns `{"found": false}` for the
     dates before the head was recorded — which is the honest answer for a
     backdated document ("we had no record of it") and is asserted as such.

   Hence: **a variation opens a new window (add), a restatement corrects a
   belief (supersede)**. `apply_change()` is the only place that decides
   which, on the document's `"restates"` flag. Do not collapse the two.
3. **Closing a window is a supersession of the grain by itself.** When an
   endorsement varies cover from an effective date, the open window is
   superseded by a *closed restatement of the same value* keeping its
   ORIGINAL `created_at` (the desk is not changing its mind about the old
   value, only saying where it stopped applying), and then a new grain opens
   at the effective date. Both are live; the world axis picks between them.
4. **`related`'s `in`/`both` only see relations the FILE declares
   entity-valued** — the reverse (OSP) index is selective by design. This
   example's graph rides on `mg:owned_by` and `part_of` because they are in
   the store's default vocabulary; `mg:covers_peril` deliberately is not, and
   `smoke.sh` asserts that `direction="in"` finds nothing for it. Two things
   that do **not** help and were tried: `reindex_links()` (rebuilds indexes
   for declared relations, does not widen the declaration) and the Python
   constructor (it has no `entity_relations` parameter at all). If you need a
   reverse walk over a new relation here, rename it onto the `mg:` entity
   vocabulary.
5. **`related` returns entity *terms*, not grains.** The aggregate is
   computed afterwards with a world-axis `entity_at` per policy — which is
   what makes a cancelled policy drop out of the aggregate on its
   cancellation date. Do not "optimize" that into a single recall; the
   temporal filter is the feature.
6. **The as-of reads happen in the DRIVER, before `run_start`.** A
   `--tool-cmd` subprocess must never open the memory the runtime is holding,
   so `run_input()` resolves world/knowledge/head/deductible and the exposure
   walk and pins them into the run's input. That is also why `trace` can
   prove what each determination was made against. Never add a memory read
   to `tool_main()`.
7. **A host tool exiting non-zero does NOT raise from `run_start`.** The
   session comes back `{"finished": "Failed { node: …, detail: … }"}`. Only
   a run that never started raises. `intake()` reads `finished` and
   `startswith("Completed")`; getting this wrong once made every refusal look
   like a success (it did, during authoring).
8. **`route` is emitted by `extract` and is mutually exclusive by
   construction** (`claim` | `change` | `referral`), so no edge in the plan
   depends on evaluation order. Keep it that way — the alternative is
   relying on first-match semantics that are not part of the frozen cond
   grammar.
9. **The refer-back route does not exist until a person signs it.** With no
   standing rule, an undated document *fails* — loudly, and that is correct.
   `smoke.sh` asserts `referrals.jsonl` does not exist in week one. The rule
   is the improvement, so it cannot be present before the loop finds the
   reason for it.
10. **`loop.cold_grains/1` is disabled in `improve()`; `loop.staleness/1` is
    NOT.** Cold-grains is pure noise here (as-of reads are not recalls, so
    every coverage grain looks cold). Staleness is left on **on purpose**:
    its proposal to expire a closed coverage window is wrong in a bi-temporal
    memory, and a person rejecting it with a written reason is one of the two
    decisions this example exists to show. Do not "clean up" that noise.
11. **`created_at` is pinned at `1756000000000`** on the workflow, tool and
    skill grains so the plan hash is stable. Change a seeded field and the
    hash moves; `out/workflow.hash` is what `../run-smokes.sh` compares.
    Coverage grains are the exception — their `created_at` IS the knowledge
    clock and comes from the fixture.
12. **`TODAY` is pinned at `2026-08-01`** so every as-of read in the acts is
    deterministic. Fixture dates run 2026-01 → 2026-07, all in the past, so
    nothing here goes stale with the wall clock — except the `loop.staleness`
    day count, which the acts deliberately do not assert on.
13. **Fixtures are synthetic and the dates are load-bearing.** `END-2201`'s
    `received_at` is *after* its `effective_from`; `CORR-118` carries
    `"restates": true`; `CLM-8801`'s date of loss is *before* `END-2201`'s
    effective date and its amount (612,000) sits *between* the old limit and
    the new one. Change any one of those and the centrepiece stops being a
    demonstration.
14. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table, and
    `.github/workflows/ci.yml`. Shared how-to material lives in `../docs/`.
