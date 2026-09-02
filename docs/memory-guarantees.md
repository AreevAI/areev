# Memory guarantees: what is withheld, what is measured, what is not built

Five questions about what a memory promises — which grains may be acted on,
whether a deleted value can return, how abstention is measured, how long a write
takes to become readable, and whether reading changes belief.

This file records what was decided about each, **including the ones deliberately
not built**, so the reasoning is on the record rather than re-litigated. Where
the answer is "no", it is a decision and not an oversight.

| # | Finding | Verdict |
|---|---|---|
| 1 | Retracted grains rank lower but still surface | **Fixed** — shared classifier + withholding at assembly, DSAR pinned |
| 2 | A forgotten value can be written back | **Real gap, already known.** Design needed before code |
| 3 | The eval does not separate INVENTED from MISS | **Built** — `honesty_metrics` M5 reports both directions |
| 4 | Write-to-readable lag is unmeasured | **Measured and fixed** — `honesty_metrics` M6 asserts 6 of 6 legs; the replica text leg was broken |
| 5 | Recall never changes belief | **Documented + pinned** on both backends |

---

## 1. Retraction — the defect is disagreement, not demotion

**The question:** `verification_status = "retracted"` applies a `-0.3` priority
penalty (`crates/areev-context/src/render.rs:238`) and the grain still reaches
recall. *"A retraction that ranks rather than withholds."*

**What we found.** Four read paths, four different meanings for one value:

| Path | Treatment |
|---|---|
| `areev-store` recall | **ignored** — writes the field, never filters |
| `areev-cal` render | displays as metadata, no filter |
| `areev-context/render.rs:238` | `-0.3` priority penalty |
| `areev-cli/corpus.rs:213` | weight **`0.0`**, class `"rejected"` — excluded |

**The counter-evidence does not defend the current behaviour.**
`docs/loop-proposal.md:938` records that a `verification_status=retracted`
marker was *rejected* as the rollback inverse **because** demotion is not a real
undo. The adapter confirms it in code — `areev-loop-adapter/src/substrate.rs:513`
maps `retract()` to `forget()`:

> `// No index-only retraction primitive exists; the honest mapping for undoing`
> `// an engine-created ADD is a tombstone of that grain.`

So nothing in the loop depends on retracted grains being visible. The one
decision path cited as load-bearing deliberately routes around this code.

**Decided: it is a defect.** Argued from the field's own semantics:

1. `grain.rs:189` — the field *"replaces deprecated `contradicted` boolean."*
   A boolean is a filter; replacing it with a richer type should not silently
   convert it into a rank.
2. Three of four values (`unverified`/`verified`/`contested`) describe evidence.
   `retracted` describes an **act** — a withdrawal. Not the same axis.
3. `adjusted_priority` re-merges what the schema separates: the `-0.3` is added
   to a `confidence`-derived float in one sum. We hold the state apart from the
   score in the schema and undo that separation in the scorer.
4. Four disagreeing readers violate the single-source-of-truth principle the
   codebase applies everywhere else (`areev_cal::classify`, one renderer, one
   conformance suite over both backends).

**Fix, in two steps — deliberately separable.**

- **Step 1 (do first).** One exhaustive classifier, no wildcard —
  `areev_core::verification::Trust` — consumed by `render.rs` and `corpus.rs`.
  This removes the actual defect, changes no behaviour, and lands with a
  conformance case.
- **Step 2 (decide separately).** `retracted` withheld at the context-assembly
  boundary with explicit opt-in to include. `contested` **keeps** `-0.15`: that
  one genuinely is a degree.

**What breaks.** Nothing in `.mg` — read-path only. Rollback is unaffected
(uses `forget`). CAL queries returning retracted grains would change under
step 2: that needs a CHANGELOG entry and probably `WITH include_retracted`.

**The risk that gates step 2.** `subject_report` (DSAR) and erasure
[share one selector](gdpr.md) so that a disclosure covers exactly what an erasure
removes. If exclusion leaks into the DSAR path we **under-disclose** — an
Art. 15 problem created while fixing an epistemics problem. Step 2 must scope
exclusion to context assembly and explicitly include retracted grains in DSAR.

---

## 2. Rejected-value tombstone — real gap, deliberately not implemented

> **Status: not built, on purpose.** The gap is real and we intend to close it.
> It is blocked on one question that is a **compliance decision, not an
> engineering one**, and guessing it wrong is expensive:
>
> **May a rejection ledger keep a hash of content we just erased for someone?**
>
> `docs/gdpr.md` states that a content address is a *pseudonymous identifier,
> not anonymized data*. Refusing a value later means remembering something about
> it — so "never believe this again" may or may not survive "this must not
> exist". Rejection and erasure are genuinely different operations, but that is
> a judgement the maintainer (with a compliance reviewer) has to make, not one
> to infer from the code.
>
> Everything else below is ready to build the moment that is answered.


**The question:** `forget` removes the record thoroughly, but the content hash is
not consulted on the **write** path, so a background pass that re-reads an old
transcript can re-assert the same wrong fact as a new row — memory laundering:
the row is gone, the belief comes back.

**What we found.** We already know. `crates/areev-bench/src/bin/honesty_metrics.rs`
models the exact failure (mem0 #4573, a hallucinated fact stored 808 times) and
scopes our protection honestly:

> *"Scope: identical content incl. timestamp — NOT a paraphrase deduper;
> near-duplicate phrasings need the write-time novelty gate, roadmap SP-1."*

**`SP-1` is referenced there and defined nowhere in the repo.** That is the first
thing to fix — a roadmap pointer with no roadmap entry.

**Decided: the gap is real and worth closing, but it needs a design first.**
Areev's own loop re-derives memory automatically, which is precisely the
condition the mechanism exists for, so "not our use case" is not available to us.

Open questions the design must answer, none of which should be guessed:

- **Coverage.** Seven write paths (`add`, `add_batch`, `add_with_embedding`,
  `add_if_novel`, `supersede`, `import_bundle`, `import_bundle_until`). A check
  that misses `import_bundle` is bypassed by replication and is decorative.
- **Normalization.** One shared normalizer for rendering and rejection, or
  look-alike characters evade the key.
- **Erasure interaction.** Retaining a hash of erased content retains a
  derivative of it, and `docs/gdpr.md:213` says content addressing is not
  anonymization. Rejection and erasure are different operations, but
  `forget_subject` and the retention sweep both need a stated answer.
- **Bounding.** The ledger cannot grow without limit.

**What we already have to build on:** `add_if_novel` is a write-path check on
`(ns, subject, relation, object)` — the right key shape, on the right path, for
one of seven entry points. It compares against live heads, not a rejection
ledger. Recommendation cooldowns keyed on `dedup_key`
(`loop-proposal.md:931`) are the same idea one level up.

---

## 3. Measuring abstention — the narrow version is ours

**The question:** nothing distinguishes *"answered when it should have refused"*
(INVENTED) from *"failed to answer something it held"* (MISS), and the two move
in opposite directions.

**Decided: the general version is the host's problem; a narrow version is ours.**

Areev is a substrate, not an answerer. Whether to abstain is the host's call, and
a harness that scores abstention would be scoring a policy we do not own.

But there is a substrate-level analogue with the same shape:

- **surfaced-what-should-be-withheld** — a grain the trust state says must not be
  acted on, returned anyway. This is exactly finding #1.
- **withheld-what-was-held** — a grain present and not returned.

Those are ours, they move in opposite directions, and today nothing reports
either. **Build this after #1**, because it is the regression test that stops
#1 recurring, and it is meaningless before the trust classification is single-
sourced.

---

## 4. Write-to-readable lag — measured, and it found a real gap

**The question:** no benchmark in the field measures the window between a write and
that write being recallable. Ours should be ~0 by construction — no daemon,
synchronous capture — but we had never measured it.

**Measured.** Four legs, with a deterministic embedder installed:

| Leg | Lag |
|---|---|
| local, structural recall | **0** — same transaction |
| local, text (BM25) | **0** |
| local, vector | **0** — the add path embeds synchronously |
| replica, structural recall | **0** — grains arrive with the bundle |
| replica, vector | **0** — the import path embeds when the host has an embedder |
| **replica, text (BM25)** | **unbounded on the importing handle** |

**An earlier assumption here was wrong and is corrected.** We expected the
*vector* leg to lag on a replica, since bundles carry blobs rather than index
rows. It does not: the importing host embeds on the way in. The leg that lags is
**text**.

**What actually happens.** `insert_blob` — the bundle-import write path — does
not write text postings. A memory that has just imported answers `search_text`
with **nothing**. Reopening the memory fixes it, because `finish_open` carries a
self-heal (`index_text && indexed_text > 0 && fts_docs == 0` → `rebuild_text_index`)
written for files that predate the current BM25 leg; it happens to cover a fresh
replica too. Calling `rebuild_text_index` by hand fixes it as well.

**Why this matters.** The self-heal only runs **at open**. A long-running
follower — `areev stream` / `follow`, importing and serving from one handle —
answers every free-text query empty for as long as the process lives. The code's
own comment names this the worst available failure:

> *"the worst failure available, since it is indistinguishable from an honest
> empty result."*

That is the *"I told you that ten minutes ago"* window, and it exists in the
deployment shape we recommend for sync.

**Decided: fix it, but the shape is a real choice** — do not slap one in:

- **(a)** make `insert_blob` index text like the `add` paths do. Correct at the
  source; costs tokenization on every imported grain, which is the reason bulk
  import may have skipped it deliberately.
- **(b)** have `import_bundle` rebuild the text index once at the end when it
  detects postings missing. Cheap, matches the existing self-heal, and keeps
  bulk import fast.
- **(c)** document "reopen or reindex after import" as a requirement. Weakest —
  it leaves the failure mode live for anyone who does not read that line.

**(b)** looks right — a single rebuild per import rather than per grain, and it
puts the heal where the gap is created instead of relying on the next open. But
it needs a check against the bulk-import performance path (`defer_text_index`)
before it lands.

**Published** as `honesty_metrics` M6: six legs — structural, text and vector,
locally and on a replica after import — each asserted readable on the next call
with no reopen and no reindex. Because capture is synchronous the honest unit is
a boolean per leg, not a latency distribution that is zero by construction. The
replica text leg is the regression guard for the bug above: disabling the import
postings write turns M6 red at `text=false`, 5 of 6.

## 5. Recall never changes belief — holds, and by architecture

**The suspicion:** no `last_seen` / `access_count` / recall-driven
mutation appears to exist, so Areev may hold this property by construction and
document it nowhere.

**Verified: it holds.** `success_count` and `failure_count` exist as author-set
fields on `GrainCommon` and appear in **no** recall path — only in
`areev-cal/src/json_build.rs`, the build-from-JSON write path. Recall's only
write is `record_recall_event`, and that goes to the telemetry sidecar, which is:

- a **separate file**, host-only, *"never persisted in the main file — telemetry
  is host config, not a file-truth"*;
- **never synced** — *"bundles carry the memory file only"*;
- rebuildable — *"losing it costs evidence detail, never state"*;
- and consumed only by hygiene analyzers (cold grains, coverage gaps, budget
  pressure). It **does not feed ranking**.

That last point is not an accident and cannot casually be undone: ranking that
read the sidecar would make the same query return different results on different
hosts, breaking the guarantee in `docs/loop.md` that *"a synced file behaves
identically on its next host."* **Replication determinism is what enforces the
property**, which is a stronger guarantee than a convention.

**Decided: document it as an explicit guarantee and pin it with a conformance
test** that fails if a recall path ever writes to the memory file. Cheapest item
on this page, and systems get this wrong in both directions — some let recall
raise a confidence class, others never record usefulness at all.

---

## What these share

- **#1 and #3 are one item in two halves.** #1 is the behaviour; #3 is the
  number that proves it stays fixed. Do #1 step 1, then #3.
- **#1 and #2 are the same family** — *withholding*. One is a grain marked as
  not-to-be-believed; the other is a value that must not return after deletion.
  A single `Trust` concept should serve both; design #2 with #1's classifier in
  hand, not before.
- **#2 and #5 are the same principle from opposite sides.** #5 says the read path
  must never mutate belief. That is exactly what makes a write-path rejection
  ledger safe: the check belongs on write, and recall stays pure.
- **#4 stands alone** and is the cheapest to act on.

## Recommended order

1. **#1 step 1** — shared trust classifier. Real defect, small, no behaviour change.
2. **#5** — document the guarantee, add the conformance test. We already hold it.
3. **#4** — measure both legs, publish with the replica caveat.
4. **#3** — the two counters, as #1's regression test.
5. **#2** — define `SP-1` first, then design. Largest, and the erasure
   interaction must be settled before any code.
