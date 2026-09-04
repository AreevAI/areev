# receipts-ablation-2026-09-04 — which change earned run 2's gain

The 2×2 pre-registered in [`../../RECEIPTS.md`](../../RECEIPTS.md), which
separates the two things receipts run 2 changed at once: the evidence
projection naming an Observation's observer, and the learner model.

| | evidence anonymous | evidence named |
|---|---|---|
| `gpt-oss-120b` | run 1 — B 70/720 | **cell D — B 245/720** |
| `qwen3-30b` | **cell C — B 255/720** | run 2 — B 382/720 |

`cellC/` and `cellD/` hold this directory's own measurements: `RESULTS.json`
(every count, paired test, coverage, curve and the full review ledger,
recomputed by `receipts/verify.py`), `MANIFEST.md` checksumming the raw
files, which stay local because they embed the corpus, and each seed's
`run.config.json` recording the exact pinned legs and host policy —
including `evidence_attribution`, the one variable these cells turn.

Run 1 and run 2 live in `../receipts-sroie-2026-09-04/` and
`../receipts-sroie-run2-2026-09-04/`.

**Arm A is 97/720 in all four cells** — four separate runs across a day,
landing on the same baseline. That is the drift check that makes the B
column comparable.

Read from the same baseline, the two changes are near-equal main effects:
attribution alone is **+175** (run 1 → cell D), the learner alone is **+185**
(run 1 → cell C), and both together are **+312**. Cell D was pre-registered
as an expected null and is not one; `../../RECEIPTS.md` records that
correction and what it means for the authoring-rate diagnostic that made the
prediction.
