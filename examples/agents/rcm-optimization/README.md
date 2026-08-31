# RCM denial optimization

**The problem.** A payer remittance lands carrying a wall of denied claims
— and how many is not knowable when the plan is written. Hand-roll the
fan-out and you get a fold nobody can replay or audit; meanwhile the same
denial causes recur for months, because the fix lives in one biller's head
instead of anywhere the next remittance can read.

**What you get.** One node returns a list and the runtime does the rest: a
screening task per denial, joined and folded deterministically, every run
replayable byte-for-byte. And the desk learns where it counts: one signed
approval in week one, and next week **three of five denials classify
themselves** — fewer denials spending a person's attention, and a written
reason on every one that still does.

The desk itself: healthcare **revenue cycle**. The agent screens every
denial, clusters the causes per payer, and proposes a coding/submission fix
**with the denials as evidence**. A billing lead approves one and rejects
another, each with a written reason — and the approved one becomes a fact
that changes how the next remittance is handled.

The plan has **six nodes**. The first tick spawns **eleven** classification
tasks.

That gap is the whole point of this example: **the width of the work is a
property of the file, not of the plan**, so the plan cannot enumerate it.
One node returns a `$send` list, the runtime spawns a task per denial,
joins the batch before anything downstream fires, and folds the per-task
results through **declared reducers** — `append` for the rows, `sum` for
the money and the counters. No other example here spawns work at runtime.

```
 payer                     the areev RCM agent                       billing lead
──────►  remittance ──poll──►  split_denials ─┐
 835 /    feed       trigger        │         │  $send: one task per denial
 EOB      (fixtures)                │         │  (the plan never declared a count)
                                    ▼         ▼
                             classify_denial ×N   ── runs once per denial, own
                                    │                retry budget, own task path
                     ┌──────────────┴───── the batch JOINS here ──────────────┐
                     │  reducers:  classified=append   denied_cents=sum       │
                     │             auto_classified=sum unmapped=sum           │
                     └──────────────┬──────────────────────────────────────────┘
                                    ▼
                                 cluster ─── under the floor, or already mapped ──►  file_report
                                    │                                                    ▲
                              a pattern worth a person's time                            │
                                    ▼                                                    │
                             [ lead_review ] ──approve──► apply_fix ─► resubmission ─────┤
                              a person, by name              queue                       │
                                    └──────reject, with a reason───────────────────────► ┘
```

And the second loop — the one that makes it *self-improving*. Every
classification is already a grain, so improvement is analysis over the
desk's own record:

```
 20 classifications over 4 remittances ──► areev loop (deterministic analyzers)
        (the desk's tool record)                │  Recommendation + the denials, by hash
                                                ▼
                                          [ a person ] ──approve, signed──►  a lesson
                                                ▲                            on the tool
   next remittance's context ◄── the trigger's saved CAL query ◄─────────────┘
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
OK -- 11 tasks the plan did not enumerate, 2 runs verified, 1 fix approved, 1 rejected, 1 mapping learned.
OK -- 20 denials over 4 remittances, 2 mappings learned, 6 resubmissions queued, 1 loop finding signed.
```

## The fixtures are invented — all of them

**The denial codes in this example are synthetic.** `DN-101`, `DN-204`,
`DN-311`, `DN-402`, `DN-517`, `DN-622` and their texts were made up for
this directory. They are **not** CARC/RARC codes, no meaning is taken from
any real code set, and nothing here should be used to interpret a real
remittance. Both payers, every claim id, every CPT-shaped string and every
patient reference are fictional. There is no patient data of any kind — a
denial fixture carries an opaque `patient_ref` and nothing else.

[`fixtures/codebook.json`](fixtures/codebook.json) is the **desk's own
crosswalk**: the payer supplies the code and the text, and the last column
(`proposed_root_cause`) is the desk's guess at what it means. It only ever
*suggests*. Nothing acts on it until a billing lead has approved it into
memory — which is the distinction the whole example turns on.

## Week one — `smoke.sh`

| Step | What it proves |
|---|---|
| 1 | The plan stores its reducer table, and every value is a **string** naming a built-in |
| 2 | A reducer written as an **object** mints a content address and then refuses at **run start** with `RUN-E019` — a plan you can save is not a plan you can run |
| 3 | An absent cursor seeds and fires nothing; the next tick works both remittances |
| 4 | **The fan-out.** 6 + 5 denials → 11 tasks → `classified` is exactly as long as the file, in **spawn order**, and `denied_cents` equals the file's total |
| 5 | Both runs replay from their journals byte-for-byte — the fold is reproducible, not merely plausible |
| 6 | The desk cannot approve its own proposal (`RUN-E012`), and a verdict with no reason never reaches the run |
| 7 | dana approves the Meridian fix → 3 claims queued under her name; omar rejects the Cascade one → **nothing** queued, his reason on the report |
| 8 | Redelivering the same remittances starts nothing |
| 9 | The approved mapping is a fact; the rejected one left behind only the reason it was rejected |

## Week two — `improve.sh`

| Step | What it proves |
|---|---|
| 1–2 | **The memory payoff.** Meridian sends `DN-311` again; 3 of 5 denials classify themselves from the mapping dana approved, recalled through the trigger's `context_query`. Nobody is asked. |
| 3 | Cascade's `DN-517` comes back and **parks again** — a rejection is a reason, not a mapping |
| 4 | The loop clusters the repeated cause across both weeks: *"failed 6 times (67% of the calls that could fail this way)"*, with the denials cited by hash |
| 5 | The gates: a **host policy granting auto-apply buys nothing** (the analyzer's manifest is `auto_apply: Never` — its finding is free text the engine did not author); no reason is refused by the driver; an empty reason is refused by the engine (`LOP-E011`) |
| 6 | omar approves *and* applies, with a reason; the lesson is now recallable from the desk's own memory |
| 7 | omar approves the fix he rejected a week ago, citing the evidence he asked for → 3 more claims queued, a second mapping learned |
| 8–9 | The desk briefs itself out of two saved CAL queries; a second loop pass **dedups** rather than nagging |

## What each fixture exercises

| Fixture | Exercises |
|---|---|
| [`remits/01-meridian-2026-07-31.json`](fixtures/remits/) | 6 denials → 6 spawned tasks; a 3-claim `DN-311` cluster that clears the floor |
| [`remits/02-cascade-2026-07-31.json`](fixtures/remits/) | 5 denials, a different width in the same tick — the plan is identical, the fan-out is not |
| [`remits/03-meridian-2026-08-07.json`](fixtures/remits/) | The payoff: 3 `DN-311` auto-classified from memory, 2 `DN-517` left unmapped **under** the cluster floor, so no gate |
| [`remits/04-cascade-2026-08-07.json`](fixtures/remits/) | The repeat that turns a rejected proposal into an evidenced one, and gives the loop its cluster |
| [`codebook.json`](fixtures/codebook.json) | The crosswalk that only suggests — invented codes, fictional payers |
| [`decisions/00-desk-approves-itself.json`](fixtures/decisions/) | Separation of duties: the principal that started the run is refused (`RUN-E012`) |
| [`decisions/04-no-reason.json`](fixtures/decisions/) | A verdict with an empty `because` never reaches the run |
| [`decisions/01…`, `02…`, `03…`](fixtures/decisions/) | One approval, one rejection, and the approval that follows the rejection once the evidence arrives — each with the lead's written reason |

## What this example is teaching

**1. Dynamic width, declared once.** `split_denials` returns
`{"$send": [{"node": "classify_denial", "input": {…}}, …]}` and the runtime
does the rest: each spawn gets its own task path (`/0000`, `/0001`, …), its
own attempt counter, and its own retry budget. The target node's *static*
activation is preempted — it runs N times, not N+1 — and its downstream
edges do not fire until the whole batch has drained. The plan grain never
mentions a number.

**2. A spawned task sees only what it was handed.** A normal node's input is
the whole merged run state; a `$send` task's input is exactly the `input`
its spawn decision named, and nothing else. That is a feature: the spawner
decides what each task may see, which is where the desk chooses to pass the
*approved* mapping and withhold the raw remittance.

**3. Reducers are strings, and they are checked late.** The table is
`{"classified": "append", "denied_cents": "sum", …}` — the built-ins are
`lww` (the default), `append`, `sum`, `max`, `min`. `reducers` is an
untyped passthrough on the Workflow grain: write `{"kind": "append"}` and
it stores cleanly, mints a content address, and replicates in a bundle,
then refuses at **every future run start** with `RUN-E019`. `smoke.sh` step
2 does exactly that on purpose. Author the table once; assert it as stored.

**4. Order-independence is what makes the fold trustworthy.** The
reducers are batching-invariant by law and the scheduler merges in
*canonical* order (node index, then spawn order — task paths are
zero-padded so lexicographic is numeric), never completion order. So
`areev run verify` re-derives every checkpoint and byte-compares it, which
is what `smoke.sh` step 5 asserts. A fan-out you cannot replay is a
fan-out you cannot audit.

**5. The crosswalk suggests; the memory decides.** The same
`proposed_root_cause` sits in the file in week one and week two. In week
one it changes nothing — every denial is `unmapped` and a person is asked.
In week two the *approved* mapping arrives through the trigger's
`context_query` as a Fact, and three denials classify themselves. The
difference between the two weeks is one signed decision, not one more
config file.

**6. A rejection is a reason, not a mapping.** omar's "three claims is one
week of noise" leaves nothing behind that could auto-classify anything. The
cluster comes back the next week, and only then — with the evidence he
asked for — does it become a mapping. An agent that treated "no" as
"suppress this forever" would be a worse agent.

**7. Judgment is measured where it belongs.** `min_cluster_size` is a Fact
in `org.rcm.policy`, not a constant in the agent — so the loop can propose
moving it and a person can approve the move. The desk's own instrumentation
(`denial_root_cause`) is recorded under a name of its own, deliberately not
the node name, because the run journal already writes execution grains
under node names and the loop's rate gate divides by that denominator.

## Going live

Three seams, three processes. Nothing else changes — not the plan, not the
reducer table, not the gate, not the journal.

| Leg | Replace | With |
|---|---|---|
| **Inbound** | `agent.py connector` | a process that reads your 835/ERA feed (clearinghouse SFTP, payer portal, whatever it is) and emits `{items, cursor, more}` on stdout — the contract in [`docs/triggers.md`](../../../docs/triggers.md) |
| **Work** | `agent.py tools` | processes that read your claim system and write your resubmission worklist — JSON on stdin, JSON on stdout, one process per effect ([`docs/run.md`](../../../docs/run.md)) |
| **Human** | the `decide` fixture reader | your worklist UI or an email reply, calling `run respond --as <principal>`; the approver's identity **is** the audit record |

Before it touches real remittances:

- **Nothing in this repo is PHI-safe by default.** A real deployment puts
  the memory where your PHI lives and under your BAA, and reads
  [`docs/security-model.md`](../../../docs/security-model.md) first. Areev
  has a pseudonymization engine (`areev anonymize`, `set_anon_policy`) and
  a DSAR read/erase pair (`subject_report` / `forget_subject`) —
  [`docs/gdpr.md`](../../../docs/gdpr.md) is the article→capability map —
  but this example deliberately carries no identifiers to demonstrate them
  on. Keep it that way: a denial cluster needs a claim id and a code, not a
  person.
- **Deployment shape**, embedded vs Postgres, and the heartbeat that
  actually calls `trigger run`: [`../docs/deploy.md`](../docs/deploy.md).
- **The cluster floor is a policy decision.** Three is the number this
  example chose so a week of noise cannot spend a lead's attention. Yours
  will be different, and it belongs in `org.rcm.policy` where it can be
  argued with, not in the code.

## The last chapter: the desk changes how it reads itself

Everything above evolves what the desk *remembers*. Steps 10-13 of
[`improve.sh`](improve.sh) evolve the CAL that turns memory into a prompt —
the `desk_pulse` briefing query, which lives **in the file** and replicates
with it.

A model reads the desk's own record and notices that the briefing shows
`rcm-reducer-probe`, a reducer-validation fixture, beside the production plan:
a lead reading that briefing is one glance away from reasoning about the wrong
graph. It proposes a rewrite. Then:

- **A host policy granting auto-apply on the `query` class applies nothing.**
  The engine refuses twice over — `origin = llm` is categorically ineligible,
  and the auto-apply gate admits only the `memory` class. A grain edit changes
  one remembered value; a definition rewrite changes what *every future
  briefing* contains.
- **A lead signs it**, with a written reason, and the briefing changes: the
  probe is gone, the plan, the policy and the learned mappings stay.
- **It can be taken back.** A `DEFINE` writes a registry row, not a grain, so
  the ordinary "retract what the apply created" would undo nothing while
  reporting success. The engine records the inverse at apply — or refuses the
  apply — which is the only reason the rollback step can exist.

Keyless: the model leg is [`../../llm/mock.py`](../../llm/mock.py) replaying
[`fixtures/llm/query-revision.json`](fixtures/llm/query-revision.json), so CI
runs the whole path with no key. Point `LOOP_LLM_CMD` at a real backend for
the live version. The fixture proves the *governance*; it claims nothing about
what a model would propose.

## The pieces

| File | What |
|---|---|
| [`python/agent.py`](python/agent.py) | the whole agent — the two subprocess seams and the driver |
| [`smoke.sh`](smoke.sh) / [`improve.sh`](improve.sh) | the language-neutral act scripts; **the assertions are the spec** |
| [`fixtures/`](fixtures/) | synthetic remittances, the crosswalk, and the leads' decisions |
| [`CLAUDE.md`](CLAUDE.md) | the working rules for changing anything here |

## Where to go next

- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same
  governance shape with a bounded correction cycle, in three languages
- [`../sanctions-screening/`](../sanctions-screening/) — the agent's
  *code* as a governed grain, pinned by the host
- [`docs/run.md`](../../../docs/run.md) — the runtime: plans, `$send`,
  reducers, budgets, verify
- [`docs/loop.md`](../../../docs/loop.md) — the analyzers, the gates, the
  recommendation lifecycle
