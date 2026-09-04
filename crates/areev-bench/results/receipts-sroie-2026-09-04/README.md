# receipts-sroie-2026-09-04 — governed self-improvement on public receipts

The run [`../../RECEIPTS.md`](../../RECEIPTS.md) reports, against its
pre-registration (committed before any paid receipt was read). Three seeds
over ICDAR 2019 SROIE: per seed, 40 experience receipts with a learn pass
every 2 corrections, and 60 held-out receipts read at A0, B, B2 and A, plus
the verify-then-revert leg and the learning curve.

## What is here, and what is not

`RESULTS.json` is the published evidence: every count, every paired test,
the per-field coverage, the curve, the verify-then-revert verdicts, and the
full review ledger — every rule proposed, whether the supervisor approved
it, and why. Every number in RECEIPTS.md is recomputed from it by
`receipts/verify.py`, and nothing is entered by hand.

**The raw trials do not travel.** `trials.json` and the per-receipt journal
rows carry the corpus's own values — vendor names, addresses, filed totals —
and this repo redistributes none of SROIE. `MANIFEST.md` checksums those
local files so an operator who re-runs can confirm their own copy is what
produced the published counts:

```bash
python3 crates/areev-bench/receipts/verify.py <your run dir> --check
```

The review ledger is the exception that does travel, deliberately: a
proposal's text is the model's own words and a reason is the reviewer's,
and a governance result that hid what was turned down would not be one.

## Reproduce

```bash
cd crates/areev-bench/receipts
python3 build_sroie.py                       # 626 fetched, 612 kept
sh dryrun.sh /tmp/dry                        # the keyless gate, no key needed
export OPENROUTER_API_KEY=…
for S in 1 2 3; do SEED=$S LEARNER_MODEL=openai/gpt-oss-120b \
  LEARNER_PIN=deepinfra/bf16 sh curve.sh runs 40 60 10; done
python3 verify.py runs --write
```

`run.config.json` in each seed directory records the exact commands, every
pinned model leg, and the host policy the run learned under.
