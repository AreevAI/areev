# Governed self-improvement on public ad-buy invoices

The second corpus. [`RECEIPTS.md`](RECEIPTS.md) measured the same claim on
ICDAR-SROIE retail receipts and found it twice over — a learner that made an
agent substantially better under human approval, and a gate that caught a
human-approved rule which had made it worse. One corpus is one corpus, so
this repeats the experiment somewhere deliberately unlike it.

## The corpus

**VRDU ad-buy forms**, the DeepForm collection (Wang, Zhu et al., *VRDU: A
Benchmark for Visually-rich Document Understanding*, KDD 2023,
[arXiv:2211.15421](https://arxiv.org/abs/2211.15421)): real political
advertising invoices filed with the US Federal Communications Commission,
each shipping the document's extracted text and human field annotations.
Fetched by `receipts/build_vrdu.py` from
[`google-research-datasets/vrdu`](https://github.com/google-research-datasets/vrdu)
as one gzipped JSONL, stdlib only and cached. Nothing from it is committed
here; only counts travel.

Of 641 records, **530 are kept** — 28 dropped for missing one of the three
always-required fields, 82 for a flight date the builder will not
disambiguate, 1 for an unparseable amount. Field coverage in what remains:
Gross Amount, Advertiser and Contract Number on all 530; Flight From on 437
and Flight To on 435.

## Why this corpus, specifically

It differs from SROIE on every axis that could otherwise confound a
replication:

| | receipts (SROIE) | ad-buy (VRDU) |
|---|---|---|
| document | Malaysian till receipt | US broadcast advertising contract |
| length | ~500 characters | **~5,200** median, 16,600 at p90 |
| day-one field | Invoice Date | **Gross Amount** |
| fields learned | vendor, amount, address | advertiser, contract number, two flight dates |
| date convention | `DD/MM/YYYY` | **`YYYY-MM-DD`** |
| amount convention | plain, two decimals | plain, two decimals, **no `$`, no separators** |

The date convention is the deliberate one. In run 1 of the receipts
experiment the loop authored *"convert extracted Invoice Date to ISO
8601"*, four gates passed it, and it took that agent from 30 correct to 0
because the receipts ledger wants day-first. **On this corpus that same rule
is correct.** So an agent that learns the convention from this business's
own corrections gets both right, and a model applying a general prior about
how dates should look gets exactly one right. That is a property no single
corpus can test.

## Pre-registered design (written before any paid run)

*Committed before a single paid invoice is read.*

**Scale.** Seeds 1, 2, 3. Per seed: 40 experience invoices with a learn pass
every 2 corrected invoices and a snapshot every 10, and 60 held out. Seeds
permute the corpus, so the three runs are three task sets.

**Legs, all pinned, temperature 0, request seed = the run seed** — the same
configuration receipts run 2 used, so a difference between the two results
is the corpus and not the setup:

| leg | model | pin |
|---|---|---|
| capture agent | `qwen/qwen3-30b-a3b-instruct-2507` | `coreweave/bf16` |
| learner (DISCOVER/VERIFY) | `qwen/qwen3-30b-a3b-instruct-2507` | `coreweave/bf16` |
| GROUND | `openai/gpt-4o-mini` | `openai` |
| rule reviewer | `openai/gpt-4o` | `openai` |

`discover_objective: learner`, `evidence_attribution: named`,
`outcome_evalset` naming the held-out set.

**Primary.** Exact match, paired over (invoice, field) by McNemar's exact
test, B against A, pooled and per seed, with the B-vs-B2 noise floor
reported beside it.

**Secondary**, pre-declared: the semantic metric; per-field coverage under
each arm; the learning curve at 0/10/20/30; the verify-then-revert leg; and
— specific to this corpus — **whether the loop authors an ISO date rule
here**, which is the same proposal that was wrong on receipts and is right
here.

**Stated in advance.** A null publishes as a null beside the receipts
result and would say the receipts finding does not generalise past one
corpus. A regression publishes with its per-rule verdicts and reverts, as
run 1's did. The reviewer is a model on the fixed rubric in
`receipts/accountant.py`, not a person, and its rejections are published
with their reasons.

## Result — seed 1 (seeds 2 and 3 running)

**It replicates, and more strongly than on receipts.** 60 held-out ad-buy
invoices, 285 scored (invoice, field) trials:

| | arm A — rules rolled back | arm B — rules applied |
|---|:---:|:---:|
| exact | 45 (15.8%) | **224 (78.6%)** |
| paired | | **179 wins, 0 losses**, p < 0.0001 |

The B-vs-B2 noise floor is 5 discordant trials of 285. Per field:

| field | A | B |
|---|:---:|:---:|
| **Gross Amount** (day one) | **45/60** | **57/60** |
| Advertiser | 0/60 | 51/60 |
| Contract Number | 0/60 | 28/60 |
| Flight From | 0/53 | 43/53 |
| Flight To | 0/52 | 45/52 |

### The two things receipts could not show

**The day-one field improved.** [`RECEIPTS.md`](RECEIPTS.md) records a
limitation honestly: every one of its 286 wins fell on a field the baseline
left blank, and on Invoice Date — the field the agent already captured on
every receipt — the rules won *nothing*. The supported claim there was
narrow: the loop taught the agent *which* fields to capture, not how to do
better at one it was already doing.

Here Gross Amount goes **45/60 → 57/60**. The agent was already filling it
in both arms; what changed is that the loop learned the amount convention —

> "Format the Gross Amount as a plain number with two decimals, no dollar
> sign and no thousands separator, like 27900.00."

— and applied it to a field it had never got wrong for want of trying. That
is the claim the receipts corpus could not support, and it holds here.

**The date rule is right this time, and it is the same rule that was
catastrophically wrong before.** Receipts run 1's loop authored *"Convert
extracted Invoice Date to ISO 8601 (YYYY-MM-DD)"*, four gates passed it, and
it took that agent from 30 correct to 0 because that ledger files day-first.
On this corpus the loop wrote

> "Extract and record both Flight From and Flight To dates from every
> invoice using the exact YYYY-MM-DD format as provided."

and it is correct: a filed `2020-05-26` against a produced `2020-05-26`,
43 of 53 and 45 of 52 exact. Same rule, opposite corpora, right both times
*because it came from each business's own corrections rather than from a
prior about how dates should look*. A model applying a general convention
gets one of these two corpora right by luck. This is the property no single
corpus can test, and it is why this one was chosen.

### What is not better here

Contract Number is the weak field at 28/60, roughly half of what the other
learned fields reach. The agent finds a contract number and files a
different one — these invoices carry several identifiers, and nothing in
the corrections disambiguates which the ledger means. That is a limitation
of the task as posed, not of the loop, and it is reported because 78.6% is
an average over one field the agent half-gets and three it mostly does.

*Seeds 2 and 3 are running and will be published here whatever they show.*
