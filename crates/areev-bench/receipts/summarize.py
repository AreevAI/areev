#!/usr/bin/env python3
"""Aggregate the eval arms of one or more seeds into the published table.

Emits per-field coverage, the paired-test counts for every arm pair and both
metrics, and per-field wins — everything RECEIPTS.md quotes. Every number
here recomputes from `trials.json`; nothing is entered by hand.

    summarize.py --profile sroie runs/s1/eval/trials.json [runs/s2/eval/trials.json ...]
"""
import argparse
import collections
import json
import os
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ledger_profile


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def one(path, fields):
    trials = json.load(open(path, encoding="utf-8"))
    by = {}
    for t in trials:
        by.setdefault(t["arm"], {})[(t["seq"], t["field"])] = t

    out = {"source": path}
    out["documents"] = len({k[0] for k in by["B"]})
    out["trials_per_arm"] = len(by["B"])

    # Coverage: did the agent put ANY value in the field?
    for arm in sorted(by):
        cov, tot = collections.Counter(), collections.Counter()
        for k, t in by[arm].items():
            tot[k[1]] += 1
            if str(t["got"]).strip():
                cov[k[1]] += 1
        out["coverage_" + arm] = {f: [cov.get(f, 0), tot.get(f, 0)] for f in fields if tot.get(f)}

    for metric in ("exact", "semantic"):
        for left, right in (("B", "B2"), ("B", "A"), ("B2", "A")):
            if left not in by or right not in by:
                continue
            keys = sorted(set(by[left]) & set(by[right]))
            b = sum(1 for k in keys if by[left][k][metric] and not by[right][k][metric])
            c = sum(1 for k in keys if by[right][k][metric] and not by[left][k][metric])
            out["%s_%s_vs_%s" % (metric, left, right)] = {
                "wins": b, "losses": c, "p": round(mcnemar_exact(b, c), 6)}
        wins = collections.Counter()
        for k, tb in by["B"].items():
            ta = by.get("A", {}).get(k)
            if ta and tb[metric] and not ta[metric]:
                wins[k[1]] += 1
        out[metric + "_wins_by_field"] = dict(wins)
        for arm in sorted(by):
            out["%s_passed_%s" % (metric, arm)] = sum(1 for t in by[arm].values() if t[metric])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("trials", nargs="+")
    args = ap.parse_args()
    fields = ledger_profile.get(args.profile)["fields"]
    results = [one(p, fields) for p in args.trials]
    print(json.dumps(results, indent=1))

    print("\n--- across %d seed(s) ---" % len(results), file=sys.stderr)
    for r in results:
        e = r.get("exact_B_vs_A", {})
        n = r.get("exact_B_vs_B2", {})
        print("  %-28s exact B-vs-A %d/%d (p=%.4f) | noise %d | exact B %d A %d of %d"
              % (r["source"][-28:], e.get("wins", 0), e.get("losses", 0), e.get("p", 1.0),
                 n.get("wins", 0) + n.get("losses", 0),
                 r.get("exact_passed_B", 0), r.get("exact_passed_A", 0), r["trials_per_arm"]),
              file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
