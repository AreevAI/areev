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

## Results

*Pending the full run. The trial run's only job was to prove the pipeline
end to end on this corpus with real models; its numbers are not evidence.*

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_vrdu_reg.py                                   # 1,321 kept of 1,915
export OPENROUTER_API_KEY=…
PROFILE=vrdu_reg SEED=1 EXP=40  EVAL=20  CKPTS=20           sh curve_tune.sh runs/trial   # the trial
for S in 1 2 3; do PROFILE=vrdu_reg SEED=$S EXP=320 EVAL=100 CKPTS=20,40,80,160 sh curve_tune.sh runs/curve; done
python3 curve_stats.py runs/curve --write
```
