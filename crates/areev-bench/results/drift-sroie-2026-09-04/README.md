# drift-sroie-2026-09-04 — a ledger that changes its mind

One seed of the non-stationary experiment [`../../DRIFT.md`](../../DRIFT.md)
describes: 160 real SROIE receipts, a new requirement at document 41, and
at document 81 the filing convention for dates replaced — announced once,
never repeated — with three arms read against the same held-out set at
every checkpoint, scored under the convention in force at that point.

`DRIFT.json` is `receipts/drift_stats.py --write` over the seed: every
arm's exact count per checkpoint, the paired tests, per-arm coverage, and
the verify verdicts. The three summaries beside it are the seed's own
configuration, experience totals and verify report. Raw trials and the
memory stay local because they embed corpus text.

**Why one seed.** Three were planned. Seed 3 was cancelled by decision when
the programme was redirected to the stationary four-way comparison. Seed 2
ran to completion and is **not published**: a harness defect, since fixed,
scanned the memory with `LIMIT 300` and that seed had written 432 facts, so
its prompt silently carried 4 of its 11 approved rules; it also took 34
failed model calls in a provider outage. Seed 1 is under both thresholds —
264 facts, all 5 rules rendered, zero failed calls — and is the evidence.
