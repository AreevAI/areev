# Governed self-improvement on a real expense workflow

Does a memory an operator corrects actually make an agent better — measurably,
causally, on work that was not designed to be a benchmark?

This records a measurement against a **private dataset**: a real company's
supplier invoices and the spreadsheet a person fills in from them, 206 rows
joined to 206 documents. The documents are not in this repo and will not be.
Only the counts below travel; the harness that produced them lives with the
data. What is reproducible here is the *design*, not the corpus — the same
harness runs against any invoice-plus-sheet pair.

`scripts/expense_curve_chart.py` renders the chart from the numbers in this
file. Re-run it by hand when they change.

## The task

An agent reads an invoice and fills one spreadsheet row. On day one its
instruction is to capture the invoice date and nothing else. An accountant
reviews each row against the real sheet and replies the way a person would,
introducing new requirements on a fixed schedule that mirrors how the real
form actually grew — vendor, amount and currency at invoice 2, a description
at 6, a category at 10, a payment date at 16.

Nothing else is ever told to the agent. Every requirement reaches it the long
way: the accountant says it → the loop proposes a rule → a reviewer approves
or rejects it → the approved rule is applied → it renders into the prompt of
every later invoice. The prompt is assembled from the memory file on every
single invoice, so what the agent knows is exactly what the memory holds.

## Why the measurement is paired

The first eight exploratory runs were worthless and are reported here because
the reason generalises. Whole-run accuracy ranged 0.9% to 13.0%, and two runs
differing only by a prompt header scored 13.0% and 0.9% — one flip in an
early review decision changes every correction after it, and the run diverges.
**The noise between runs was larger than the effect being looked for.**

So nothing below compares two runs. Each invoice is its own control: the same
held-out invoice is read twice under prompts that are byte-identical except
that the learned rules are present or withdrawn. Only pairs that *disagree*
carry information (McNemar's exact test); a pooled accuracy delta would hide
how few trials actually moved.

Three arms, over 30 held-out invoices never seen in the experience phase:

| Arm | State |
|---|---|
| **B** | the rules as the experience phase left them |
| **B2** | the same state, run again — the noise floor |
| **A** | every recommendation rolled back through the API |

Arm A is produced by a genuine rollback, not by declining to render the
lessons: the claim is that the *governed apply* is the lever, so withdrawing
it has to travel the governance path. There is no fourth re-applied arm — the
lifecycle forbids `RolledBack -> Applied`, correctly, and with each invoice a
stateless call there is no order effect for it to rule out.

## Result

30 held-out invoices, 210 (invoice, field) trials per arm, exact match against
the filed value.

| Seed | B vs A | Noise floor (B vs B2) | p |
|---|---|---|---|
| 1 | **21 wins, 0 losses** | 1 | <0.0001 |
| 2 | **32 wins, 0 losses** | 1 | <0.0001 |
| 3 | **30 wins, 0 losses** | 0 | <0.0001 |

The noise floor is what makes the rest meaningful: two passes over identical
memory disagree on 0–1 of 210 trials, so an effect of 21–32 with no
regressions in any seed is not a re-roll.

Coverage — whether the agent put *any* value in a field — shows the mechanism.
Arm A is identical in all three seeds:

| Field | Arm A | Arm B (per seed) |
|---|---|---|
| Invoice Date | 23/30 | 23, 16, 23 |
| Vendor Name | **0/30** | 21, 16, 23 |
| Amount | **0/30** | 2, 16, 23 |
| Currency | **0/30** | 0, 16, 23 |
| Expense Description | **0/30** | 23, 16, 0 |
| Payment Date | **0/30** | 0, 15, 0 |

A single pair shows what a "win" is. Same invoice, same prompt scaffold, the
only difference being whether the recommendations are applied:

```
Vendor Name    A = ''                 B = 'Anthropic, PBC'    filed: 'Anthropic, PBC'
Invoice Date   A = '15/11/2025'       B = '11/15/2025'        filed: '11/15/2025'
```

The second is not a formatting nicety: without the rule the agent transposed
day and month.

## Learning curve

Held-out accuracy against memory as it stood after 0, 10, 20, 30 and 40
corrections. Same 30 invoices and same seven fields at every point, so the
only thing varying is how much the agent has been taught.

| Corrections seen | Exact | Read correctly | Fields covered |
|---|---|---|---|
| 0 (rolled back) | 0/210 | 12/210 | 1 of 7 |
| 10 | **38/210** | 56/210 | 4 of 7 |
| 20 | 36/210 | 53/210 | 4 of 7 |
| 30 | 36/210 | 52/210 | 4 of 7 |
| 40 | 36/210 | 54/210 | 4 of 7 |

Nearly all of the gain arrives in the first ten corrections and then the curve
is flat to very slightly negative. Governed learning here is a large one-time
step, not a steady climb.

**A curve this repo does not publish**: the running score *during* the
experience phase falls (32% → 20%). That is not the agent getting worse — the
accountant is adding required fields as it goes, so the denominator grows. It
measures the goalpost moving, and presenting it as a learning curve would be
wrong in the flattering direction's opposite.

## What this does not show

- **Absolute accuracy is low.** 17% exact, 26% read-correctly. This is a
  working learning loop, not a production capture agent.
- **Three of seven fields never move.** Category and Payment Date stay at or
  near zero in most seeds.
- One dataset, one 30-invoice held-out slice, three seeds.
- The human reviewer is a model applying a fixed rubric, not a person. It
  agrees with pre-registered expectations on 10 of 11 cases and is not
  deterministic; its variance is confined to the experience phase, which ends
  before any measurement begins.

## Three engine defects this found

Each produced a plausible null result rather than an error, which is why a
score-only benchmark would never have surfaced them.

1. **Every human note reached the model as an empty string.** `grain_brief`
   rendered an Observation through a fact-triple branch needing
   subject+relation+object; an Observation has no relation, and the fallback
   never checked `object`, where the store puts the text. The single
   highest-value evidence in a memory was the one shape that rendered to
   nothing, so an explicit instruction from a person could never become a
   lesson.
2. **A lesson could only prevent, never start doing something.** The lesson
   contract asked for a rule "preventing a recurring mistake". When an agent
   simply never produces a field, the rule it needs is additive. The model
   reached for the nearest allowed shape — "validate all required fields
   before submission" — which passed DISCOVER, GROUND, VERIFY, human review,
   apply and render, and changed nothing: 0/30 coverage, against 6/6 for the
   same fields under "capture X, Y, Z". **A lesson can clear every gate the
   engine has and still be a no-op, because no gate asks whether the wording
   names an action the agent can take.**
3. **The pin probe could not talk to OpenAI models**, reporting a working
   provider as a bad tag.
