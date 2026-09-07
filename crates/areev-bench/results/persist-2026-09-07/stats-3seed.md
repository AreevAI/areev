# PAST-Bench — paired statistics

Runs: run1-sighted, run2, run3. Δ is the benchmark's self-evolution gap on the
evaluation episodes; the pairing unit is the family.

## Per arm

| arm | families | seeds | mean Δ | per-seed means | mechanism |
|---|---:|---:|---:|---|---:|
| areev-governed | 26 | 3 | **+0.266** | s1-sighted +0.237, s2 +0.286, s3 +0.273 | 0.201 |
| areev-passive | 26 | 3 | **+0.291** | s1-sighted +0.306, s2 +0.273, s3 +0.294 | 0.235 |
| hermes | 26 | 3 | **+0.246** | s1-sighted +0.252, s2 +0.265, s3 +0.221 | 0.166 |

## Noise floor — the same arm, the same family, different seeds

| arm | families with ≥2 seeds | mean abs seed-to-seed Δ difference | sd of family means |
|---|---:|---:|---:|
| areev-governed | 26 | **0.095** | 0.063 |
| areev-passive | 26 | **0.126** | 0.081 |
| hermes | 26 | **0.092** | 0.062 |

## Paired contrasts — per family, seed means, Wilcoxon signed-rank

| contrast | families | mean difference | wins/losses | Wilcoxon p | sign-test p |
|---|---:|---:|---|---:|---:|
| areev-governed − areev-passive | 26 | **-0.025** | 14/12 | 0.7800 | 0.8450 |
| areev-governed − hermes | 26 | **+0.020** | 12/13 | 0.4195 | 1.0000 |
| areev-passive − hermes | 26 | **+0.045** | 16/10 | 0.2133 | 0.3269 |

## Per capability (no test — 5 to 8 families each)

| capability | areev-governed | areev-passive | hermes |
|---|---|---|---|
| information-gathering | +0.467 (n=6) | +0.477 (n=6) | +0.425 (n=6) |
| memory | +0.310 (n=5) | +0.336 (n=5) | +0.255 (n=5) |
| procedural | +0.112 (n=8) | +0.144 (n=8) | +0.169 (n=8) |
| update | +0.236 (n=7) | +0.267 (n=7) | +0.174 (n=7) |
