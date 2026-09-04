# Governed self-improvement on public receipts

Does a memory an accountant corrects make a capture agent measurably,
causally better — on real documents anyone can download, with the learner
being a language model, under human approval at every step?

[`EXPENSE.md`](EXPENSE.md) answered yes on a private company's invoices, and
the corpus cannot leave the company. This is the same design on a public
corpus, with the harness in the repo (`receipts/`), so the whole result can
be re-run by a stranger for a few dollars.

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

*Running. Nothing published yet.*

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_sroie.py                                    # once; cached
sh dryrun.sh                                              # keyless plumbing gate
export OPENROUTER_API_KEY=…
for S in 1 2 3; do SEED=$S sh curve.sh runs; done         # A0 + experience + paired eval + verify/revert + curve
python3 summarize.py runs/seed1/eval/trials.json runs/seed2/eval/trials.json runs/seed3/eval/trials.json
```

The binding must be built from this tree
(`maturin develop --release -m crates/areev-py/Cargo.toml` into `.venv`);
`env.sh` documents every leg and how to override one.
