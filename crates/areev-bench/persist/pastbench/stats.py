#!/usr/bin/env python3
"""The paired statistics across seeds — what the runs are allowed to claim.

    stats.py RUNROOT --runs run1-sighted,run2,run3 [--json OUT] [--md OUT]

For every arm it reports the benchmark's own Δ (self-evolution gap on the
evaluation episodes) per family per seed, and then:

- **the noise floor**: the same arm's Δ on the same family across seeds.
  The spread here bounds what any between-arm difference is allowed to
  mean. Reported as the mean absolute seed-to-seed difference and as the
  standard deviation of the per-family seed means.
- **paired arm contrasts**: for each pair of arms, the per-family
  difference of seed means, a two-sided **Wilcoxon signed-rank** test over
  the 26 families (exact for n≤20, normal approximation above), and a
  **paired sign test** as the distribution-free companion. Families are the
  pairing unit because every arm ran the same 26.
- **per capability**, the same contrast, reported without a test: 5–8
  families is too few to test and saying so is the point.

Nothing here re-scores an episode: every Δ is read from the benchmark's
`sequence_comparison.json` via `summarize.py`'s JSON.
"""
from __future__ import annotations

import argparse
import collections
import json
import math
import statistics
from itertools import combinations
from pathlib import Path

CAP_UPDATE = {"EP03_recall_then_modify", "SM03_fact_correction", "SM04_rule_migration",
              "SM06_temporary_exception_pollution", "SM07_scoped_rule_migration"}


def capability(family: str) -> str:
    if family.startswith("PC02") or family in CAP_UPDATE:
        return "update"
    if family.startswith("PG"):
        return "information-gathering"
    if family.startswith("PC"):
        return "procedural"
    return "memory"


def wilcoxon(diffs):
    """Two-sided Wilcoxon signed-rank. Exact for n<=20 (enumeration), normal
    approximation with continuity correction above. Zeros dropped (Wilcoxon's
    own convention), which is reported alongside n."""
    d = [x for x in diffs if x != 0]
    n = len(d)
    if n == 0:
        return None, 0
    order = sorted(range(n), key=lambda i: abs(d[i]))
    ranks = [0.0] * n
    i = 0
    while i < n:
        j = i
        while j + 1 < n and abs(d[order[j + 1]]) == abs(d[order[i]]):
            j += 1
        avg = (i + j) / 2.0 + 1
        for k in range(i, j + 1):
            ranks[order[k]] = avg
        i = j + 1
    w_plus = sum(r for r, x in zip(ranks, d) if x > 0)
    if n <= 20:
        # exact: every sign assignment is equally likely under H0
        from itertools import product
        target = min(w_plus, sum(ranks) - w_plus)
        count = 0
        total = 0
        for signs in product((0, 1), repeat=n):
            total += 1
            wp = sum(r for r, s in zip(ranks, signs) if s)
            if min(wp, sum(ranks) - wp) <= target:
                count += 1
        return count / total, n
    mean = n * (n + 1) / 4.0
    sd = math.sqrt(n * (n + 1) * (2 * n + 1) / 24.0)
    z = (abs(w_plus - mean) - 0.5) / sd
    p = 2 * (1 - 0.5 * (1 + math.erf(z / math.sqrt(2))))
    return min(1.0, p), n


def sign_test(diffs):
    pos = sum(1 for x in diffs if x > 0)
    neg = sum(1 for x in diffs if x < 0)
    n = pos + neg
    if n == 0:
        return None, 0, 0
    # exact two-sided binomial at p=0.5
    k = min(pos, neg)
    tail = sum(math.comb(n, i) for i in range(0, k + 1)) / 2 ** n
    return min(1.0, 2 * tail), pos, neg


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--runs", required=True)
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    root = Path(a.root)
    runs = [r for r in a.runs.split(",") if r]
    # arm -> family -> {run: delta}
    data: dict[str, dict[str, dict[str, float]]] = collections.defaultdict(lambda: collections.defaultdict(dict))
    mech: dict[str, list[float]] = collections.defaultdict(list)
    for run in runs:
        p = root / run / "summary.json"
        if not p.exists():
            print("missing", p)
            continue
        for r in json.loads(p.read_text())["rows"]:
            if r["delta_eval"] is None or r["agent"].startswith(("pre-seed", "seed-v")):
                continue
            data[r["agent"]][r["family"]][run] = r["delta_eval"]
            if r["mechanism"] is not None:
                mech[r["agent"]].append(r["mechanism"])
    out: dict = {"root": str(root), "runs": runs, "arms": {}, "contrasts": [], "noise": {}}
    lines = ["# PAST-Bench — paired statistics", "",
             "Runs: " + ", ".join(runs) + ". Δ is the benchmark's self-evolution gap on the",
             "evaluation episodes; the pairing unit is the family.", ""]

    lines += ["## Per arm", "", "| arm | families | seeds | mean Δ | per-seed means | mechanism |",
              "|---|---:|---:|---:|---|---:|"]
    for arm in sorted(data):
        fams = data[arm]
        per_seed = {run: statistics.mean(v[run] for v in fams.values() if run in v)
                    for run in runs if any(run in v for v in fams.values())}
        means = [statistics.mean(v.values()) for v in fams.values()]
        out["arms"][arm] = {"families": len(fams), "seeds": list(per_seed),
                            "mean_delta": statistics.mean(means),
                            "per_seed_mean": per_seed,
                            "mechanism": statistics.mean(mech[arm]) if mech[arm] else None}
        lines.append("| %s | %d | %d | **%+.3f** | %s | %.3f |" % (
            arm, len(fams), len(per_seed), statistics.mean(means),
            ", ".join("%s %+.3f" % (k.replace("run", "s"), v) for k, v in per_seed.items()),
            statistics.mean(mech[arm]) if mech[arm] else float("nan")))

    lines += ["", "## Noise floor — the same arm, the same family, different seeds", "",
              "| arm | families with ≥2 seeds | mean abs seed-to-seed Δ difference | sd of family means |",
              "|---|---:|---:|---:|"]
    for arm in sorted(data):
        spreads, sds = [], []
        for fam, byrun in data[arm].items():
            vals = list(byrun.values())
            if len(vals) < 2:
                continue
            spreads += [abs(x - y) for x, y in combinations(vals, 2)]
            sds.append(statistics.pstdev(vals))
        if spreads:
            out["noise"][arm] = {"mean_abs_diff": statistics.mean(spreads), "mean_sd": statistics.mean(sds),
                                 "n_families": len(sds)}
            lines.append("| %s | %d | **%.3f** | %.3f |" % (arm, len(sds), statistics.mean(spreads), statistics.mean(sds)))

    lines += ["", "## Paired contrasts — per family, seed means, Wilcoxon signed-rank", "",
              "| contrast | families | mean difference | wins/losses | Wilcoxon p | sign-test p |",
              "|---|---:|---:|---|---:|---:|"]
    arms = sorted(data)
    for x, y in combinations(arms, 2):
        common = sorted(set(data[x]) & set(data[y]))
        if len(common) < 5:
            continue
        diffs = [statistics.mean(data[x][f].values()) - statistics.mean(data[y][f].values()) for f in common]
        pw, nw = wilcoxon(diffs)
        ps, pos, neg = sign_test(diffs)
        out["contrasts"].append({"a": x, "b": y, "families": len(common),
                                 "mean_difference": statistics.mean(diffs), "wins": pos, "losses": neg,
                                 "wilcoxon_p": pw, "sign_p": ps,
                                 "per_family": dict(zip(common, diffs))})
        lines.append("| %s − %s | %d | **%+.3f** | %d/%d | %s | %s |" % (
            x, y, len(common), statistics.mean(diffs), pos, neg,
            "%.4f" % pw if pw is not None else "—", "%.4f" % ps if ps is not None else "—"))

    lines += ["", "## Per capability (no test — 5 to 8 families each)", "",
              "| capability | " + " | ".join(arms) + " |", "|---" * (len(arms) + 1) + "|"]
    caps = sorted({capability(f) for arm in data for f in data[arm]})
    for cap in caps:
        row = "| %s " % cap
        for arm in arms:
            vals = [statistics.mean(v.values()) for f, v in data[arm].items() if capability(f) == cap]
            row += "| %+.3f (n=%d) " % (statistics.mean(vals), len(vals)) if vals else "| — "
        lines.append(row + "|")

    md = "\n".join(lines)
    if a.json:
        Path(a.json).write_text(json.dumps(out, indent=1), encoding="utf-8")
    if a.md:
        Path(a.md).write_text(md + "\n", encoding="utf-8")
    print(md)


if __name__ == "__main__":
    main()
