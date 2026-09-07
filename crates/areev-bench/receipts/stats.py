#!/usr/bin/env python3
"""McNemar's exact test over the paired arms.

Only pairs that DISAGREE carry information. b = trials the rules fixed,
c = trials the rules broke; the concordant ones say nothing either way and
are excluded on purpose — reporting a raw accuracy delta over all trials
hides how few actually moved.

The B-vs-B2 line is the control and must be read first: it is the same memory
state twice, so anything it shows is the agent's own nondeterminism. An effect
in B-vs-A is only worth discussing if it is clearly larger than that.

    stats.py runs/<name>/eval/trials.json [exact|semantic]
"""
import json
import sys
from math import comb

PAIRS = [("B", "B2", "noise floor — identical state, so any movement is the agent"),
         ("B", "A", "the effect — rules in force vs rolled back"),
         ("B2", "A", "the effect, replicated against the second B pass")]


def mcnemar_exact(b, c):
    """Two-sided exact binomial p over the discordant pairs."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(comb(n, i) for i in range(k + 1)) / (2 ** n)
    return min(1.0, 2 * tail)


def main():
    trials = json.load(open(sys.argv[1], encoding="utf-8"))
    metric = sys.argv[2] if len(sys.argv) > 2 else "exact"

    by_arm = {}
    for t in trials:
        by_arm.setdefault(t["arm"], {})[(t["seq"], t["field"])] = t[metric]

    print("metric: %s\n" % metric)
    for arm, d in sorted(by_arm.items()):
        print("  arm %-3s %d/%d passed (%.1f%%)"
              % (arm, sum(d.values()), len(d), 100.0 * sum(d.values()) / max(len(d), 1)))
    print()

    for left, right, note in PAIRS:
        if left not in by_arm or right not in by_arm:
            continue
        keys = sorted(set(by_arm[left]) & set(by_arm[right]))
        b = sum(1 for k in keys if by_arm[left][k] and not by_arm[right][k])
        c = sum(1 for k in keys if by_arm[right][k] and not by_arm[left][k])
        p = mcnemar_exact(b, c)
        print("%s vs %s  (%s)" % (left, right, note))
        print("    n=%d pairs | %s-only %d | %s-only %d | discordant %d | p=%.4f"
              % (len(keys), left, b, right, c, b + c, p))
        if b + c == 0:
            print("    -> nothing moved at all")
        elif p < 0.05:
            print("    -> significant: %s wins %d, loses %d" % (left, b, c))
        else:
            print("    -> not significant at n=%d discordant" % (b + c))
        print()


if __name__ == "__main__":
    sys.exit(main())
