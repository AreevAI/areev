# Governed self-improvement on public receipts

Does a memory an accountant corrects make a capture agent measurably,
causally better — on real documents anyone can download, with the learner
being a language model, under human approval at every step?

[`EXPENSE.md`](EXPENSE.md) answered yes on a private company's invoices, and
the corpus cannot leave the company. This is the same design on a public
corpus, with the harness in the repo (`receipts/`), so the whole result can
be re-run by a stranger for a few dollars.

**Two runs, and they answer differently — which is the point.**

**Run 2** (below): a model read a person's corrections, wrote the rule they
implied, a reviewer approved it, and the agent went from **31 to 123** of
240 exact on held-out receipts. Paired, **92 wins and 0 losses**, p <
0.0001, against a noise floor of 2. The Verify gate then confirmed the
improvement `held`, caught a later rule that hurt, and reverted it back to
123. Self-improvement under governance, on real public documents, for
$0.26.

**Run 1**, published in full below and not revised: the same design with a
different learner and one engine defect still in place made the agent
*worse* — 3 wins against 30 losses, p = 0.000001 — because one approved
rule contradicted an instruction the accountant had written into that
memory thirty-one times, and took the agent from 30 correct to 0. It passed
four independent gates. The fifth, measuring the held-out set, caught it.

Read together they are one finding: **the governance layer is what makes an
LLM-authored learner safe enough to be worth having.** Run 1 is what
happens without measurement; run 2 is what the same machinery is worth with
it. [Skip to the result](#result); everything before it is how the
measurement was set up, written before it was run.

## The corpus

**ICDAR 2019 SROIE** (Huang et al., [arXiv:2103.10213](https://arxiv.org/abs/2103.10213)):
626 scanned Malaysian retail receipts with line-level OCR transcriptions
and four key fields — company, date, address, total. Real documents,
competition ground truth, fetched by `receipts/build_sroie.py` from the
public mirror (`zzzDavid/ICDAR-2019-SROIE`, stdlib + cache, offline after
the first run). The dataset's terms are the Robust Reading Competition
portal's; nothing from it is committed here — only counts travel.

The agent reads the OCR text, never the image: SROIE ships the
transcriptions, so what is measured is capture and filing, not OCR.

**What the ledger wants is not what the receipt prints.** The filed row is
derived from the competition's truth under one stated bookkeeping
convention — dates as `DD/MM/YYYY`, totals as plain two-decimal numbers,
company and address exactly as printed — by a deterministic builder that is
in the repo in full. Receipts print dates seven ways and some totals carry
an `RM` prefix, so "exact" measures learning how this business files, and
"semantic" (right date, right amount, right name, however written) measures
reading the receipt at all. A receipt whose date or total cannot be
normalized unambiguously is dropped by the builder and counted, never
guessed at.

## The task

An agent reads a receipt and fills one ledger row. On day one its
instruction is to capture the invoice date and nothing else. An accountant
reviews each row against the filed ledger and replies the way a person
would, introducing requirements on a fixed schedule: the vendor and the
amount at receipt 2, the vendor's address at receipt 8. Formatting
corrections arrive as a person states them — once, as a rule, with the filed
value as the example.

Nothing else is ever told to the agent. Every requirement reaches it the
long way: the accountant says it → the loop proposes a rule → a reviewer
approves or rejects it → the approved rule is applied → it renders into the
prompt of every later receipt. The prompt is assembled from the memory file
on every single receipt, so what the agent knows is exactly what the memory
holds, and rolling a rule back through the API removes it from the prompt
structurally.

**The learner is a language model.** The loop's DISCOVER stage reads the
accountant's notes and the filed rows and authors the rule; GROUND checks
its premises against the cited evidence on a different model; VERIFY
stress-tests it; the accountant's reviewer (a third model, given the rubric
and the column names, never the ledger) decides. The deterministic analyzers
run too, as always, but this corpus produces no tool failures for them to
cluster: every applied rule here is one a model wrote and a reviewer read.
`LOOP_POLICY` selects the DISCOVER objective (`docs/loop.md`); the
authoring-rate instrument in `SELFIMPROVE.md` is what chose it.

## Why the measurement is paired

Whole-run accuracy across runs is noise: one flip in an early review
decision changes every correction after it. So nothing here compares two
runs. Each held-out receipt is its own control — read under prompts that are
byte-identical except that the learned rules are present or withdrawn — and
only pairs that disagree carry information (McNemar's exact test).

Three arms over the held-out receipts, which the experience phase never saw:

| Arm | State |
|---|---|
| **B** | the rules as the experience phase left them |
| **B2** | the same state, run again — the noise floor |
| **A** | every recommendation rolled back through the API |

Arm A is a genuine rollback, not a decision to stop rendering: the claim is
that the *governed apply* is the lever, so withdrawing it has to travel the
governance path.

## Approval is not the end: verify, and revert what hurts

A rule a reviewer approved from its text alone can still be useless or
harmful in use. The loop's Verify gate measures it (`docs/loop.md`,
"Evalset-backed outcomes"), and this harness is the first to exercise that
gate on LLM-authored rules against real held-out documents:

- **A0 is journaled before any learning.** The day-one agent reads the
  held-out set once; the pass is recorded in the memory as an evalset run.
  Every rule the loop then applies carries that evalset as its outcome
  metric (`Policy.outcome_evalset`), with A0 as its baseline — and A0 is
  the same prompt arm A later produces by rollback, so arm A replicates it.
- **B is journaled after learning.** A loop pass a day later (the engine's
  clock is pinned; nothing sleeps) records a verdict per applied rule:
  `held` if B scored at least A0, `regressed` otherwise, and a revert is
  proposed for any regression.
- **A harmful rule is admitted on purpose** (`regress.py`): a fixture
  "model" authors "write dates month-first", the stubs ground and verify
  it, and the reviewer approves it — a forced regression. It renders into
  the prompt like any other rule; the held-out set is read under it (arm
  H) and journaled; the next pass measures it as `regressed`, proposes the
  revert, the reviewer approves, and applying the revert retracts the rule
  through the same rollback path a person would use. The held-out set is
  read again (arm R). Then the same fixture model is asked again: the
  retracted rule is **not re-proposed** — a measured revert puts the
  finding on cooldown, exactly as a rejection does.

The keyless dry run (`dryrun.sh`) proves every step of that chain with a
mock agent whose only behaviours are "obey the date rule in the prompt" and
"obey the harmful one if present": exact steps up when the good rule lands,
down under the harmful one, back up on revert. What the paid run adds is a
real model on both sides of every rule.

**Seeds are task sets.** The corpus is seed-free; a seed permutes it and
assigns positions, so the experience receipts and the held-out receipts
differ between seeds. Three seeds are three replications over three task
sets, not one set re-rolled — the bound the synthetic bench had to state
about its 2026-08-26 run does not apply here.

## Pre-registered design (written before any paid run)

*Committed 2026-09-04, before a single paid receipt was read. The learner
configuration is the one the authoring-rate grid selected under its own
pre-registered rule (`SELFIMPROVE.md`, "Outcome (2026-09-04)"), not one
chosen after seeing anything on this corpus.*

**Corpus.** SROIE via `build_sroie.py`: 626 receipts fetched, **612 kept**
— 13 dropped for a date the builder will not disambiguate, 1 for an
unparseable total, 0 for missing OCR text.

**Scale.** Seeds 1, 2, 3. Per seed: 40 experience receipts (a learn pass
every 2 corrected receipts, a memory snapshot every 10) and 60 held-out.
Seeds permute the corpus, so the three runs are three task sets.

**Legs, all pinned, temperature 0, request seed = the run seed.**

| leg | model | pin |
|---|---|---|
| capture agent | `qwen/qwen3-30b-a3b-instruct-2507` | `coreweave/bf16` |
| learner (DISCOVER/VERIFY) | `openai/gpt-oss-120b` | `deepinfra/bf16` |
| GROUND | `openai/gpt-4o-mini` | `openai` |
| rule reviewer | `openai/gpt-4o` | `openai` |

DISCOVER objective `learner`; `Policy.outcome_evalset` names the held-out
set, field `exact`, higher-is-better.

**Primary.** Exact-match wins vs losses, **B vs A**, paired over (receipt,
field) by McNemar's exact test, pooled and per seed — with the **B vs B2
noise floor printed beside it**, because an effect that moves fewer trials
than two identical passes disagree on is not evidence.

**Secondary**, all pre-declared so none can be promoted afterwards: the
semantic metric; per-field coverage under A and B; the learning curve at
0/10/20/30 experience receipts against the same held-out set; the spend.

**The verify leg.** Per-rule verdicts after B (expected `held`; any
`regressed` is published with its revert). Then the forced regression: H
below B and R above H, both paired; the revert proposed by the gate,
approved, applied through the API; and the retracted rule not re-proposed
on the next pass.

**Stated in advance:**

- **B ≈ A is a publishable result** — "a model-authored learner did not
  move this corpus" — and ships with the full ledgers and the reviewer's
  reasons, not as a footnote.
- **A model-authored lesson is not reliably restorable.** There is no B2-
  as-re-apply arm here for the reason the synthetic bench documented: an
  authored lesson may simply not be re-authored. B2 is a second pass at
  the same state (the noise floor), and nothing else.
- **The reviewer is part of what is measured.** It is a model on a fixed
  rubric written before these runs, given the column names and never the
  ledger. Its rejections are published with their reasons.
- **The transfer from the authoring-rate grid is not assumed.** That grid
  measured a memory of tool calls; this corpus has none. If the learner
  authors nothing here, that is the result and the funnel will say where
  the drafts died.
- Every model call's usage is journaled; `verify.py --check` recomputes
  every published number from the trials and re-derives the checksums.

## Pre-registered diagnostic — `learners.py` (written before it was run)

A separate question from the one above, and it can only ever explain a
result, never improve one: **over one fixed captured memory, what does each
candidate learner author, and what does the supervisor do with it?** No
held-out pass, no agent call, no score — K governed learn passes per model
over copies of the same memory a run left behind.

The column it exists for is whether an approved rule is **additive** — does
it name a ledger field to capture, or only say how to write one the agent
already produced. That is the distinction EXPENSE.md's second defect turns
on: where an agent never fills a field at all, the rule it needs is
additive, and a formatting rule cannot supply it however cleanly it passes
every gate.

Stated before running it: this changes nothing about the run above, whose
numbers stand as published. If it shows one learner authoring additive
rules where another does not, that is a claim about **proposers on this
corpus** and would need its own A/B/A/B run to become a claim about
learning.

## Result

Two runs, in the order they happened. **If you read one thing, read
[run 2](#run-2-result--three-seeds)** — it is the result. Run 1 is
directly below it in document order because it came first and its
pre-registration must sit in front of its numbers, but it is the
cautionary half.

| | run 1 | run 2 |
|---|:---:|:---:|
| A — rules rolled back | 97/720 | 97/720 |
| B — rules applied | 70/720 | **382/720** |
| B vs A, paired | 3 wins, 30 losses | **286 wins, 1 loss** |
| verify-then-revert leg | passed, 2 of 3 seeds | passed, 2 of 3 seeds |

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../../docs/assets/receipts-selfimprove-dark.svg">
  <img src="../../docs/assets/receipts-selfimprove-light.svg" width="880"
       alt="Left: learning curves over the same held-out SROIE receipts after 0, 10, 20, 30 and 40 experience receipts, all starting near 31 of 240. Run 2 (green) rises at the first checkpoint to 124, 141 and 125 and holds. Cell C (gold) splits: one seed rises to 132, two stay flat at 60. Cell D (blue) splits three ways: one seed rises to 151, one to 52, one never moves off 36. Run 1 (red) stays flat, swings to 86 and falls to 33, or drops to 0. Right: pooled arms per cell — arm A is 97 of 720 in all four, drawn with a rule across them, while arm B is 70 for run 1, 255 for cell C, 245 for cell D and 382 for run 2.">
</picture>

Four cells: run 1, the ablation's cells C and D, and run 2. The four arm-A bars
are drawn with a rule across them because they are the same height —
97/720 in every cell, four runs across a day. Regenerate with
`python3 crates/areev-bench/scripts/receipts_chart.py
docs/assets/receipts-selfimprove "run 1=<dir>" "cell C=<dir>" "cell D=<dir>"
"run 2=<dir>"`;
every number comes from each cell's `RESULTS.json`, so the picture cannot
drift from the tables.

### Run 1

**On real receipts, a model-authored learner under human review made the
agent significantly worse — and the only thing that caught it was
measuring the outcome.** That sentence is the finding; the tables are the
evidence for it.

### The pre-registered primary test

Exact match, paired over (receipt, field) by McNemar's exact test, B (rules
applied) against A (every rule rolled back through the API):

| seed | rules applied | A exact | B exact | B vs A | p |
|---|:---:|:---:|:---:|:---:|:---:|
| 1 | 3 | 31/240 | 33/240 | 2 wins, 0 losses | 0.50 |
| 2 | 2 | 36/240 | 37/240 | 1 win, 0 losses | 1.00 |
| 3 | 5 | 30/240 | **0/240** | **0 wins, 30 losses** | **<0.0001** |
| **pooled** | | **97/720** | **70/720** | **3 wins, 30 losses** | **0.000001** |

The B-vs-B2 noise floor is 1, 0 and 0 discordant trials — two identical
passes essentially agree, so the swings above are not the agent re-rolling.
Arm A reproduces A0 within a trial in every seed (31/31, 36/36, 30/30),
which is the causal lever working: rolling the rules back restores the
state the agent was deployed in.

### What broke seed 3, and what let it through

One approved rule did all of it:

> **"Convert extracted Invoice Date to ISO 8601 (YYYY-MM-DD) before storing
> the fact."**

The ledger's convention is `DD/MM/YYYY`, and the accountant had said so in
the agent's own memory — **in 31 of the 40 observations that memory
holds** — in as many words: *"The Invoice Date is right but write dates as
DD/MM/YYYY, like 20/03/2018."* The rule proposes the opposite. It took the
one field the agent got right on every receipt from 30 exact to **0**.

It passed everything:

| gate | verdict |
|---|---|
| DISCOVER cite-check | cited real evidence |
| GROUND (a different model) | supported |
| VERIFY (adversarial, abstention-biased) | kept |
| the supervisor, on a rubric fixed before the run | **approved** — "Use ISO 8601 format for dates." |

Four independent checks, one of them a model reading a written rubric with
the column names in front of it, and a rule contradicting an instruction
stated thirty-one times in the same memory went through all four. It is worth being
precise about how close this was: in a smaller run against the same corpus
GROUND *did* catch this exact proposal, refusing it with *"The evidence
states that dates are written as DD/MM/YYYY, which contradicts the claim
about standardizing to ISO 8601 format."* The same gate, the same model,
the same error — caught once, missed once.

**That is the argument for measuring outcomes rather than trusting review.**
Nothing about the rule's text gives it away as harmful; only reading the
held-out set under it does.

### The gate does catch it — including the real one, unplanted

The verify-then-revert leg ran against each finished memory. Start with the
seed that broke, because there the gate had something genuine to find:

**Seed 3 — all five learned rules measured `regressed`, 30 → 0, and a
revert proposed.** No rule was planted for this. The loop measured what a
human reviewer had approved, against the A0 baseline journaled before any
learning, and correctly declared every one of them a regression. That is
the whole arc the design is for, on a failure nobody arranged.

The planted-harmful sub-test then failed its own checks in that seed, and
the harness says so (`all_ok: false`, naming which). It is undefined on an
already-broken run: with B at 0/240 a deliberately harmful rule cannot
lower the score further, and the revert the script reaches for is one of
the five real ones rather than its own. Reported rather than quietly
skipped — a sub-test that cannot mean anything here is not evidence that
anything passed.

In the two seeds where the learned rules held, the planted leg ran as
designed and every check passed:

| seed | learned rules | B | H — a harmful rule admitted on purpose | R — after the gate's revert |
|---|---|:---:|:---:|:---:|
| 1 | 3, all measured `held` (31 → 33) | 33 | **8** | **34** |
| 2 | 2, both measured `held` (36 → 37) | 37 | **10** | **37** |

In both, the harmful rule rendered into the prompt, collapsed the score,
was measured `regressed` against the A0 baseline, and `outcome_review`
proposed the revert. Approving and applying that revert retracted the rule
through the same rollback path a person would use and recovered the score.
Asked again on the next pass, the same proposal was **not** re-queued — a
measured revert puts the finding on cooldown, which is the engine change
this run's design forced.

So the two halves of the claim separate cleanly on real public data:

- **Governance: proven, and replicated.** A rule that hurt was detected from
  held-out measurement, reverted through the API, and kept from returning —
  and in seed 3 the rules it detected were real ones a human had approved,
  not planted ones.
- **LLM-authored learning: not proven, and at this scale actively harmful.**
  Pooled, the rules the loop authored and a reviewer approved cost the agent
  27 net trials.

### Why the rules are shaped like that — the loop read the conversation backwards

The pre-registered diagnostic
([`results/receipts-learners-2026-09-04/`](results/receipts-learners-2026-09-04/))
ran five governed learn passes per model over one fixed 40-receipt memory.
**Not one of fifteen passes, across `gpt-oss-120b`, `qwen3-30b` and
`qwen3-235b`, proposed a rule naming a field to capture.** Three models of
very different sizes converging on one shape over one memory says the shape
comes from the evidence rather than the proposer — so the next question is
what the evidence actually looked like, and the answer is not what it
appeared to be.

**The accountant's instruction is in the bundle, first, and not buried.**
Dumping one DISCOVER request off the wire
([`wire/discover-request-response.json`](results/receipts-learners-2026-09-04/wire/discover-request-response.json)):
the 64-item evidence bundle is **39 observations to 25 facts** — the
person's words dominate the numbers — and item one is, verbatim:

> Thanks — I also need the vendor and the amount on every one of these,
> otherwise I can't file it. Vendor Name is …

The DISCOVER instruction, for its part, already asks for exactly the rule
that sentence calls for, in its own example text: *"Either ADD an action it
is failing to take ('Record the vendor name and the amount on every
invoice, not just the date')"*.

**And the model proposes the opposite.** From the same request:

> "The agent repeatedly requests vendor name and address even when already
> recorded, causing redundant user interaction" → lesson: *"If a Vendor Name
> or Vendor Address fact exists for the current document, do not request
> adding it."*

The accountant's messages carry a complaint **and the corrected values** in
one sentence, and the model resolves that shape backwards: it reads a person
supplying a correction as the agent having *asked* for data it already had.
Almost every rule this corpus produced follows from that misreading, which
is why they are about not re-requesting and about tidying values already
present.

**The grain knew who spoke. The projection dropped it.** An Observation
carries `observer_id` and `observer_type`, and the receipts harness fills
both — the accountant's notes are recorded as `user:accountant`, type
`human`. The engine's evidence projection rendered only the text, so 31
attributed corrections reached the model as 31 anonymous sentences. That is
an engine defect of the same family as the one
[`EXPENSE.md`](EXPENSE.md) reports first (an Observation rendering to an
empty string): the single highest-value evidence a memory holds, degraded
on the way to the model.

**It is fixed, and the fix is a separate pre-registered experiment, not a
re-run of this one.** The published numbers above stand as measured; tuning
a harness after seeing its result is how a benchmark gets steered toward
its answer. What follows is the fix tested the only way that isolates it —
the same models, the same fixed memory, the one variable changed.

### Pre-registered follow-up: does naming the speaker change what is authored?

*Design committed before the re-run. The engine now renders an Observation
as `<observer> (a person) said of <subject>: <text>` when the grain records
an observer, and unchanged otherwise.*

`learners.py` again, over the **identical** seed-1 40-receipt memory, five
passes each for `gpt-oss-120b`, `qwen3-30b` and `qwen3-235b` — the same
three models that produced **0 additive rules in 15 passes** before. The
measure is the same: does an approved rule name a ledger field to capture,
or only say how to write one already produced. No held-out pass, no score.

- **Additive rules appear** → the misreading was the projection, and the
  diagnosis holds. It would still not be a learning claim: only a fresh
  A/B/A/B run can say whether such rules help, and that is a further
  experiment.
- **Nothing changes** → the misreading is not about attribution, the stated
  diagnosis is wrong, and this section says so.

#### Outcome: it moved one model of three

Evidence: [`results/receipts-learners-2026-09-04/attributed/`](results/receipts-learners-2026-09-04/attributed/),
beside the before-rows in the same directory.

| learner | additive rules per pass, before → after | passes with one, before → after |
|---|:---:|:---:|
| `qwen3-30b` | 0.00 → **0.60** | 0/5 → **3/5** |
| `qwen3-235b` | 0.00 → 0.00 | 0/5 → 0/5 |
| `gpt-oss-120b` | 0.00 → 0.00 | 0/5 → 0/5 |

For `qwen3-30b` the change is exactly what the diagnosis predicted, on both
sides. The rules it now writes name the document:

> "Copy the vendor's name and address exactly as printed on the receipt,
> including capitalization, punctuation and spacing."

> "Record the vendor address from every user observation, even if not
> explicitly requested, if it is present."

And the rule that defined the old behaviour — *"if a Vendor Name or Vendor
Address fact exists for the current document, do not request adding it"* —
is now **proposed and rejected** rather than approved. The misreading is
gone from the model that had it.

For the other two, nothing moved: both still write only formatting rules
about stored values. So attribution is **sufficient for one model and not
for two**, and the honest statement is narrower than "the fix works" — the
projection defect was real and was one cause, not the only one.

**This is still not a learning claim.** No held-out receipt was read in
this experiment. Whether rules of that shape help is what a fresh
A/B/A/B run would have to answer, and it has not been run.

### Twenty-two receipts of experience, generated by a broken agent

Seed 3's journal dates the damage precisely. The rule landed after receipt
18, and from receipt 19 to receipt 40 the agent scored **0 exact on every
single remaining receipt** — and the loop went on proposing, and the
supervisor went on approving, two more rules on top of it (after receipt 30
and after receipt 40).

| seed 3, held-out set at | rules | exact (of 240) |
|---|:---:|:---:|
| A0, before any learning | 0 | 30 |
| after 10 experience receipts | 2 | 30 |
| after 20 (the ISO rule now applied) | 3 | **0** |
| after 30 | 4 | **0** |
| B, after 40 | 5 | **0** |

So more than half of that run's experience — the corrections the loop then
learned from — was produced by an agent that had been broken by its own
last lesson. A learning loop that applies without measuring does not merely
fail to improve; it poisons the evidence its next lesson is drawn from.

That is the sharpest form of the cadence argument, and it is why the Verify
gate's schedule of checkpoints is load-bearing rather than ceremony. The
gate did fire here — correctly, on all five rules — but only when it was
finally given a held-out run to compare against, which in this design
happens at the end.

### The gain that did not survive, and why it is reported

The three learning curves — the same 60 held-out receipts against memory as
it stood after 0, 10, 20 and 30 experience receipts — show a process far
more volatile than the end states suggest, and volatile in both directions:

| exact (of 240) at | 0 | 10 | 20 | 30 | B (40) |
|---|:---:|:---:|:---:|:---:|:---:|
| seed 1 | 31 | 31 | **86** | 33 | 33 |
| seed 2 | 36 | 36 | 37 | 37 | 37 |
| seed 3 | 30 | 30 | **0** | 0 | 0 |

Seed 2 is flat throughout. The other two move by 55 and by 30 between
consecutive checkpoints, in opposite directions, and in both cases the
published end state records none of it.

Coverage says what moved: as deployed the agent fills the day-one field on
60/60 receipts and the other three on 0/60. At two rules, Vendor Name and
Vendor Address went to **47/60 each**. A third rule took them to 1/60.

That third rule was a near-duplicate of the second — Jaccard **0.571**
against the reviewer's 0.6 duplicate threshold, admitted by 0.03. Seed 2
also applied two rules and its coverage never moved, so the count is not
the mechanism and this write-up does not claim to know what is. What is
measured is that between two applies the same held-out set scored 86 and
then 33, and that **nothing in the run noticed**, because nothing measured
between applies. The gate had no run to compare against while the damage
was being done. That is an argument about the gate's *cadence*, and it is
the concrete thing this corpus taught.

## Pre-registered run 2 — does a configuration that authors additive rules help?

*Committed before it was run. **Run 1's numbers above stand as published**
and are not revised by anything here; this is a second experiment with its
own design, not a re-run.*

Run 1 established two things that together set this up: the loop's rules
did not help, and no learner authored a rule naming a field to capture.
The follow-up then found a configuration that does — `qwen3-30b` as the
learner, with the observer named in the evidence projection, authoring an
additive rule on 3 passes in 5. The obvious question is the one run 1
could not ask: **when the loop authors rules of that shape, does the agent
get better?**

**Two things change from run 1, and that is stated rather than hidden.**
The projection now names the observer, and the learner is `qwen3-30b`
instead of `gpt-oss-120b`. Both are needed to get additive rules at all —
`gpt-oss-120b` does not author them even with the fix — so this is not an
ablation of either change. It is a test of one intervention: *a
configuration that demonstrably authors additive rules*. Attributing the
outcome to the projection alone would need a third run and is not claimed.

Everything else is held: SROIE, seeds 1–3, 40 experience receipts with a
learn pass every 2, 60 held out, agent `qwen3-30b` pinned `coreweave/bf16`
at temperature 0, GROUND `gpt-4o-mini`, reviewer `gpt-4o` on the same fixed
rubric, the same primary test (exact-match B vs A, paired, with the B-vs-B2
noise floor beside it), and the same verify-then-revert leg.

**Stated in advance:** a null publishes as a null, next to run 1's, and
would say that additive rules are not what this corpus was missing. A gain
publishes with the caveat above about which of the two changes earned it.
If any seed regresses the way run 1's seed 3 did, the per-rule verdicts and
the reverts are published with it.

### Run 2 result — three seeds

**The agent learned, in every seed, by a margin nothing else in this
document approaches.**

| seed | A (rules rolled back) | B (rules applied) | B vs A | p |
|---|:---:|:---:|:---:|:---:|
| 1 | 31/240 | **123/240** | 92 wins, 0 losses | <0.0001 |
| 2 | 36/240 | **141/240** | 106 wins, 1 loss | <0.0001 |
| 3 | 30/240 | **118/240** | 88 wins, 0 losses | <0.0001 |
| **pooled** | **97/720** | **382/720** | **286 wins, 1 loss** | **2.3 × 10⁻⁸⁴** |

The B-vs-B2 noise floors are 2, 4 and 1 discordant trials, so two identical
passes essentially agree and the swing is the rules.

**The control is exact.** Arm A pooled is **97/720 in run 1 and 97/720 in
run 2** — same agent, same receipts, same seeds, same rollback path, and
the day-one baseline lands on the same number either way. The whole
distance between run 1's B of 70 and run 2's B of 382 is what the loop
authored and a reviewer approved. There is no drift to explain away.

Coverage, pooled, is the mechanism in one line: under arm A the agent fills
the day-one field on 180 of 180 held-out receipts and the other three on
**0**. Under arm B it fills Vendor Name and Amount on 180 of 180, and
Vendor Address on 60 of 180 (seed 2 alone learned the address).

Evidence:
[`results/receipts-sroie-run2-2026-09-04/`](results/receipts-sroie-run2-2026-09-04/).

### Run 2 result — per seed

**The agent learned, and the gain is large, clean and causally attributed.**
Same 60 held-out receipts, same agent, same seed, same evalset hash
(`e75276a1a5002547`) as run 1's seed 1 — so this is directly comparable to
the 31 → 33 null above.

| arm | exact (of 240 trials) |
|---|:---:|
| A — every rule rolled back through the API | 31 |
| B — rules applied | **123** |
| B2 — the same state again | 123 |

Paired over the same trials: **B vs A is 92 wins and 0 losses, p < 0.0001**,
against a B-vs-B2 noise floor of 2 discordant trials. Not one trial got
worse. Arm A reproduces run 1's arm A exactly at 31, which is what makes
the comparison a comparison: the baseline did not move, the rules did.

Coverage says what the agent actually started doing:

| field | A (0 rules) | B |
|---|:---:|:---:|
| Invoice Date (day one) | 60/60 | 60/60 |
| Vendor Name | **0/60** | **60/60** |
| Amount | **0/60** | **60/60** |
| Vendor Address | 0/60 | 0/60 |

Run 1 never captured the Amount field on a single receipt in any seed. Run
2 captures it on every one. The rule that did it, authored by the model and
approved by the reviewer on the same fixed rubric:

> **"Extract and record the vendor name and amount on every receipt, not
> just the date."**

That is the additive shape run 1 could not produce in 19 learn passes
across three seeds, and it is worth being exact about what it demonstrates:
a language model read a person's corrections, wrote the rule those
corrections implied, a reviewer approved it, and applying it moved a real
agent on real documents from 12.9% to 51.2% exact — with the rollback arm
proving the rules are the lever.

**What earned it is not isolated.** Two things changed from run 1, as
pre-registered above: the observer is now named in the evidence, and the
learner is `qwen3-30b` rather than `gpt-oss-120b`. Both were needed to get
additive rules at all. Which one carries the effect is not answered here
and is not claimed.

#### What the gain is, and what it is not

All 286 exact wins fall on fields arm A left **completely blank**. On
Invoice Date — the one field the day-one agent already captured, on 180 of
180 receipts in both arms — the rules won **nothing**:

| field | coverage, A → B (of 180) | exact wins | semantic wins |
|---|:---:|:---:|:---:|
| Amount | 0 → 180 | 157 | 176 |
| Vendor Name | 0 → 180 | 121 | 169 |
| Vendor Address | 0 → 60 | 8 | 49 |
| **Invoice Date** | **180 → 180** | **0** | **0** |

So the precise claim this run supports is: **the loop taught the agent
*which* fields to capture, and having been told, the agent read them right
394 times and filed them in the ledger's exact form on 286 of those.** The
gap between those two numbers is the filing conventions — a value read
correctly but written in a form the ledger rejects — and closing most of it
is real learning, because the conventions were never in the agent's prompt
either.

**The claim it does not support** is that governed memory made the agent
better at something it was already doing. On the field it already did, the
rules changed nothing. A reader should discount any reading of "12.9% to
51.2%" that implies otherwise, and anyone citing this should quote the
table rather than the headline.

That is also the honest answer to the obvious objection — *of course an
agent told to capture the vendor starts capturing the vendor*. It is
correct, and it is why the information path matters more than the effect
size here: the agent was never told. The requirement existed only in an
accountant's corrections, and the only route from there to the prompt ran
through the loop authoring a rule and a reviewer approving it.

#### The curves: learned early, and they stay

Same held-out receipts against memory as it stood through each run:

| exact (of 240) at | 0 | 10 | 20 | 30 | B (40) |
|---|:---:|:---:|:---:|:---:|:---:|
| **run 2**, seed 1 | 31 | **124** | 124 | 125 | 123 |
| **run 2**, seed 2 | 36 | **141** | 132 | 150 | 141 |
| **run 2**, seed 3 | 30 | **125** | 120 | 118 | 118 |
| run 1, seed 1 | 31 | 31 | 86 | 33 | 33 |
| run 1, seed 2 | 36 | 36 | 37 | 37 | 37 |
| run 1, seed 3 | 30 | 30 | 0 | 0 | 0 |

Every run 2 seed reaches its level by the first checkpoint and holds it for
thirty more receipts and a dozen more learn passes, the reviewer rejecting
most further proposals. Run 1's seeds either never moved, swung and
collapsed, or went to zero.

That stability is worth as much as the level. A loop whose gain survives
its own subsequent passes is one an operator can leave running; run 1's was
not, and the run 1 sections below are the record of why.

#### Seed 3's verify leg did not finish

A provider error killed the harness mid-arm after it had established that
both of seed 3's learned rules measured `held` with no revert proposed. It
could not be re-run: the interrupted attempt had already applied its
deliberately harmful rule and crashed before the revert, so that memory now
carries an applied rule that is an artifact of the crash. Seed 3's headline
numbers are unaffected — its evaluation completed before the verify leg
began — and seeds 1 and 2 ran the leg in full, as did two seeds of run 1.
The crash produced a fix: an arm now scores a failed call as the document
producing nothing and still reports, rather than taking the measurements
before it down too.

#### The whole cycle, on the improved agent

The verify-then-revert leg ran against the same memory and passed every
check:

| step | exact (of 240) |
|---|:---:|
| B — the learned rule applied | 123 |
| the gate's verdict on it | **`held`**, baseline 31 → current 123 |
| H — a harmful rule admitted on purpose | 108 |
| R — after the gate's revert was approved and applied | **123** |

So on one corpus, in one run, every step of the loop is exercised against a
real measurement:

1. The agent is deployed knowing one field.
2. An accountant corrects it; the corrections go into memory.
3. The loop reads them and authors the rule they imply.
4. A reviewer approves it on a rubric fixed before the run.
5. Applying it moves the agent from 31 to 123 exact.
6. The Verify gate re-measures and confirms the improvement `held`.
7. A later rule that hurts is measured `regressed`, and a revert proposed.
8. Applying the revert restores 123, and the retracted rule is not
   re-proposed on the next pass.

Steps 6 to 8 are what run 1 also demonstrated. Steps 3 to 5 are what run 1
could not, and are the difference between a governed loop that only
protects an agent and one that also improves it.

## Pre-registered ablation — which of the two changes earned run 2's gain?

*Committed before either cell was run. Runs 1 and 2 stand as published;
this adds the two missing cells of a 2×2 and does not revise them.*

Run 2 changed two things from run 1 at once, and said so. The 2×2 that
separates them, with the two runs already in it:

| | evidence **anonymous** | evidence **named** |
|---|---|---|
| learner `gpt-oss-120b` | **run 1** — 70/720 | **cell D** |
| learner `qwen3-30b` | **cell C** | **run 2** — 382/720 |

`Policy.evidence_attribution: anonymous` restores the previous rendering
exactly (test-pinned), so each cell turns one variable and nothing else.
Everything else is held: corpus, seeds, splits, agent, GROUND, reviewer,
rubric, scale, and the same primary test.

**Cell C is the load-bearing one.** If `qwen3-30b` with anonymous evidence
already reaches run 2's level, the projection fix was not needed and the
learner model carries the result. If it lands near run 1, attribution is
necessary and the two changes are jointly required.

**Cell D asks whether attribution is sufficient.** The authoring diagnostic
already says `gpt-oss-120b` writes no additive rule even with the observer
named (0 of 5 passes), so a null here is expected; it is run because an
expected null that is not measured is an assumption.

**Stated in advance.** Three seeds each, the same 40/60 split. The four
cells are compared on arm B, unpaired across runs, and a cross-run
comparison at this scale is read against the fact that **arm A is 97/720 in
both existing runs** — an equality that is itself the drift check. Whatever
lands is published in this table, including a cell that contradicts the
reading above. If cell C matches run 2, this document will say the
projection fix was not what earned it.

### Ablation result — both changes earned part of it, and different parts

Evidence: [`results/receipts-ablation-2026-09-04/`](results/receipts-ablation-2026-09-04/).

| | evidence **anonymous** | evidence **named** |
|---|---|---|
| learner `gpt-oss-120b` | **run 1** — A 97 → B **70** (3 wins, 30 losses) | **cell D** — A 97 → B **245** (148 wins, 0 losses) |
| learner `qwen3-30b` | **cell C** — A 97 → B **255** (158 wins, 0 losses) | **run 2** — A 97 → B **382** (286 wins, 1 loss) |

**Arm A is 97/720 in all four cells.** Same agent, same receipts, same
seeds, same rollback path, four separate runs across a day. That equality is
the drift check the pre-registration promised, and it holds exactly, so the
B column is comparable.

Cell C also passed the **verify-then-revert leg on 3 of 3 seeds** — every
one of the eleven checks, on every seed, including the two whose learned
rules bought nothing. On seed 3 the planted rule took 60 to 13, the gate
measured `regressed`, and the revert restored 60 exactly, with no
re-proposal. Governance does not depend on the learner being the good one.

Cell D's leg passed in full on 1 of 3, and the two that did not are reported
with their reasons rather than counted as passes. On seed 1 the planted rule
*raised* the score (149 → 159), so there was no regression to catch and the
sub-test is undefined — the same undefined case as the second corpus's seed
3. On seed 2, which applied no rules at all, the gate still did its job (the
planted rule took 36 to 7 and the revert restored 36 exactly); its single
failed check was `every applied lesson got a verdict`, which asserted that
*some* lesson got one and so could not be satisfied by a seed that learned
nothing. That is a defect in the check, not in the gate, and it is fixed —
zero applied lessons and zero verdicts now passes.

### The prediction for cell D was wrong, and it was wrong by a lot

The pre-registration above says a null was expected here, because the
authoring diagnostic had `gpt-oss-120b` writing no additive rule in 0 of 5
passes even with the observer named. Cell D is not a null. It is **+175**,
within ten trials of what swapping the learner buys.

That is worth two separate corrections.

**The decomposition published before cell D landed was an artifact of the
order it was measured in.** It read *swap the learner, +185; add attribution
on top, +127*, which makes the learner sound primary and attribution a
top-up. Measured from the same baseline they are near-equal main effects:

| from run 1, change | B | gained |
|---|:---:|:---:|
| nothing — run 1 | 70/720 | — |
| **attribution only** — cell D | 245/720 | **+175** |
| **learner only** — cell C | 255/720 | **+185** |
| both — run 2 | 382/720 | **+312** |

Together they buy less than the +360 that adding the two main effects would
predict, so they overlap. But neither is a top-up on the other: either one
alone recovers rather more than half of the total gain.

Cell D is the most variable cell in the square — 118, 0 and 30 wins across
its three seeds, with one seed applying no rule at all. Attribution makes
this learner *capable* of the result; it does not make it reliable at it.

#### The seed that learned nothing is the control this programme lacked

Cell D's seed 2 applied **zero** rules across nineteen governed passes (two
proposals, both rejected), so arms A and B are the same prompt. It was run to
measure a learner and it accidentally measured the instrument:

**Six independent evaluations of it returned byte-identical trials** — the
three curve snapshots and all three eval arms, 240 trials each, the same
extracted value on every field of every receipt, scoring 36 exact and 55
semantic every time.

Two things follow, neither of which had been measured before:

- **The pinning is bit-reproducible.** `env.sh` pins every leg because
  *unpinned* routing once moved 5 of 60 held-out tasks between two
  byte-identical runs. Pinned, at temperature 0 with a request seed, six runs
  of 240 trials agree exactly.
- **The rollback path contributes nothing of its own.** Arm A is produced by
  genuinely rolling every rule back through the API, not by declining to
  render one. With nothing to roll back it returns arm B exactly. So every
  A-versus-B gap in this document is the rules, and not an artifact of the act
  of rolling back — which is the control the causal claim rests on, and until
  this seed it was argued rather than shown.

**The diagnostic did not predict the run — for the second time.** The first
was when it measured what a model *authors* while the runs measured what
*survives review* (corrected below). This is the second: it measured a
learner writing nothing useful over one fixed memory, and that learner is
worth +175 over full runs. The pre-registration for this corpus already
refused to assume the transfer — *"the transfer from the authoring-rate grid
is not assumed"* — and that caution is now measured rather than merely
stated. `learners.py` is a screening tool for choosing what to run. It is
not a predictor of what a run will do, and this document should not be read
as though it were.

**They still buy different things**, which is the part worth keeping.

*The learner model buys the convention.* Cell C's gain is almost entirely
one rule — *"Write all dates in DD/MM/YYYY format"* — and on two of its
three seeds coverage never moves at all: 29 and 30 of its wins are the date
field alone. That is the same field, on the same corpus, where
`gpt-oss-120b` proposed **ISO 8601** and took run 1's seed 3 from 30 correct
to 0. Two models, one corpus, one field, opposite rules.

*Attribution buys reliability, not capability.* Without it the model still
finds the additive rule and still proposes it — repeatedly. What changes is
whether it survives review. In cell C the additive rule got through on
**1 of 3 seeds**; in run 2, on **3 of 3**. Seed 1's ledger is the clearest
case: fifteen rejections, one approval, and the rejected proposals all read
like

> "Extract vendor name and amount from every invoice **immediately upon
> receipt, before any other processing**"

which the reviewer refuses, correctly, as *"presupposes a stage the
assistant does not have."* With the observer named the same model writes

> "Extract and record the vendor name and amount on every receipt, not just
> the date."

So the mechanism is not that attribution helps the model understand. It is
that **an unattributed correction leads the model to invent a workflow
around the exchange**, and a reviewer doing its job refuses rules that
depend on stages the agent has not got. The projection defect converted
into a governance rejection, which is why it cost a whole run.

*Cell D shows a second symptom, and it is the one that unifies them.* The
other learner does not get rejected — it gets the direction backwards. On
**seed 3, the same seed, the same corpus, the same reviewer, with only the
evidence rendering changed**:

| `gpt-oss-120b`, seed 3 | A | B | paired |
|---|:---:|:---:|---|
| run 1 — evidence anonymous | 30 | **0** | 0 wins, 30 losses |
| cell D — evidence named | 30 | **60** | 30 wins, 0 losses |

A mirror, and the rules say why. Anonymous, that seed wrote *"Convert
extracted Invoice Date to ISO 8601 (YYYY-MM-DD)"* plus three brittle patches
for the exact strings it had seen (`NO <number>.`, `DIMILIKI OLEH :`). Named,
it wrote *"Record invoice dates in DD/MM/YYYY format with slashes, not
hyphens"* and *"Record the invoice date and vendor address for every receipt,
not just vendor name and amount"* — the right convention, and the additive
rule.

Nothing about the evidence changed except who is named as having said it.
The accountant's instruction was in that memory either way, **31 times in 40
observations**.

So the unified statement is that **attribution supplies the *direction* of a
correction.** Read as a bare sentence, a correction shows the model that two
values differ but not which one is authoritative. It then does one of two
things, and this ablation caught both: it falls back on its own prior about
how a date should look and patches the specific strings in front of it
(`gpt-oss-120b`, silently wrong, passes review), or it invents a workflow
that would explain the exchange (`qwen3-30b`, correctly rejected). Named, the
same evidence identifies the ledger's convention and both models generalise
from it.

That also makes run 1 a confirmed diagnosis rather than a plausible one. The
projection defect was *identified* from run 1's failure; cell D is the
experiment that could have refuted it, in a cell pre-registered as an
expected null, and it did not.

**This corrects the reading published earlier in this document.** The
"Outcome: it moved one model of three" section measured *approved additive
rules over one fixed memory* and concluded attribution changed what the
model authors. Over full runs the sharper statement is that it changes what
survives review, and the difference between 1 of 3 seeds and 3 of 3 is the
whole of that learner's gap. Cell C is also not a null: at 158 wins and 0
losses it is a real result on its own, and anyone citing run 2's 286 should
know that 158 of it survives without the engine fix.

Cell D then corrected this reading a second time, in the other direction:
"what survives review" is the mechanism for `qwen3-30b`, but for
`gpt-oss-120b` the bad rules survive review perfectly well and are simply
wrong. Both reduce to attribution carrying the direction of the correction.

### Not affected by the τ² bridge bug

The sibling [`tau2/`](tau2/README.md) harness had a tool-call parsing
defect that voided one of its arms, and it is worth saying plainly that it
cannot touch anything above. The receipts agent uses **no tools**: it sends
`"tools": []` and its whole reply is one JSON object read from the message
content. There is no tool-call parsing on this path to get wrong. The
numbers here were produced by an agent that was acting normally, which the
per-field coverage independently shows — it filled the day-one field on
60 of 60 receipts in every arm.

### Cost

Measured as the account-level delta, which is every model call each run
made:

| | spend |
|---|---|
| run 1, three seeds | **$0.26** |
| run 2, three seeds | **~$0.55** |

Each run is 1,700 held-out receipt reads plus 120 experience reads, around
55 governed learn passes, and the reviewer's verdict on every proposal.
Run 1's agent leg alone is $0.06 (528k prompt and 26k completion tokens at
Qwen3-30B's $0.10/$0.30 per million); the rest is the reviewer on `gpt-4o`
and GROUND on `gpt-4o-mini`. Run 2 costs more because it learns more: more
rules proposed means more reviewer calls.

Cost is not the obstacle to running this, in either direction. Nothing in
run 1's null is explained by having spent too little, and run 2's result
cost less than a cup of coffee.

### Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_sroie.py                       # 626 fetched, 612 kept
sh dryrun.sh /tmp/dry                        # the keyless gate — no key needed
export OPENROUTER_API_KEY=…
for S in 1 2 3; do SEED=$S LEARNER_MODEL=openai/gpt-oss-120b \
  LEARNER_PIN=deepinfra/bf16 sh curve.sh runs 40 60 10; done
python3 verify.py runs --write               # every published number, recomputed
```

`--write` refuses if the seeds disagree on which artifacts they carry.

That shape is structural, not exotic. `curve.sh` evaluates the learning-curve
snapshots **last**, after the paired evaluation and the regress leg, so a
seed still running already has its headline numbers on disk and no curve at
all — and a results file written then looks complete and correct. It has
cost a curve three times now: once to `set -e` aborting on a failed regress
check (the comment in `curve.sh` records it), once on the ad-buy corpus, and
once on the ablation's cell C, where it took a whole verify-then-revert leg
with it. Each time every published number was right and nothing said a seed
had contributed less than the others.

Wait and re-run, or pass `--allow-ragged` for a run genuinely cut short —
which records what is absent, so `--check` fails the day the rest arrives.

Committed evidence and what deliberately stays local:
[`results/receipts-sroie-2026-09-04/`](results/receipts-sroie-2026-09-04/).

### Batch reads

A held-out read's prompts never depend on an earlier answer, so a whole
arm can go to a provider's batch endpoint as one job: half the list price
on the usual tiers, and no rate-limit storm can cost the read a document.
`evaluate.py --batch` submits each arm through `$AGENT_BATCH_CMD` (see
`env.sh`). `scripts/batch_toolcall.py` speaks two shapes — the OpenAI files
+ batches flow that OpenAI, Together and Groq share, and OpenRouter's
inline `POST /api/beta/batches`, whose completed batch object carries the
results and the actual cost — and `--selfcheck` runs either against
`scripts/mock_batch_server.py`. The governed stream and mem0's adds stay
synchronous, because their next request carries the last answer.
OpenRouter batches only models with a `:batch` variant (69 of 430 on
2026-09-06, priced at the batch tier), and the agent model of every study
so far has none, so a batched study picks its agent model with that in
mind — and validates the choice the way the streamlake move was validated,
the same read synchronous and batched, paired trial for trial. Batch
usage rows carry the tier's discount, or the provider's reported cost
where it gives one, and `cost.py` uses them.
