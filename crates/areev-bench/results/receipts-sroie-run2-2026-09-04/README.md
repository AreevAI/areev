# receipts-sroie-run2-2026-09-04 — the run where the agent learned

Run 2 of the receipts experiment, pre-registered in
[`../../RECEIPTS.md`](../../RECEIPTS.md) before it was run. Same corpus,
same three seeds, same agent and same held-out sets as
[`../receipts-sroie-2026-09-04/`](../receipts-sroie-2026-09-04/); two things
differ, both named in the pre-registration — the evidence projection now
names who made an observation, and the learner is `qwen3-30b` rather than
`gpt-oss-120b`.

| seed | A (rolled back) | B (applied) | B vs A | noise floor |
|---|:---:|:---:|:---:|:---:|
| 1 | 31/240 | 123/240 | 92 wins, 0 losses | 2 |
| 2 | 36/240 | 141/240 | 106 wins, 1 loss | 4 |
| 3 | 30/240 | 118/240 | 88 wins, 0 losses | 1 |
| **pooled** | **97/720** | **382/720** | **286 wins, 1 loss** | 7 |

**Arm A pools to 97/720 in this run and 97/720 in run 1.** Same agent, same
receipts, same seeds, same rollback path: the day-one baseline is identical,
so the distance between run 1's B of 70 and this run's 382 is what the loop
authored and a reviewer approved.

## What is here, and what is not

`RESULTS.json` carries every count, every paired test, per-field coverage,
the curves, the verify verdicts and the full review ledger — every rule
proposed, whether it was approved, and why. `receipts/verify.py` recomputes
all of it; nothing is entered by hand. `MANIFEST.md` checksums the raw
files, which stay local because they embed SROIE's own values.

**Seed 3's verify leg is incomplete and that is recorded rather than
hidden.** A provider error killed the harness mid-arm, after it had
established that both of seed 3's learned rules measured `held` with no
revert proposed. It could not be re-run: the interrupted attempt had
already applied its deliberately harmful rule and crashed before the
revert, so that memory now carries an applied rule that is an artifact of
the crash and not of the run. Seed 3's headline numbers are unaffected —
its evaluation ran to completion before the verify leg started. Seeds 1 and
2 completed the leg in full, as did two seeds of run 1.

The crash also produced a fix: an arm now scores a failed model call as the
document producing nothing and reports, instead of taking five measured
documents down with it.

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_sroie.py
export OPENROUTER_API_KEY=…
for S in 1 2 3; do SEED=$S LEARNER_MODEL=qwen/qwen3-30b-a3b-instruct-2507 \
  LEARNER_PIN=coreweave/bf16 sh curve.sh runs 40 60 10; done
python3 verify.py runs --write
```
