#!/usr/bin/env python3
"""Pool the prompt-structure grid over seeds.

    structure_stats.py <structure-root> [--write]

Each `seedN/structure.summary.json` holds every (position, format) cell's
exact count and its paired result against the baseline cell. This sums them
over seeds, re-tests the pooled discordant pairs, and prints one grid, so a
structural effect is read the same way a learned rule is: per (document,
field), McNemar exact, across every seed at once.
"""
import argparse
import glob
import json
import os
import re
import sys
from math import comb

POSITIONS = ("system-bottom", "system-top", "user-turn")
FORMATS = ("markdown", "json", "toon", "sml")


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    seeds = {}
    for p in sorted(glob.glob(os.path.join(args.root, "seed*", "structure.summary.json"))):
        s = json.load(open(p))
        seeds[int(re.search(r"seed(\d+)", p).group(1))] = s
    if not seeds:
        raise SystemExit("no seedN/structure.summary.json under %s" % args.root)
    base = next(iter(seeds.values()))["base_cell"]

    pooled = {}
    for pos in POSITIONS:
        for fmt in FORMATS:
            key = "%s|%s" % (pos, fmt)
            cells = [s["cells"][key] for s in seeds.values() if key in s["cells"]]
            if not cells:
                continue
            w = sum(c.get("vs_base", {}).get("wins", 0) for c in cells)
            l = sum(c.get("vs_base", {}).get("losses", 0) for c in cells)
            pooled[key] = {"position": pos, "format": fmt, "seeds": len(cells),
                           "exact": sum(c["exact"] for c in cells), "n": sum(c["n"] for c in cells),
                           "chars": round(sum(c["chars"] for c in cells) / len(cells)),
                           "vs_base": {"wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}}

    print("Baseline cell: %s (the published arm B placement). Pooled over %d seed(s)." % (base, len(seeds)))
    print("\n| rules placed… | " + " | ".join(FORMATS) + " |")
    print("|---|" + "---:|" * len(FORMATS))
    for pos in POSITIONS:
        row = []
        for fmt in FORMATS:
            c = pooled.get("%s|%s" % (pos, fmt))
            if not c:
                row.append("—")
            elif "%s|%s" % (pos, fmt) == base:
                row.append("**%d**" % c["exact"])
            else:
                v = c["vs_base"]
                row.append("%d (%d/%d)" % (c["exact"], v["wins"], v["losses"]))
        print("| %s | %s |" % (pos, " | ".join(row)))
    print("\nexact of %d; (wins/losses) against the baseline cell, McNemar exact" % next(iter(pooled.values()))["n"])

    if args.write:
        out = {"base_cell": base, "seeds": sorted(seeds), "cells": pooled,
               "per_seed": {str(k): {kk: {"exact": vv["exact"], "vs_base": vv.get("vs_base")}
                                     for kk, vv in v["cells"].items()} for k, v in seeds.items()}}
        p = os.path.join(args.root, "STRUCTURE.json")
        json.dump(out, open(p, "w"), indent=1, sort_keys=True)
        print("wrote", p)


if __name__ == "__main__":
    sys.exit(main())
