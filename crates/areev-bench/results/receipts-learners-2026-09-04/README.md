# receipts-learners-2026-09-04 — what each learner authors from one memory

The diagnostic that explains the receipts result, pre-registered in
[`../../RECEIPTS.md`](../../RECEIPTS.md) before it was run. It scores
nothing: five governed learn passes per model over copies of **one**
captured memory (seed 1's 40-receipt snapshot), so the evidence is held
fixed and only the proposer varies.

| learner (pinned) | proposed/pass | approved/pass | additive/pass | passes with an additive rule |
|---|:---:|:---:|:---:|:---:|
| `gpt-oss-120b` (deepinfra/bf16) | 1.0 | 0.00 | 0.00 | 0 of 5 |
| `qwen3-30b` (coreweave/bf16) | 2.0 | 0.20 | 0.00 | 0 of 5 |
| `qwen3-235b` (nebius/fp8) | 1.0 | 0.00 | 0.00 | 0 of 5 |

**Additive** means the rule names a ledger field to *capture*, rather than
saying how to write one the agent already produced. Not one of fifteen
passes across three models produced one. Every proposal, approved or
rejected, is shaped like *"if a Vendor Address fact exists for the current
document, store it without a trailing period"* — a rule about the fact
grains the harness writes, not about reading a receipt.

That is the reason the receipts run's rules are what they are, and it is a
property of **how the evidence is framed**, not of which model proposes:
three models of very different sizes converge on the same shape over the
same memory.

The framing is not that the person's words are missing. Dumped off the
wire, the bundle is 39 observations to 25 facts and its first item is the
accountant saying *"I also need the vendor and the amount on every one of
these"*. The model reads that — a complaint carrying the corrected values
in the same sentence — as the agent having **asked** for data it already
had, and proposes "do not request it again". `../../RECEIPTS.md` has the
full quote and what follows from it.

**A caveat against the model the authoring-rate grid selected.** All five
of `gpt-oss-120b`'s proposals were refused at GROUND here, against zero
refusals for either qwen. That grid measured a workload of tool failures;
this corpus has none, and the selection did not transfer. Stated because
the pre-registered rule picked that model and this is the evidence against
it, not tucked into a footnote.

Rows are `<learner>.jsonl`, one per pass: the DISCOVER funnel, every
proposal, and the supervisor's verdict and reason. `summary.json` is
derived from them.

## The wire dump

`wire/discover-request-response.json` is one complete DISCOVER exchange
recorded by `scripts/tee_llm.py` — the exact request the engine sent
(instructions, findings, all 64 evidence items) and the exact bytes the
model returned. It is the evidence for the paragraph above: the bundle
composition, the accountant's sentence sitting first in it, and the model
proposing "do not request adding it" against it. Nothing about that claim
needs to be taken on trust.
