# persist track — PAST-Bench evidence, 2026-09-07

What travels from the office box for [`../../PERSIST.md`](../../PERSIST.md).
Every number in that document is recomputable from these files; the
benchmark's own per-family `sequence_comparison.json` and
`sequence_summary.json` (129 MB) stay on the box, and `MANIFEST.md` carries
the sha256 of everything here.

| path | what it is |
|---|---|
| `stats-3seed.{json,md}` | the headline: per-arm means over three seeds, the noise floor, and the paired Wilcoxon / sign tests over 26 families |
| `run1/` | the FIRST run, **blind gate** — the reviewer was never shown the cited evidence (PERSIST.md §11 #18). Kept because it was published as a reading, not pooled with the rest |
| `run1-sighted/`, `run2/`, `run3/` | the three post-fix seeds, all three arms, 26 families each |
| `slm-tuned-seen/`, `slm-untuned/`, `slm-tuned-unseen/` | the tuning leg: the 1.7B with the adapter, the untuned control, and the held-out-families adapter |
| `<run>/<arm>/<family>/*.{areev,mem0}_ledgers.json` | the governance ledger per family per persistence variant: every proposal, the reviewer's decision and reason, the evidence it was shown, the agent's memory operations, the session-close nudge |
| `<run>/<arm>/<family>/usage.jsonl` | the metered model calls of the loop and reviewer legs |
| `<run>/spend.jsonl` | key-usage bound per family-run. **Not additive** when runs overlap: with nine parallel streams each bound covers every stream in its window |
| `<run>/summary.json` | `summarize.py`'s per-family table for that run |
| `audit.{json,md}` | run 2's governance decisions re-judged by a second model against the same evidence, with a 20-item hand-review sample |
| `adapters/` | both LoRA manifests (loss curves, VRAM, iterations) and both corpus manifests (which families, rows kept and skipped) |
| `tune-heldout.txt` | the six families held out of the unseen adapter, chosen by `random.Random(1)` over the sorted family names |

## Reproduce

The harness is [`../../persist/`](../../persist/); `persist/README.md` has
the commands. PAST-Bench is pinned at commit `f822351` with three patches
applied by `persist/pastbench/patch_pastbench.py`, all three recorded in
PERSIST.md §11 (an agent allowlist, a crash guard in the reflection
summariser, and a longer retry backoff for rate limits).
