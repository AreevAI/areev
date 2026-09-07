# authoring-rate-2026-09-04 — which proposer authors a lesson at all

The measurement that chose the learner configuration for the receipts run.
Design, decision rule and full reading: [`../../SELFIMPROVE.md`](../../SELFIMPROVE.md),
"The authoring-rate instrument" and "Outcome (2026-09-04)".

**One captured experience, eight configurations, ten passes each.** Seed 1,
300 experience tasks against the support-desk environment, agent
`qwen/qwen3-30b-a3b-instruct-2507` pinned `coreweave/bf16` at temperature 0
(1,533 tool calls, 276 of them errors). Every pass copies that memory,
runs one governed loop pass through the real engine, and records what came
out. GROUND is `openai/gpt-4o-mini` (openai) in all eight cells, so no
proposer grades itself. The loop legs take `--seed {pass}`, so ten passes
are ten draws.

Two files per cell:

- `<objective>-<model>.jsonl` — one row per pass: the DISCOVER funnel stage
  by stage, every LLM finding with the reviewer's disposition, what each
  origin applied, and the wall time.
- `<objective>-<model>.summary.json` — authoring rate, mean stored per pass
  and funnel totals, derived from those rows and nothing else.

Recompute any published figure from the rows:

```bash
for f in crates/areev-bench/results/authoring-rate-2026-09-04/*.summary.json; do
  python3 -c "
import json,sys; d=json.load(open(sys.argv[1])); t=d['funnel_totals']
print(f\"{d['label']:30} {d['passes_with_llm_finding']}/{d['passes']} \"
      f\"proposed {t['proposed']} cited {t['cited']} grounded {t['grounded']} kept {t['kept']}\")" "$f"
done
```

**What this is not.** No lesson here is scored on held-out tasks — nothing
in this directory is a learning claim. It answers only "does the proposer
produce something to govern, and where do its drafts die", which is the
question the 2x2 (`../selfimprove-2x2-qwen3-30b-2026-08-30/`) could not
answer for itself and which decides what the paid run spends on.

**Spend is not reported.** The loop adapter returns no usage, and the
account-level delta that would have bounded these 80 passes was not read
before the first one. Rather than publish a figure nobody measured, this
says so; the per-model prices are in `../../SELFIMPROVE.md` and the
receipts run that followed cost a measured $0.26.
