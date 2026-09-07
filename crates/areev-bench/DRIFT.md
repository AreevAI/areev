# A ledger that changes its mind

[`RECEIPTS.md`](RECEIPTS.md) and [`ADBUY.md`](ADBUY.md) measure a governed
loop against a business that holds still: the conventions are fixed on day
one and only *revealed* over time. This experiment asks the harder half of
the question the loop exists to answer. A business does not hold still. When
a convention the agent has correctly learned is *replaced*, improving means
retracting — and retracting is the one thing a memory that only stores and
recalls cannot do.

Evidence: [`results/drift-sroie-2026-09-04/`](results/drift-sroie-2026-09-04/).
One seed; the README there says why.

## The design

The same real SROIE receipts as before, 160 of them per seed, against a
ledger whose demands move on a declared timeline
(`ledger_profile.py`, profile `sroie_drift`):

| document | what the accountant says |
|---|---|
| 2 | vendor and amount are required too |
| 41 | the tax file needs the vendor's address now |
| **81** | *"the group that bought us wants dates spelled out so nobody misreads them — write every date like 20 March 2018 from now on, not day-first with slashes"* |
| 82–160 | nothing. It is said **once** |

The flip is to a spelled-out month deliberately. Every model's prior for a
date is ISO, so a flip *toward* ISO would let an agent with no memory improve
for free and no post-flip gain would mean anything. A spelled-out month is a
real bookkeeping convention (it cannot be misread day-first or month-first),
no model writes it unprompted, and after document 81 the only way to score is
to have been told and to have let go of what came before.

The documents are real. The policy timeline is ours, declared here before
the run, and deterministic: `refile()` re-emits the builder's stored value
under the convention in force. Datasets with genuine annotated concept drift
are not agent-shaped, and agent-shaped datasets have no annotated drift, so
induced-and-declared drift on real documents is the honest option, and it is
the standard one in the concept-drift literature.

**Three arms**, the same 60 held-out receipts, re-read at documents 40, 80,
120 and 160 and scored under the convention in force at that point:

- **A — frozen.** The day-one agent, every rule rolled back through the API.
- **B — governed.** Propose, review, apply, measure.
- **C — ungoverned.** The **same memory** arm B reads, rendered without the
  loop: every correction the accountant made, verbatim, in order, nothing
  proposed, reviewed or retracted. This is what a store-and-recall memory
  puts in the prompt, and it is deliberately the generous form — it gets the
  complete record, including corrections the governed run's own trajectory
  elicited, without paying for the governance.

Every checkpoint journals its governed arm into the primary memory, so the
run leaves a time series of eval runs rather than one reading at the end.

## Result — seed 1

| checkpoint | ledger files | A | C | B | B vs A | B vs C |
|---|---|:---:|:---:|:---:|---|---|
| 40 | DD/MM/YYYY | 30 | 60 | **126** | 96 W, 0 L | 96 W, 30 L |
| 80 | DD/MM/YYYY | 30 | 60 | **173** | 143 W, 0 L | **114 W, 1 L** |
| **120** | DD Month YYYY | **0** | 59 | 114 | 114 W, 0 L | 114 W, 59 L |
| 160 | DD Month YYYY | 0 | 60 | 111 | 111 W, 0 L | 110 W, 59 L |

Exact matches of 240 (60 receipts × 4 fields). McNemar exact, paired per
(document, field); every comparison is p < 0.0001.

### Before the change: governing beats remembering, and the gap is behavioural

By document 80 the governed agent produces **every required field on every
receipt** — coverage 240 of 240 — and scores 173. The ungoverned arm never
moved off coverage 60. It carried the accountant's literal sentence *"I also
need the vendor and the amount on every one of these"* in its prompt on
every document, and kept filing one field, because a memory of someone once
saying a thing is not an instruction. Its 60 is the day-one agent's 30 with
better date formatting.

That is the answer to *"is governing worth more than remembering?"* in one
line: **remembering corrections improves accuracy on what the agent already
attempts; governing them changes what it attempts at all.**

### After the change: a double dissociation

Checkpoint 120, per field:

| field | A — frozen | C — ungoverned | B — governed |
|---|:---:|:---:|:---:|
| Invoice Date *(the one that changed)* | 0/60 | **59/60** | **0/60** |
| Vendor Name | not produced | not produced | 35/60 |
| Amount | not produced | not produced | 59/60 |
| Vendor Address | not produced | not produced | 20/60 |

The two systems fail in exactly opposite ways.

**The ungoverned arm tracks the change perfectly and learns nothing else.**
It gets the new format right 59 times in 60, because it carries the
accountant's latest words verbatim and the model follows the most recent
instruction — and after 120 documents it still produces one field.

**The governed arm learned all of that and cannot let go.** Full coverage,
59/60 on Amount, and `09/03/2018` where the ledger now wants `09 March 2018`
— zero of sixty on the one field that changed. Its memory at the end holds
both of these, live, at once:

> Format Invoice Date as **DD Month YYYY** and copy Vendor Name and Address
> exactly as printed…

> Format Invoice Date as **DD/MM/YYYY** and Amount as a plain number with two
> decimals…

So the loop *did* learn the new convention — sixty documents late, at
document 141, but it got there. It never withdrew the old one. Told two
incompatible things about the same field on every document, the agent
picked the stale one.

### The gate said `held`, five times out of five

```
seed1 verify: 5 verdict(s) — 5 held, 0 regressed
```

Every applied rule was marked `held`, including the two above that contradict
each other. No revert was proposed, so nothing was ever retracted.

The gate is right by its own definition. The outcome metric compares the
current score against a baseline frozen at proposal time — the newest
journaled run before the rule was proposed. Every rule here was proposed
when that run was the day-one agent at 30. After the flip the agent scores
111. It has not fallen below day one; it has fallen below *itself*, and the
comparison cannot see that. A keyless rehearsal of this exact design said the
same thing in the sharpest possible form — `held`, baseline 0.0 → current
0.0 — on a rule wrong on every document.

## What this establishes

1. **Governed memory dominates plain memory while the world holds still.**
   173 against 60, and the gap is what the agent *does*, not how well it
   formats.
2. **Governed memory does not track a change.** It learns the new rule and
   keeps the old one beside it.
3. **Plain memory tracks the change and never learns anything else.**

Neither is adequate on its own, and the missing piece in ours is not a
design flaw but two named mechanisms, each visible in this run and neither
in the engine today:

- **A contradiction between authored lessons is invisible.**
  `contradiction_sweep` fires on a subject holding two live values under a
  *functional* relation. `lesson` is not functional — an agent legitimately
  holds many — so two rules that contradict each other about one field are
  not a contradiction the analyzer can see. Making them one means a lesson
  declaring the field and aspect it governs, so two live rules on *(Invoice
  Date, format)* are structurally a conflict. That is a schema decision for
  authored proposals, recorded as such.
- **The outcome baseline is day one.** A rule that was right for eighty
  documents and wrong for eighty more still beats the deployment baseline.
  Comparing against a recent window, or a rule's own best, would have marked
  every date rule here `regressed` at checkpoint 120.

[`docs/loop.md`](../../docs/loop.md), "What the gate does not catch",
carries both.

## What is reported and not claimed

**One seed.** Three were planned. Seed 3 was cancelled by decision when the
programme was redirected to the stationary four-way comparison
([`FOURWAY.md`](FOURWAY.md)). Seed 2 ran to completion and is **not
published**, for a reason worth stating in full: `lessons_markdown` scanned
the memory with `LIMIT 300`, and a 160-document run writes over 400 facts.
Seed 2 had 11 approved rules and its prompt silently carried 4 of them. An
earlier reading of that seed as "rule accumulation harm" was wrong and is
retracted; every published 40-document run is under 130 facts and was
verified to render exactly the rules it did before the fix. Seed 1, at 264
facts, rendered all 5. Seed 2 also took 34 failed model calls in a provider
outage; seed 1 took none.

**Not metered.** This run predates the usage ledger the four-way comparison
introduced, so its cost is not journaled and is not quoted.

**Adaptation speed is not the finding.** Seed 1 took sixty documents to
author the new rule; the invalid seed 2 authored one within three. The
failure is the same either way: nothing is ever removed.

## Reproduce

```bash
cd crates/areev-bench/receipts
sh dryrun_drift.sh /tmp/drift-dry              # keyless: the plumbing, and the gate saying `held`
export OPENROUTER_API_KEY=…
PROFILE=sroie_drift SEED=1 sh drift.sh runs 160 60 40
python3 drift_stats.py runs --profile sroie_drift --write
```
