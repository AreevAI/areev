# The tuning learning curve

[`FOURWAY.md`](FOURWAY.md) found that a 1.5B model tuned once on a governed
memory beat the 30B model it was distilled from — and that about half of
that edge was familiarity with vendors it had seen, not learning. That was
one corpus of 34 rows, tuned once, on receipts. This asks what happens to the
tuned model **as the deployment grows**: does quality keep climbing with the
corpus, or level off — and does it matter whether each new model starts from
the plain base or from the last one?

## Pre-registered design (written before the real run)

*Committed after a 40-document trial run and before any full seed.*

### Corpus, and why this one

**VRDU registration forms** (Wang, Zhu et al., KDD 2023): 1,915 real Foreign
Agents Registration Act filings from the US Department of Justice, each with
extracted text and human field annotations. `receipts/build_vrdu_reg.py`
keeps **1,321** — 532 dropped for a filing date the parser cannot read with
certainty (the older scans are OCR noise like *"Subscribed and sworn to
before me at. CW you day of 1925"*), 62 for a missing required field —
across **478 distinct registrants**, filed 1948 to 2023. The clerk's ledger
wants the registration number as digits, the file date as `YYYY-MM-DD` (the
forms print *"July 16, 2008"* and, older, *"23rd day of March 1962"*), and
the registrant and signer exactly as printed.

Two properties no earlier corpus had, and both are the point:

- **A real timeline.** Every form carries the date it was filed. The
  experience stream is sampled across the corpus and then sorted by that
  date, so "over a period of time" is chronology, and the OCR quality that
  varies by era is part of what the deployment meets.
- **An entity key.** The held-out set is drawn from *organisations* the agent
  never learned from (`dataset.split_entity`). That designs the
  memorisation question out of the evaluation instead of checking for it
  afterwards, which is what the receipts result had to do.

### What runs

Three seeds. Per seed, the governed loop runs **320 documents** in filing-date
order, snapshotting its memory at **20, 40, 80, 160 and 320** — a log axis,
because a plateau is invisible on a linear one. At each checkpoint the memory
becomes a corpus (`slm_corpus.py`: the filed rows under the rules as they
stood) and **two adapters**:

| mode | starts from | trains on |
|---|---|---|
| **scratch** | the plain base | everything up to the checkpoint |
| **continual** | the previous checkpoint's continual adapter | only the documents since it |

Each adapter, and the LLM carrying the same checkpoint's rules, reads two
held-out sets of **100 documents**:

| set | drawn from | answers |
|---|---|---|
| **unseen** | registrants never in the stream | does it generalise? — the only number that may claim learning |
| **seen** | the stream's own registrants, other filings | what familiarity buys, at every checkpoint |

The untuned base under the final rules runs once per seed, the control that
separates a small model with rules from a small model tuned on the corpus.

**Base models.** `Qwen3-1.7B` (4-bit) is the primary, with `Qwen3-0.6B` as
the second combination on seed 1. `Qwen3.5-2B` was the first choice and is
not usable here: its LoRA backward pass on these 2–3k-token documents fails
deterministically in the Metal command buffer with *Insufficient Memory*
under `mlx_lm` 0.31.2 and 0.31.3, at batch 1, 1,536 tokens and gradient
checkpointing alike (the trial's first checkpoint, reproduced four times;
the hundred-token smoke test that passed was not a test). Qwen3 trains on
the same corpus cleanly and its template has a real thinking switch, which
the serving shim turns off, so the untuned control answers directly too.

### Overfitting, controlled rather than checked

Reviewers will ask, so the design answers before they do:

1. **Held-out by organisation, not by document.** No unseen-set filing shares
   a registrant with anything in any corpus. The seen set is reported beside
   it so the familiarity gap is a measurement, not a suspicion.
2. **Epochs scale with the corpus.** A fixed 200 steps on 20 rows is twenty
   epochs; `slm_train.sh` trains four epochs, floored at 40 and capped at 400
   iterations.
3. **Validation-selected checkpoints.** Validation loss on rows carved from
   the *experience* set is measured every 25 steps and the checkpoint with
   the lowest is kept — never the last. Best and last are both in the
   manifest and both are published (`curve_stats.py`'s "overfitting
   receipt").
4. **Small adapters.** LoRA on 8 layers, rank as shipped; no full fine-tune.
5. **Every arm pairs.** Scratch against continual, and each against the LLM,
   per (document, field), McNemar exact, on the same held-out documents.

### The overfitting record, as numbers

Every checkpoint adapter leaves the following, pooled over seeds by
`curve_stats.py` into `CURVE.json`, so the paper's overfitting section can
cite measurements rather than assurances:

| metric | what it is | where it comes from |
|---|---|---|
| unseen rate, 95% CI | exact-match on registrants never learned from, Wilson interval | `eval_*_unseen` |
| familiarity gap | seen-set rate minus unseen-set rate | `eval_*_seen`, `eval_*_unseen` |
| **train-set rate** | the adapter read against a seeded sample of its own training rows | `curve_extra.sh` → `eval_*_train` |
| **memorisation gap** | train-set rate minus unseen rate | derived |
| **forgetting** (continual) | train-set rate on rows from *earlier* checkpoints vs the latest delta | `eval_continual_train`, rows keep `seq` |
| validation curve | loss every 25 steps; best vs last; which was kept | `adapter.manifest.json` |
| loss gap at kept step | validation loss minus train loss at the kept checkpoint | manifest |
| effective epochs | iterations × batch ÷ rows | manifest |
| trainable fraction | LoRA parameters as a share of the base | manifest, from `mlx_lm`'s report |
| per-field exact and semantic, unseen | which fields generalise a convention and which are read right but filed wrong | trials |
| noise floor | the final scratch adapter reads the unseen set twice | `eval_scratch_unseen`, arms B and B2 |

*Stated in advance for these:* train-set rate will sit well above unseen at
every checkpoint (a small model on forty rows memorises its rows), and the
question the paper answers is whether the memorisation gap *shrinks* as the
corpus grows — evidence of generalisation replacing recall — or holds
steady. The continual path is expected to show forgetting on old rows by
the third checkpoint. The loss gap at the kept step should stay small and
roughly flat; a widening gap with corpus size would be the signature of
overfitting the selector failed to catch, and it would be published as
such.

### Stated in advance

- **Shape.** Unseen-set quality rises steeply to 40–80 documents and then
  flattens — a log-shaped curve with the ceiling set by OCR noise, not by
  learning. The seen-set curve keeps rising with registrant coverage; the
  gap between the two widening with corpus size is the cleanest evidence of
  where learning stops and familiarity begins.
- **Scratch vs continual.** Continual trails scratch at every checkpoint
  after the first, by a small margin on the unseen set, because it sees each
  document once and the accumulated corpus more than once. If it *matches*
  scratch, the accreting corpus is the cheaper path and that is the finding.
- **The 0.6B** lands below the 1.7B at every checkpoint and plateaus earlier.
- **The LLM with rules** does not improve past 40 documents: rules saturate
  quickly and the ceiling is the model's transcription, not its instructions.
- A curve that keeps rising to 320 with no flattening publishes as exactly
  that, and says the corpus was too small to find the plateau.

**Cost.** The governed phase on long forms is about $2.5 per seed; tuning
and every small-model read are local and free; the LLM reads two held-out
sets at five checkpoints, about $0.40. Roughly **$9 for three seeds** and a
day of unattended machine time.

## Then: DocILE

The scale run. [DocILE](https://github.com/rossumai/docile) (Rossum, 2023):
~6,700 annotated real business invoices with 55 field types, plus ~100k
unlabelled, released for research under registration. Ten times the
documents and ten times the tokens per document of anything here.

Plan, in order, none of it started:

1. **Access.** Registration is per person; the dataset does not download
   from a URL. That is the one step this harness cannot do on its own.
2. **Builder.** `build_docile.py` on the same JSONL shape: text from the
   provided OCR, a filed row from the KILE annotations for a fixed subset of
   fields — invoice id, issue date, due date, total, supplier name, supplier
   address — with the ledger's conventions declared (ISO dates, plain
   amounts, names as printed). Entity key: supplier. Timeline: issue date.
3. **Trial, then real.** The same `curve_tune.sh` with checkpoints at 20, 40,
   80, 160, 320, 640 and 1,280 — the plateau this corpus can actually
   reach — and 200-document held-out sets.
4. **What changes at scale.** The governed phase is where the cost is: DocILE
   invoices are ~10× SROIE's tokens, so 1,280 documents of governed
   experience is on the order of **$25–40 per seed**. The LIMIT cap is
   already lifted (`memory.py` reads by subject and relation); at 1,280
   documents the per-relation reads stay under it.
5. **The question DocILE can answer that this one cannot.** With 55 field
   types and a supplier set in the thousands, whether the tuned model's
   unseen-supplier curve keeps rising past a few hundred documents — or
   whether the plateau found on 320 registration forms is the plateau.

### Amendment after the first real checkpoint (disclosed, not hidden)

Seed 1's first real checkpoint, 20 documents against 100-document sets,
read: LLM with rules **86% / 85%** (unseen / seen), tuned 1.7B **47% /
58%** — the opposite of the trial. The loop had approved five rules by
document 8, so the LLM was strong; and the tuned model had trained on **10
rows**, because `slm_corpus.py` emitted a row only for documents the
accountant had *corrected*. Once the rules made the agent right most of the
time, the corpus shrank to the hard, odd cases — a biased, undersized
training set, not the deployment's filed rows. A ledger has a row for every
document it processed. The corpus is now every experience document with
its filed row (the builder's truth under the accountant's corrections), so
the corpus-size axis is exactly the document count. Seed 1's checkpoints
were discarded and re-run under the one rule; every seed uses it. The
trial's numbers above were under the old rule and are left as they were.

### Observation after the corrected checkpoint: the checkpoints are eras

Re-run on every filed row (16 of 20), seed 1's checkpoint 20 read **47%
again** on unseen registrants — so the corpus selection was not the cause.
The per-field split located it: File Date **1 of 100**, and every miss a
century — *1917-04-18* for 2017, *1908-11-25* for 2008, *1918-01-31* for
2018. The stream is sorted by filing date, so seed 1's first 20 documents
are filed **1963–1991** (median 1985), its first 160 still have a median of
1994, and the held-out unseen set has a median of **2015**. A 1.7B tuned on
forms from the 1960s reads a two-digit year as 19xx. The trial did not show
this because its 40-document stream happened to start in 1983 (its first
20: median 2002) against a 2008–2018 held-out set. The LLM, which reads
dates natively, is unaffected: 86% on the same set.

The pre-registered design stands and its predictions are not touched, but
their **mechanism** is now stated before the results: on a chronological
stream the checkpoint axis is *time*, and the fixed all-era held-out sets
measure how much of the eventual distribution a checkpoint's corpus
covers. A curve that rises to 160 and beyond is era coverage arriving, not
capacity — and it is the drift result of [`DRIFT.md`](DRIFT.md) surfacing on
a real corpus without being staged.

One leg is added, post hoc and disclosed here before it runs
(`curve_next.sh`): at every checkpoint, both adapters and the LLM carrying
that checkpoint's rules read the **next 20 stream documents** — what the
deployment met next, unseen by any adapter at that checkpoint and of its
own era. That is the prequential curve, the honest time axis. The governed
agent's own record over the same window is already in its journal (rules as
they evolved inside the window) and is reported beside it; seed 1's, after
checkpoint 20, is 90%. *Stated in advance:* on the next window the tuned
model tracks the LLM closely at every checkpoint, because the era matches,
and the gap between its next-window rate and its all-era rate is the
measure of how far the deployment's past is from its future.

Caught by the same checkpoint: two reads of one adapter — checkpoint 20's
continual adapter is a copy of scratch — paired as 72 wins to 66 losses on
the unseen set while their outputs differed on **9 of 386** trials. The
unseen set's *order* varied per process: `split_entity` iterated the
held-out registrants as a Python set, whose order follows the per-process
hash seed, before shuffling and cutting the pool. Verified after the fix:
the experience stream is unchanged (320 of 320 documents match both seeds'
journals), the seen set is unchanged, and both pre-fix unseen reads covered
exactly the fixed set's 100 documents — so every rate stands and nothing
was re-run; only position-keyed pairing was wrong, and `curve_stats.py` now
pairs by document. The trial's paired counts below are recomputed that way
(the same two reads pair as 3 wins to 0). The seen-set pairing was never
affected, and neither was any earlier corpus: only this profile splits by
entity.

Seed 1's checkpoint 40 raised a second question, and it is answered by
measurement rather than by argument. On the unseen set the scratch adapter
read 45% and the continual one **64%** — continual over scratch 78 wins to
3 — the reverse of the prediction above. The training receipts say why:
the scratch adapter's validation selector kept **iteration 25 of 72** (loss
0.030 there against 0.038 at the end, on a **four-row** validation set),
about 1.4 epochs, while the continual adapter carries its 40 iterations
from checkpoint 20 plus 40 more. A four-row validation set cannot tell
0.030 from 0.038; the selector may be under-training the scratch path and
the headline scratch-vs-continual comparison would then be measuring the
selector. So one more post-hoc leg, disclosed before it runs
(`curve_kept.sh`): wherever the selector kept an earlier checkpoint than
the last one saved, the latest saved checkpoint reads the unseen set too,
and kept-vs-latest is reported per checkpoint (the keep step overwrote the
final iteration's weights with the kept ones, so "latest" is the last
numbered checkpoint — iteration 50 of 72 here — and the record says so).
*Stated in advance:* the latest checkpoint matches or beats the kept one at
every checkpoint where they differ; if it does, the selector on a four-row
set is reported as a method that hurt, and the scratch curve is read from
both.

Two runs share one laptop GPU here (seed 1's re-run beside the main
queue), and a local read under a concurrent training stretched from about
2 seconds a call to 15, once past the harness's 180-second cap (seed 2,
checkpoint 80, one document scored as no output; the summary records
`failed_calls`). `gpu_yield.sh` now stops training while any local server
is up, and the cap is 600 seconds. **Local latencies from this study are
not evidence for the speed claim** — [`FOURWAY.md`](FOURWAY.md) measured
them uncontended, and that is the number the paper uses.

### Observation at checkpoint 160: the gate approved a contradiction, and nothing measured it

Seed 1's LLM with the checkpoint-160 rules read **66%** on unseen
registrants, down from 86% at 80 — File Date 82 → **13** of 100. The
sixth rule, approved after document 120, says *"Copy the registrant name
and file date exactly as printed … the original date format, before any
standardization"*; the fourth says *"Standardize all dates as
YYYY-MM-DD"*. The reviewer approved the contradiction from its text, the
agent resolved it by filing dates as printed, and the deployment's own
record shows the damage as it happened: File Date correct on 69 of 79
documents before the rule, then **8 of 40, 4 of 40 and 11 of 120** after
it, for 200 documents, with no revert. Seed 2 approved a similar
*"exactly as printed"* rule for the file date at document 85 and slipped
from 70 of 79 to 54–60 of 80 — milder, because its LLM resolved the same
contradiction the other way more often.

Why the loop's Verify gate was silent: it compares a lesson's metric
against a baseline, and the only evalset run this harness journaled was the
day-one baseline (26% on the unseen set), before any rule. The reads at
each checkpoint were taken against snapshots and never journaled back, so
outcome review had nothing after the apply to compare — and had it been
fed only the final read, 66% against 26% is *held*. A gate that measures
against the start of the deployment cannot see a rule that costs twenty
points at document 120. The tuned model, meanwhile, learned from the
**filed rows** — ISO dates, whatever the rule text says — and read 93% at
the same checkpoint, on the same registrants, with the same six rules in
its system prompt. Distillation from the ledger's rows is robust to a bad
rule in a way the prompt is not; that is a result, and it was not
predicted.

One more post-hoc leg follows from this, disclosed before it runs
(`curve_verify.sh`): the checkpoint reads are journaled into the final
ledger as evalset runs on the deployment's own timeline — the 86% before
rule 6's apply, the 66% after — and a loop pass then records its verdicts.
*Stated in advance:* with a measurement on each side of the apply, outcome
review records `regressed` for rule 6 and proposes its revert, and applying
the revert returns the LLM to its checkpoint-80 reading. Reading the engine
settled the second half before the leg ran: the verdict compared against
the run the *proposal* froze — day one, here — so it would have said
`held`. That is fixed in the engine (`197a665`: the baseline is the newest
run journaled before the *apply* when one exists; identical to before when
nothing was journaled in between, which is every verdict any published run
recorded), with a test that replays this case, and the leg runs on that
engine from a separate build so nothing changes under the live seeds.
Verdicts are per lesson: rules 1–5 were applied by document 8, before any
checkpoint read, so their baseline stays day one and they read `held`; rule
6 is judged against checkpoint 80.

Two trainings at once were the next failure. Seed 1's final scratch
adapter (316 rows) was training beside seed 2's checkpoint-160 adapter, and
its first attempt failed in Metal with *Insufficient Memory* on a 36 GB
laptop; `slm_train.sh`'s retry (batch 1, 2,048-token sequences, gradient
checkpointing) ran to 400 iterations with the loss **NaN from iteration
40**, and the selector, seeing one finite validation point, kept iteration
25 — 0.16 of an epoch. The likely mechanism: the retry shortens sequences
below some documents' length, and with the prompt masked a document that
does not fit leaves no completion token to score. The adapter was
discarded and is retrained with trainings serialised (`gpu_yield.sh`: only
the oldest training runs, and none while a local server is up); the
retry's sequence length is a fix for when no training invocation is live.
No published number comes from that adapter.

Seed 3's first attempt died at document 26: the learner's provider
rate-limited (HTTP 429) past the eight retries the model script makes,
and a learn pass that cannot reach its model was fatal to the run, where
an agent call in the same state parks the document and goes on. A learn
pass now fails the same way the agent does — skipped, counted
(`learn_failures` in the experience summary, a `failed` line in the
journal), and triggered again at the next correction, with the evidence
still in the memory. Seed 3 was rerun from the start on that harness; its
first 26 documents are not part of any number here.

Seed 3 stopped once more, at checkpoint 160, on a safety guard: the corpus
builder asserts that no held-out document enters a training corpus, and it
keyed that check on the document id — the filename cut to 40 characters,
which a few filings share. A stream document that merely shared a name
with a held-out one tripped it. The guard now compares document text; the
split itself partitions by registrant and had not leaked anything. Seed 3
resumed from its persisted checkpoints (`curve_resume.sh`); no number was
affected.

Caught before it ran: the train-set read passed its sample size as the
split's held-out size, and in `split_entity` that size decides which
registrants are held out and therefore which documents form the stream — a
40-row "training rows" read would have sampled another deployment's
documents. The sample size is now a separate parameter (`--rows`), and the
split is byte-identical for every published call.

### Added after the results: the baselines the four-way chart had

Read against the published curve, the chart lacked the two lines every
earlier study drew — an agent with **no memory** and one with **mem0** —
so both are added, on the same unseen sets, and the expectation is
written down before the mem0 numbers exist. *No memory* is the day-one
agent (the one-field instruction, no rules): it was read once per seed at
the start of the run and pairs with every checkpoint; it is flat by
construction, at 25%. *mem0* is the [`FOURWAY.md`](FOURWAY.md) arm on
this corpus: the same agent, the same accountant, `add()` after every
corrected document and `search()` before the next, in its domain-prompted
and as-installed modes, read at the same five checkpoints. *Stated in
advance:* on receipts mem0 never got the agent to attempt a second field
and stayed within ten points of no memory; here the accountant's messages
name the missing fields explicitly, so mem0 should lift the agent above
25% — and stay well below the governed LLM's 84% at every checkpoint and
below every tuned reading from 160 on, because retrieval puts a handful
of past corrections in the prompt while a rule states the convention once
for every document.

## The trial, and what it caught

Forty documents, one checkpoint at 20, held-out sets of 20, seed 1. Its
job was to prove the pipeline on this corpus with real models before any
full seed ran, and it earned its keep three times over before it passed:

- The trainer ran through a `tee` pipe, so a Metal crash after the first
  validation exited 0 and an adapter directory **with no weights** was
  evaluated as a tuned model. The trainer now asserts the weights and the
  evaluator refuses their absence.
- `Qwen3.5-2B`, the intended base, cannot train on these documents on this
  machine (above). `Qwen3-1.7B` replaced it.
- Every rule the loop proposed said *"on every receipt"* — the memory's
  capture entity was hard-coded for receipts, so the evidence told the
  proposer what it was reading. The reviewer, told these were forms, refused
  all seven. The entity now derives from the profile; the corrected trial
  approved *"Record the signer name on every registration form…"*.

Passed clean: exit 0, no failed calls, $0.03. Its numbers are a pipeline
check and not evidence, but the shape is worth recording as the thing the
full run will test: on unseen registrants the tuned 1.7B scored 92% at 20
documents and 98% at 40 against the LLM's 48% and 49% (paired by document,
38 wins to 3 and 27 to 1); on seen registrants
the continual path fell below scratch at 40 (75% against 87%, 12 wins to 2)
— the divergence the pre-registration predicts, on twenty documents. The
validation-selected checkpoint fired for real at 40 (loss 0.088 at step 50
against 0.094 at the end; step 50 kept). And one adapter read twice
differed on 9 of 79 trials: the local model's noise floor is not zero, so
the full run measures it.

## Results

Three seeds of the 1.7B, one of the 0.6B, every leg complete;
`results/curve-vrdu-2026-09-06/` holds the recomputed numbers
(`CURVE.json`, `CURVE-06b.json`), every adapter's training receipt, the
verify verdicts and the metered cost. Exact-match rate on 100-document
sets, 300 documents per checkpoint pooled over three seeds; wins/losses
are McNemar pairs against the LLM carrying the same checkpoint's rules.

![The learning curve on registrants the agent never learned from](../../docs/assets/curve-unseen-light.svg#gh-light-mode-only)
![The learning curve on registrants the agent never learned from](../../docs/assets/curve-unseen-dark.svg#gh-dark-mode-only)

### Unseen registrants — the number that may claim learning

| documents learned from | LLM + rules | 1.7B tuned from scratch | 1.7B tuned continually |
|---:|---:|---:|---:|
| 20 | 84% | 55% (38/376) | 54% (37/378) |
| 40 | 85% | 58% (41/353) | 56% (35/368) |
| 80 | 84% | 70% (54/212) | 61% (50/319) |
| 160 | 78% | **90%** (195/64) | 87% (201/104) |
| 320 | 79% | **93%** (202/47) | 91% (201/69) |
| untuned 1.7B, final rules | | 17% | |

Per field, from scratch, 20 → 320 documents: File Date 62 → 285 of 300,
Registrant Name 190 → 282, Registration Number 254 → 291, Signer Name 128
→ 216 of 260. The seen set runs 5–14 points above the unseen set until
160 and 2 points above at 320 (`curve.stdout`).

### Against what was stated in advance

- **Shape — partly wrong, and the mechanism was found before the numbers
  landed.** Predicted: a steep rise to 40–80 documents, then flat.
  Observed: flat to 40 (55 → 58), the rise between 80 and 160 (70 → 90),
  then a plateau (90 → 93). The stream is chronological and the held-out
  sets are drawn across the corpus's whole span, so the curve is the
  deployment's *era coverage* arriving — the first 80 documents are filed
  before 1995 and the tuned model reads a two-digit year as 19xx (File
  Date 62 of 300 at 20 documents) until the corpus reaches the era the
  held-out forms come from. The prequential read settles what the fixed
  sets could not: on the next 20 documents of its own stream the tuned
  model matches the LLM from the first checkpoint (89% against 89%, 84–88%
  against 87%, 79–80% against 84%) and beats it at 160 (92% against 78%,
  41 wins to 10). It was competent on its present all along; the fixed
  sets measured how far its past was from its future.
- **Scratch vs continual — as predicted.** Continual trails scratch at
  every checkpoint after the first: 56 vs 58, 61 vs 70, 87 vs 90, 91 vs
  93, significant from 80 on (145/34, 74/40, 49/26). The accumulating
  corpus is the better path here; the continual path's own training rows
  from earlier checkpoints read 82% at 320 against 95% for scratch, which
  is forgetting, and it arrived at the last checkpoint rather than the
  third.
- **The 0.6B — falsified.** Predicted below the 1.7B at every checkpoint
  and an earlier plateau. Observed (one seed): level with it to 80 (55 /
  53 / 55), **91% at 160 and 95% at 320** on unseen registrants — above
  the 1.7B's 93% pooled — from adapters that trained in a third of the
  time (30 minutes against 95 for the final one). Its untuned control
  reads 8%. One seed, and the run's own governed phase approved weaker
  rules (its LLM reads 38 → 69%), so the tuned model's lead over its LLM
  is larger than the 1.7B's; the tuned rates themselves are what was
  predicted wrong.
- **The LLM with rules — as predicted, and then some.** It never improves
  past 40 documents (84 / 85 / 84) and *falls* to 78–79% at 160 and 320,
  because seed 1 approved a rule that contradicted an earlier one (below).
  Seeds 2 and 3 alone are flat at 77 → 79% and 89 → 92%.

### The overfitting record

| documents | mode | unseen [95% CI] | seen | familiarity gap | train-set | memorisation gap | old rows (forgetting) | loss gap at kept | eff. epochs |
|---:|---|---|---:|---:|---:|---:|---:|---:|---:|
| 20 | scratch | 55% [52–57] | 64% | +10 | 97% | +42 | — | 0.056 | 5.0 |
| 40 | scratch | 58% [55–61] | 69% | +10 | 95% | +36 | 94% | 0.025 | 4.0 |
| 80 | scratch | 70% [68–73] | 78% | +8 | 98% | +28 | 99% | 0.113 | 4.0 |
| 160 | scratch | 90% [88–91] | 93% | +3 | 96% | +6 | 97% | 0.052 | 4.0 |
| 320 | scratch | 93% [91–94] | 95% | +2 | 96% | +3 | 95% | 0.001 | 2.5 |
| 320 | continual | 91% [89–92] | 90% | −1 | 90% | −0 | 82% | 0.010 | 4.0 |

The question stated in advance was whether the memorisation gap shrinks
as the corpus grows. It does: **+42 points at 20 documents, +3 at 320**,
with the familiarity gap going +10 → +2 alongside it — recall replaced by
generalisation, not a model that learned its rows. The loss gap at the
kept step stays small and does not widen with corpus size. The noise
floor is zero: each final scratch adapter read the unseen set twice and
the two reads differed on **0 of 386** outputs on every seed. And the
validation selector on a four-row set is a coin flip, as suspected once
seed 1's checkpoint 40 was read: latest-vs-kept helped once (80: 70% kept
against 69% latest, 22/43), hurt twice (40: 45 → 55%, 62/24; 160
continual: 83 → 86%, 29/6), and tied at 320. Both readings are published;
the curve above is the kept one.

### The verify leg — what the gate did with its own reads

| seed | rules applied | verdicts | reverts | rules after | LLM before → after the reverts |
|---:|---:|---|---:|---:|---|
| 1 | 6 | 5 held, **1 regressed** (330 → 260 against checkpoint 80) | 1 | 5 | 260 → **332** of 386 |
| 2 | 9 | 9 held | 0 | 9 | — |
| 3 | 10 | 8 held, 2 regressed (359 → 355) | 2 | 8 | 355 → 350 of 387 |

Seed 1 is the case the study was built to find: a rule approved from its
text that contradicted an earlier one, cost twenty points, and was
invisible to a gate that measured against day one. Fed the deployment's
own checkpoint reads on its own timeline, the fixed engine records
`regressed`, proposes the revert, and the LLM re-read under the remaining
five rules recovers to 86% — the checkpoint-80 level. Seed 3 is the limit
on the other side: two rules "regressed" by four trials of 387, both were
reverted, and the re-read moved nothing (one document of it lost to a
provider error). The verdict has no noise floor, and this run measures
what that costs: two unnecessary reverts on a dip smaller than one
adapter's read-to-read variance elsewhere in this study. Two engine
changes came out of this leg and are in the CHANGELOG: the baseline for a
verdict is the newest run before the *apply* (`197a665`), and a revert is
keyed by the recommendation it reverts, so two regressed lessons on one
entity get two reverts instead of one (`f8df2d2`, found on seed 3).

### The governed phases themselves

| run | live exact | rules applied | rejected | LLM on unseen, first → last checkpoint |
|---|---:|---:|---:|---|
| seed 1 | 889 / 1221 (73%) | 6 | 46 | 86% → 67% |
| seed 2 | 998 / 1220 (82%) | 9 | 45 | 77% → 79% |
| seed 3 | 1065 / 1223 (87%) | 10 | 17 | 89% → 92% |
| 0.6B run (seed 1 again) | 771 / 1221 (63%) | 4 | 26 | 38% → 69% |

The same seed, split and models, run twice (seed 1 and the 0.6B run's
governed phase), approved six rules once and four the other time and
ended twenty points apart on the LLM; the provider's routing is not
deterministic under a seed, and the loop amplifies that into different
rule sets. The tuned models did not care: 94% and 95% at 320 on the same
unseen set, from filed rows that do not depend on which rules were
approved.

### Cost

All API calls for the three 1.7B seeds — governed phases, every LLM
held-out read at every checkpoint, the prequential and verify reads —
metered from journaled tokens: **$0.66** (15,700 calls, 4,797 of them the
agent's). The 0.6B run: $0.23. Tuning and every small-model read were
local; the 1.7B's final adapter trained in 95 minutes uncontended, the
0.6B's in 30. The pre-registered estimate was $9; the agent model's list
price is a fifth of what that assumed.

### What this study does not show

Local latency (two runs shared the GPU; `FOURWAY.md` measured it
uncontended). Whether the plateau at 320 is capacity or the corpus's
last era — the held-out set's median filing year is 2015 and the stream
reaches 2018, so what remains is the question DocILE was planned for.
The 0.6B result is one seed. And the LLM's decline is one seed's loop
approving one bad rule: seeds 2 and 3 show the same LLM flat, not
falling.

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_vrdu_reg.py                                   # 1,321 kept of 1,915
export OPENROUTER_API_KEY=…
nohup sh gpu_yield.sh > gpu_yield.log 2>&1 &                # if two runs may share the GPU
PROFILE=vrdu_reg SEED=1 EXP=40  EVAL=20  CKPTS=20           sh curve_tune.sh runs/trial   # the trial
for S in 1 2 3; do
  PROFILE=vrdu_reg SEED=$S EXP=320 EVAL=100 CKPTS=20,40,80,160 sh curve_tune.sh runs/curve
  for leg in "curve_extra.sh runs/curve/seed$S 40" "curve_next.sh runs/curve/seed$S 20" \
             "curve_kept.sh runs/curve/seed$S" "curve_verify.sh runs/curve/seed$S"; do
    PROFILE=vrdu_reg SEED=$S EXP=320 EVAL=100 sh $leg     # the post-hoc legs, in this order
  done
done
PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 CKPTS=20,40,80,160 SLM_BASE=mlx-community/Qwen3-0.6B-4bit sh curve_tune.sh runs/curve-08b
sh publish_curve.sh runs/curve runs/curve-08b ../results/curve-vrdu-$(date +%F)
```

`curve_resume.sh` picks a seed up from its persisted checkpoints after a
crash (it skips every adapter and read that exists). `curve_verify.sh`
runs under a separate build of the binding (`VERIFY_PY`) so an engine
change is never swapped under a live seed. Never edit a driver or
`slm_eval.sh` while an invocation may be running: `sh` reads a script
incrementally, and an edit mid-run cost this study one seed's checkpoint
(`curve_resume.sh` records the incident).
