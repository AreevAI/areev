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

## Result

*Not yet run.*
