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
documents and 98% at 40 against the LLM's 48% and 49%; on seen registrants
the continual path fell below scratch at 40 (75% against 87%, 12 wins to 2)
— the divergence the pre-registration predicts, on twenty documents. The
validation-selected checkpoint fired for real at 40 (loss 0.088 at step 50
against 0.094 at the end; step 50 kept). And one adapter read twice
differed on 9 of 79 trials: the local model's noise floor is not zero, so
the full run measures it.

## Results

*Pending the full run.*

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_vrdu_reg.py                                   # 1,321 kept of 1,915
export OPENROUTER_API_KEY=…
PROFILE=vrdu_reg SEED=1 EXP=40  EVAL=20  CKPTS=20           sh curve_tune.sh runs/trial   # the trial
for S in 1 2 3; do PROFILE=vrdu_reg SEED=$S EXP=320 EVAL=100 CKPTS=20,40,80,160 sh curve_tune.sh runs/curve; done
python3 curve_stats.py runs/curve --write
```
