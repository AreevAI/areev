# insurance documents → a coverage picture that knows *when*

A policy servicing desk. Endorsements, corrections, claim notices and
cancellations arrive; the agent extracts the coverage facts and keeps the
policy's picture current. But the question that decides money is never "what
does the file say now" — it is:

> **What cover was in force on the date of loss, and when did we come to
> know it?**

Those are two questions on two different clocks, and this example is built
around the fact that Areev answers both:

| clock | what it answers | on the grain | read with |
|---|---|---|---|
| **world** | what was actually *true* at time T | `valid_from` / `valid_to` | `entity_at(..., axis="world")` |
| **knowledge** | what this desk *knew* at time T | `system_valid_from` (= the grain's `created_at`), walked back down the supersession chain | `entity_at(..., axis="knowledge")` |

A claim is assessed on the **world** clock. A coverage dispute, or a
regulator asking what you told the insured in June, is answered on the
**knowledge** clock. An endorsement that is **backdated** — effective earlier
than it was recorded — makes the two disagree, and that disagreement is the
whole reason this example exists.

```
 broker / claims                the areev servicing desk                underwriter
────────────►  inbound  ──────►  extract ─┬─ change ──► book on both clocks
  endorsement  documents            │     │            (world: valid_from/to
  correction                        │     │             knowledge: created_at)
  claim notice                      │     ├─ claim ──► assess_cover ─► accumulation
  cancellation                      │     │                 │              │
                                    │     └─ referral ─► refer_back        │
                    no effective    │        (only once a person            │
                    date, no rule   ▼         has signed the rule)          │
                                  FAILED,                        settled ◄──┤
                                  loudly                         wording?   │
                                                          yes │        no │
                                                              ▼           ▼
                                              issue_determination ◄─ [ a person ]
                                              (under the underwriter's    signs it,
                                               earlier signature)         in writing
```

And the second loop, the one that makes it *self-improving* — the desk's own
runs are grains, so improvement is analysis over its own record:

```
 runs, determinations, rulings ──► areev loop (deterministic analyzers)
        (the journal)                    │ Recommendation + evidence, by hash
                                         ▼
                                   [ a person ] ── approve one, REJECT another,
                                         │          both with written reasons
   next week's intake ◄── a signed standing rule ◄──┘
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures, so CI proves it on every release.

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

Or everything at once (what CI runs): [`../run-smokes.sh`](../run-smokes.sh).

A few seconds later:

```
OK -- 1 determination signed at the limit in force, 1 refused for
     self-approval, 1 refused for having no reason, 2 clocks that disagree.
OK -- 1 ruling applied under its underwriter's signature, 1 finding
     approved, 1 finding rejected, 1 standing rule signed, 0 nags.
```

## Week one — `smoke.sh`

Five documents arrive for Harbourline Freight Ltd, insured by the (entirely
fictional) Thornbeck Mutual:

| Document | What it is | What the run does |
|---|---|---|
| `END-2201` | endorsement raising POL-4471 from 500,000 to 750,000, **effective 1 May, received 15 June** | **Books a new world window.** The old window is closed at 1 May and stays live and readable — so a loss before that date still finds 500,000. |
| `CORR-118` | correction: the deductible was mis-keyed at inception; it was always 10,000, not 5,000 | **Supersedes** the grain over the same world window. Retroactive in the world, dated in the knowledge. |
| `CLM-8801` | claim, 612,000, **date of loss 18 March** — before the endorsement's effective date | **Parks** on an underwriter, carrying the 500,000 that was in force on 18 March, not the 750,000 the file holds today. |
| `END-9002` | broker-portal endorsement with **no effective date** | **Fails, loudly.** A document with no effective date belongs on neither clock, and guessing "today" would silently rewrite when cover attached. |
| `CAN-7740` | cancellation of POL-6103, effective 1 July | **Closes the window.** A June loss still finds 400,000; a July loss finds nothing. Nothing was deleted. |

### The centrepiece

```
POL-4471  mg:coverage_limit
  date of loss 18 Mar   world 500,000   knowledge 500,000
  20 May                world 750,000   knowledge -- NOTHING KNOWN
  today                 world 750,000   knowledge 750,000
                        head (plain recall / latest): 750,000
```

Three assertions, all in `smoke.sh` step 3:

1. **A claim is assessed against the cover in force.** `entity_at(...,
   axis="world")` at the date of loss returns **500,000** while `latest()`
   and any plain `RECALL` return **750,000**. The claim is 612,000, so the
   insured is 112,000 short — where using today's limit would have told them
   they were fully covered.
2. **The knowledge clock differs from the world clock for a backdated fact.**
   On 20 May the higher limit was *already in force* and this desk *had never
   heard of it* — the endorsement arrived on 15 June. The world axis says
   750,000; the knowledge axis says nothing at all. One instant, two
   answers, both correct.
3. **A retroactive correction runs the other way.** On 18 March the
   deductible was *truly* 10,000 (the correction reached back to inception)
   and *believed* to be 5,000. "What the policy said in March" and "what we
   told the insured in March" are different questions, and the file answers
   both.

The desk keeps neither clock by convention: `valid_from`/`valid_to` and
`created_at` are set deliberately on every coverage grain from the
document's *effective date* and *received date* respectively, and the store
copies `created_at` into `system_valid_from`.

### The accumulation walk

`related()` is a bounded k-hop walk over the entity graph. From one policy:

```
out  -> Harbourline Freight Ltd, Harbourline Group
in   -> (nothing)
both -> Harbourline Freight Ltd, POL-5520, POL-6103,
        Harbourline Group, Marlowe Cold Chain Ltd, POL-7714
```

The aggregate exposure is then a **world-axis read per policy**, which is why
a cancelled policy leaves the aggregate on its cancellation date without
anyone deleting anything: 1,150,000 on 18 March, 1,000,000 on 5 July.

### What actually worked, honestly

We checked every direction empirically rather than trusting the docs, and
two of the results are worth stating plainly:

- **`out` works on any relation.** It reads the forward (SPO) index.
- **`in` and `both` only see relations the FILE declares entity-valued.**
  They read the reverse (OSP) index, which is selective by design. The
  store's default vocabulary is
  `mg:delegates_to, mg:owned_by, mg:assigned_to, mg:depends_on,
  mg:handed_off_to, mg:capable_of, delegates_to, reports_to, part_of,
  knows` — so this example's graph is deliberately built on **`mg:owned_by`**
  (policy → insured) and **`part_of`** (insured → group), which are in it.
- **`mg:covers_peril` is deliberately *not* in that set**, and the smoke
  asserts the consequence: `related("flood", "mg:covers_peril",
  direction="in")` returns `[]` even though the forward facts exist and
  `direction="out"` finds `fire, flood, theft`. **`reindex_links()` does not
  change this** — we tried; it rebuilds the indexes for declared relations,
  it does not widen the declaration.
- **The Python binding cannot widen the set.** `areev.Areev(...)` has no
  `entity_relations` parameter, so a binding-embedded agent gets the default
  vocabulary and nothing else. If you need `in` over your own relation, name
  it with one of the `mg:` entity relations, or open the file from a surface
  that can declare it.
- **`in` from a policy is empty and that is correct**, not a bug: a policy
  number appears in the *subject* position of `mg:owned_by`, never the
  object. The useful reverse walk is from the **insured** — that is what
  finds the sibling policies.

### The three gates

- **The desk cannot sign its own determination.** `run_respond` structurally
  refuses the principal that started the run (`RUN-E012`), and the desk is
  the principal that started every run here.
- **A determination with no written reason is refused.** This document may
  reach the insured; an unreasoned one is not a determination, it is an
  outcome.
- **The determination names the grain it relied on.** `agent.py trace
  CLM-8801` reads the as-of figures back out of the *run journal* — pinned
  in before the run started — so the determination is reproducible even
  though the file has moved on since.

## Weeks two and three — `improve.sh`

**The memory pays off first, before any loop is involved.** A second claim
arrives on the same clause (`clause:7B`, "are goods on the quayside in
transit?") against a different policy. Nadia settled that reading in week
one, so this one issues *without asking anyone* — and the determination
records `authority: settled-wording clause:7B`, `determined_by: user:nadia`.
The ruling carried forward; the signature did not become the desk's.

Then the broker portal sends three more endorsements with no effective date,
and three more runs fail. The loop reads the journals and proposes two
things — and a person answers them in **opposite directions**:

| Finding | Decision |
|---|---|
| `loop.run_outcome/1` — *"Workflow … failed 4/9 recent runs (44%): extract: … carries no effective date"* | **Approved**, with a reason: the portal's export template has no effective-date field. |
| `loop.staleness/1` — *"Expire POL-4471: past its declared valid_to"* | **Rejected**, with a reason: in a bi-temporal memory a closed coverage window is not stale, it is *the record* — it is the only grain that can answer a loss dated inside it. |

That second one is the point of putting a person in this loop at all. The
retention analyzer is right about every ordinary memory and wrong about this
one, and nothing in the evidence tells it so.

The fix is a **signed standing rule** (`desk-rule broker-portal refer_back
--because … --as user:tomas`, refused without a reason, and it names the
finding it came from). In week three the same broken document is **referred
back** with a reason the broker can act on — a completed run instead of a
failed one — while a complete endorsement from the same broker books
normally and lands on the world clock where it belongs.

`loop.cold_grains/1` is switched **off** in `improve()`, for a reason
specific to this shape: the desk reads its coverage picture through
`entity_at`, which is an as-of read rather than a recall, so every coverage
grain looks "never recalled" and the analyzer proposes retiring the file's
entire history. `loop.staleness/1` stays on deliberately, because a person
saying no to it is part of the lesson.

## What each fixture exercises

| Fixture | Exercises |
|---|---|
| `fixtures/policies.json` | the schedule as issued, on both clocks (`inception` → `valid_from`, `booked_on` → `created_at`), the entity graph, the accumulation limit |
| `inbound/01-endorsement-limit-raise.json` | **backdated variation** — world window opens 1 May, knowledge starts 15 June |
| `inbound/02-correction-deductible.json` | **restatement** — `"restates": true` → supersession over the same world window; knowledge diverges, world does not |
| `inbound/03-claim-cargo.json` | the world-axis as-of read at the date of loss; the client gate; the accumulation flag |
| `inbound/04-endorsement-no-effective-date.json` | the structural refusal — no effective date, no clock, no guess |
| `inbound/05-cancellation.json` | closing a window without deleting anything; a cancelled policy leaving the aggregate |
| `inbound/06-claim-quayside-again.json` | the settled-wording payoff, under the original underwriter's signature |
| `inbound/07…09-endorsement-no-effective-date.json` | the failure cluster the loop finds |
| `inbound/10-endorsement-no-effective-date.json` | the same document *after* the signed rule — referred back, not failed |
| `inbound/11-endorsement-broker-complete.json` | the same source sending a good document — still books, on the right date |
| `determinations/00-desk-self-signs.json` | separation of duties (`RUN-E012`) |
| `determinations/01-no-reason.json` | a determination with no written reason |
| `determinations/02-cover-confirmed.json` | the signed determination, and the clause reading it settles |

Every carrier, insured, group, policy number, clause and wording in here is
invented. The coverage model is deliberately thin — a limit, a deductible, a
peril list — because the temporal story is the point, not insurance depth.

## What this example is teaching

1. **"Current" is not an answer to a dated question.** Every agent that
   holds facts about a world that changes needs the world clock, and almost
   none of them have one. A supersession chain alone gives you *what we
   believed, when*; it does not give you *what was true, when*.
2. **Backdating is normal, not exotic.** Endorsements, corrections,
   retroactive coverage, late-notified claims — the record almost always
   arrives after the fact it records. That is exactly where a single-clock
   memory starts producing confidently wrong answers.
3. **Closing a window beats deleting a fact.** Cancellation, expiry and
   supersession are all *edits* in most systems. Here they are new grains,
   and the old one stays answerable at the dates it applied to.
4. **The as-of read belongs to the driver, not the tool.** A `--tool-cmd`
   subprocess must never open the memory the runtime is holding, so this
   desk resolves every memory-shaped question *before* the run starts and
   pins the answer into the run's input. The payoff is not just correctness:
   it means the journal records the picture each determination was made
   against, so it is reproducible after the file has moved on.
5. **A precedent can carry an underwriter's signature without becoming the
   agent's.** The auto-issued determination names the person whose ruling it
   applied, and the ask it skipped is the one that person already answered.
6. **Retention analysis and bi-temporal memory pull in opposite directions**,
   and only a person is positioned to arbitrate. That is a governance
   requirement, not a UX preference.

## Going live

Three seams, and none of them is Areev:

| Leg | Keyless here | Live |
|---|---|---|
| **Inbound** | `fixtures/inbound/*.json`, read by the driver, `DOC_UPTO` as the clock | a `polling` Trigger + a connector over your document intake (see [`docs/triggers.md`](../../../docs/triggers.md), [`../docs/email-providers.md`](../docs/email-providers.md)) |
| **Work** | `agent.py tools` — one process per effect, JSON on stdio | the same contract against your policy admin and claims platforms ([`docs/run.md`](../../../docs/run.md)) |
| **Underwriter** | `determine FILE`, a fixture standing in for the claims platform | `areev run respond --as <principal>`, or the console's Runs tab; per-principal credentials (`areev ui --auth`) so the approver's identity *is* the audit record |

The document *extraction* is deliberately a plain parse here. In production
that is where a model goes — `--llm-cmd` (see [`../../llm/`](../../llm/)) —
and nothing else in this example changes: the two clocks, the gate, the
journal and the loop are all downstream of whatever produced
`effective_from`.

Deployment, scheduling and the embedded-vs-Postgres decision:
[`../docs/deploy.md`](../docs/deploy.md).
