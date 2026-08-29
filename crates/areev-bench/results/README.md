# areev-bench — committed results

Machine-readable, auditable results from two benchmark families: the `accuracy`
(LoCoMo) run, and the `selfimprove_aba` A/B/A/B self-improvement runs. Both
publish their raw evidence, not just their tables.

## `accuracy` — LoCoMo

Each run is two files:

- `*.summary.json` — config (reader/judge/embedder), overall answer accuracy,
  retrieval hit-rate, per-category breakdown.
- `*.transcripts.jsonl` — one row per question: `{category, category_name,
  correct, question, gold, answer, verdict}`. This is the raw evidence — every
  answer and every judge verdict — so the number can be independently audited
  (the category has a history of unreproducible claims; we publish the receipts).

### Runs — best configuration

| file stem | benchmark | reader / judge | embedder | k | answer acc | hit@10 / hit@20 |
|---|---|---|---|---|---|---|
| `locomo-gpt-4o-mini-k20-2026-07-07` | LoCoMo (1,982 QAs) | gpt-4o-mini / gpt-4o | text-embedding-3-small@512 | 20 | **54.2%** | **74.5% / 81.6%** |

Raw turns, real embeddings, k=20 — the winning config. (Explored and dropped
because they didn't help this benchmark: distilled-observation ingest, MMR /
rerank / query-expansion refinements, and a stronger gpt-4o reader — see
`../RESULTS.md` §4 for why the bottleneck is reader synthesis, not retrieval.)

## `selfimprove_aba` — the A/B/A/B causal proof + passive-memory arms

| directory | seeds | scale | states | spend |
|---|---|---|---|---|
| `selfimprove-3seed-qwen3-30b-2026-08-26` | 1, 2, 3 | 300 experience · 100 held-out | A0/B/A1/B2 + m-steel/m-all/m-llm | $2.30 |

Per seed: `seedN.report.json` (config + governance ledger + per-state
summaries), `seedN.report.md`, and one `seedN.transcripts-eval-<STATE>.jsonl`
per state plus `seedN.transcripts-experience.jsonl` — every model call, model
and provider id, and per-call usage. `paired-stats.txt` is `aba_stats.py`'s
output over all three; `MANIFEST.md` records each run's exact command, git rev,
and a SHA-256 of every file.

Audit the published table against its own raw rows — keyless, offline:

```bash
python3 crates/areev-bench/scripts/verify_run.py \
  crates/areev-bench/results/selfimprove-3seed-qwen3-30b-2026-08-26
```

That recomputes every per-state and per-rule number from the `task_outcome`
rows, checks all states ran the same task ids (the paired test depends on it),
and re-derives the manifest checksums. CI runs it on every push.

Read `../SELFIMPROVE.md` for the design, the honesty rules, what a re-run
costs, and the known even/odd seed collision that makes this three runs over
**two** distinct task sets.

## Methodology — LoCoMo

Plain retrieve-then-read, no LoCoMo-specific tuning. Each conversation turn is
ingested as a grain; each question drives `recall_hybrid` for the top-20 turns;
the reader answers from those turns (session dates included so relative time
resolves to absolute dates — LoCoMo's temporal category requires this); an LLM
judge grades the answer against gold. hit@k counts a gold-evidence turn in the
top-k.

The number depends on the reader/judge models and retrieval quality, **not on
the store alone** — it is a full-pipeline number, published with its config and
transcripts. The LoCoMo answer key itself is ~6% wrong
([audit](https://dev.to/penfieldlabs)), so treat single-point comparisons across
vendors with suspicion.

## Reproduce — LoCoMo

```bash
# 1. dataset
curl -sSL -o locomo10.json \
  https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json

# 2. precompute real embeddings (or skip → TF-IDF floor, 40.7% hit@10)
export OPENAI_API_KEY=sk-...
python3 crates/areev-bench/scripts/embed_locomo.py locomo10.json cache.json 512

# 3. run the best config (raw turns, k=20), logging transcripts
AREEV_EMBED_CACHE=cache.json AREEV_TOPK=20 AREEV_LLM_DEBUG=1 \
AREEV_LLM_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o-mini' \
AREEV_JUDGE_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o' \
  cargo run --release -p areev-bench --bin accuracy -- locomo10.json 10 > run.log 2>&1

# 4. canonicalize into results/
python3 crates/areev-bench/scripts/parse_results.py \
  run.log crates/areev-bench/results/<stem> gpt-4o-mini gpt-4o \
  openai/text-embedding-3-small@512 <date>
```

Retrieval-only (no LLM, no key) is the same command minus `AREEV_LLM_CMD`.
Full run cost/time on 1,982 QAs ≈ $0.85 / ~50 min. See `../RESULTS.md` for the
latency, trust, and honesty-metric benchmarks.
