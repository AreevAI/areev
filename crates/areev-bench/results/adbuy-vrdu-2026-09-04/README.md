# adbuy-vrdu-2026-09-04 — the second corpus

The run [`../../ADBUY.md`](../../ADBUY.md) reports, against its
pre-registration. Three seeds over VRDU ad-buy forms: per seed, 40
experience invoices with a learn pass every 2 corrections, and 60 held out
read at A0, B, B2 and A, plus the verify-then-revert leg and the curve.

| seed | A | B | B vs A |
|---|:---:|:---:|:---:|
| 1 | 45/285 | 224/285 | 179 wins, 0 losses |
| 2 | 43/275 | 233/275 | 190 wins, 0 losses |
| 3 | 34/280 | 133/280 | 99 wins, 0 losses |
| **pooled** | **122/840** | **590/840** | **468 wins, 0 losses** |

`RESULTS.json` carries every count, paired test, per-field coverage, curve,
verify verdict and the full review ledger; `receipts/verify.py` recomputes
all of it. `MANIFEST.md` checksums the raw files, which stay local because
they embed the corpus's own values. Each `seedN.run.config.json` records the
pinned legs and host policy.

Two things in the write-up that the numbers alone do not show: seed 3
applied ten rules, scored worst, and its two most-named fields collapsed to
4/60 and 3/60 coverage — with the Verify gate correctly reporting `held`,
because 133 still beats a baseline of 35. And the planted-regression leg
failed on seed 3 because its fixture is SROIE-shaped and inert here.
