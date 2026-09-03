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

**Seeds are task sets.** The corpus is seed-free; a seed permutes it and
assigns positions, so the experience receipts and the held-out receipts
differ between seeds. Three seeds are three replications over three task
sets, not one set re-rolled — the bound the synthetic bench had to state
about its 2026-08-26 run does not apply here.

## Pre-registered design (written before any paid run)

*To be committed, with the model choice the authoring-rate instrument
made, before the first paid receipt is read.*

- Corpus: SROIE via `build_sroie.py`; the builder's drop counts reported.
- Seeds 1, 2, 3; per seed 40 experience receipts (learn pass every 2
  corrected receipts, memory snapshot every 10) and 60 held-out.
- Agent `qwen/qwen3-30b-a3b-instruct-2507` pinned `coreweave/bf16`,
  temperature 0, request seed = the run seed. Learner (DISCOVER/VERIFY):
  the model the authoring-rate instrument chose, pinned. GROUND
  `openai/gpt-4o-mini` (openai). Reviewer `openai/gpt-4o` (openai) on the
  fixed rubric in `accountant.py`.
- Primary: exact-match wins vs losses, B vs A, paired over (receipt, field),
  pooled and per seed; the B vs B2 noise floor beside it. Secondary:
  semantic; per-field coverage; the learning curve at 0/10/20/30/40
  experience receipts against the same held-out set.
- Publishes whatever lands. B ≈ A is the result "a model-authored learner
  did not move this corpus" and ships with the ledgers.
- Every model call's usage is journaled; the spend is reported.

## Result

*Not yet run.*

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_sroie.py                                    # once; cached
sh dryrun.sh                                              # keyless plumbing gate
export OPENROUTER_API_KEY=…
for S in 1 2 3; do SEED=$S sh curve.sh runs; done         # experience + paired eval + curve
python3 summarize.py runs/seed1/eval/trials.json runs/seed2/eval/trials.json runs/seed3/eval/trials.json
```

The binding must be built from this tree
(`maturin develop --release -m crates/areev-py/Cargo.toml` into `.venv`);
`env.sh` documents every leg and how to override one.
