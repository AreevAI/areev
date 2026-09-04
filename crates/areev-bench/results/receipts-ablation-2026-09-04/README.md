# receipts-ablation-2026-09-04 — which change earned run 2's gain

The 2×2 pre-registered in [`../../RECEIPTS.md`](../../RECEIPTS.md), which
separates the two things receipts run 2 changed at once: the evidence
projection naming an Observation's observer, and the learner model.

| | evidence anonymous | evidence named |
|---|---|---|
| `gpt-oss-120b` | run 1 — B 70/720 | cell D |
| `qwen3-30b` | **cell C — B 255/720** | run 2 — B 382/720 |

`cellC/` holds this directory's own measurement: `RESULTS.json` (every
count, paired test, coverage, curve and the full review ledger, recomputed
by `receipts/verify.py`), `MANIFEST.md` checksumming the raw files, which
stay local because they embed the corpus, and each seed's `run.config.json`
recording the exact pinned legs and host policy — including
`evidence_attribution: anonymous`, the one variable this cell turns.

Run 1 and run 2 live in `../receipts-sroie-2026-09-04/` and
`../receipts-sroie-run2-2026-09-04/`.

**Arm A is 97/720 in all three completed cells** — three separate runs,
hours apart, landing on the same baseline. That is the drift check that
makes the B column comparable.
