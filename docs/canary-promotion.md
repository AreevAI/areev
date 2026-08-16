# Canary promotion for governed code — design (Wave 4 deliverable, build later)

Status: DESIGN. §7.4 marks staged promotion RECOMMENDED, not required, for
v1 — required controls (no auto-apply, pinned evalset + recorded gating
run, outcome metric + revert wiring) shipped with the code pipeline. This
document is the design of record for the staged rollout that comes next.

## The problem

Apply today is binary: an approved, evalset-gated code revision becomes
the tool's resolution for every subsequent run at once. The evalset is a
necessary gate but a bounded one — production traffic finds what curated
cases don't. Between "gated" and "everywhere" there should be a stage
where the new code serves a bounded slice, measured, with the old code as
the default.

## Design

### The `candidate` head

A tool name resolves through the Definition catalogue (newest Definition
wins — `find_definition_by_name`). Promotion adds one file-truth: a
`tool:<name> mg:candidate <code-hash>` Fact in `agent:harness`, written by
Apply *instead of* superseding the default resolution.

- **Default resolution is unchanged** while a candidate exists — resumes
  and forks pin what their manifest froze, as always.
- **New runs opt in probabilistically**: `RunOptions.canary_fraction`
  (host config, default from the applied recommendation's `canary` block)
  routes that fraction of NEW manifest resolutions to the candidate hash.
  The choice is frozen into the manifest like every resolution — a canary
  run is a canary for its whole life, visibly (`pinned[].canary = true`).
- **Determinism is untouched**: randomness happens at manifest-resolve
  time in the DRIVER (host side, journaled outcome), never inside the
  scheduler; replay reads the frozen pin.

### Promotion and demotion

- The candidate's outcome metric is the SAME MetricSnapshot machinery the
  loop already runs — measured over canary runs only (join on the
  manifest's canary flag via `runs_touching(candidate_hash)`).
- After `M` clean runs (no failure-rate regression at the configured
  horizons), the loop proposes PROMOTION: an ordinary recommendation
  (never auto-applied — same §7.4 rule) whose apply supersedes the default
  Definition and retires the `mg:candidate` Fact.
- A regression at any horizon proposes DEMOTION with the standard revert
  blast-radius report; demotion tombstones the `mg:candidate` Fact — the
  default was never changed, so there is nothing else to undo. This is
  the reason candidate-first beats supersede-first: the unhappy path is
  one tombstone, not a revert of the default resolution under traffic.

### Governance invariants carried over

- Candidate placement, promotion, and demotion are each their own
  recommendation with the full audit chain (approver, BECAUSE, gating
  pin at placement).
- `areev tool provenance <hash>` grows a `canary` section: placement rec,
  fraction, clean-run count, promotion/demotion rec.
- Rule E1 applies to the placement rec exactly as to a direct apply (the
  candidate was still gated by an evalset before it saw ANY traffic).

## Non-goals

- Per-request (sub-run) routing — the unit of canary is the run, because
  the run is the unit of replay and audit.
- Traffic mirroring/shadowing live requests — the §8 shadow evaluator
  already replays journaled runs against candidates with zero dispatches;
  canary is for the effects shadow cannot exercise.
