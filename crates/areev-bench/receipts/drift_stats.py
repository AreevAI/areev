#!/usr/bin/env python3
"""Read a drift run: does the loop track a ledger that changes its mind?

    drift_stats.py <run-dir> [--write]

A drift run leaves one `seedN/at_NNN/trials.json` per checkpoint, each
holding the three arms read under the convention in force at that document:

    B  the governed loop — proposed, reviewed, applied, measured
    C  the SAME memory rendered ungoverned — every correction verbatim,
       nothing retracted. What a store-and-recall memory puts in the prompt.
    A  every rule rolled back through the API — the frozen day-one floor

Three questions, and the checkpoints answer them in order:

  1. Does it learn as requirements arrive?  B over A, before the flip.
  2. Is governing worth more than remembering?  B against C, paired.
  3. Does it TRACK a change?  B after the flip, against B before it, and
     against the ungoverned arm which structurally cannot drop a stale rule.

Everything is paired per (document, field) and tested with McNemar's exact
test, the same way the stationary runs are, so the numbers are comparable.
Raw trials stay local because they embed corpus text; only counts travel.
"""
import argparse
import json
import os
import re
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ledger_profile

METRICS = ("exact", "semantic")


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def by_arm(trials):
    out = {}
    for t in trials:
        out.setdefault(t["arm"], {})["%s|%s" % (t["seq"], t["field"])] = t
    return out


def paired(x, y, metric):
    """x over y, counted only on the documents both arms actually read."""
    keys = set(x) & set(y)
    wins = sum(1 for k in keys if x[k][metric] and not y[k][metric])
    losses = sum(1 for k in keys if y[k][metric] and not x[k][metric])
    return {"n": len(keys), "wins": wins, "losses": losses,
            "p": round(mcnemar_exact(wins, losses), 6)}


def checkpoint(path):
    arms = by_arm(json.load(open(path)))
    row = {"arms": {}}
    for a, trials in arms.items():
        row["arms"][a] = {m: sum(1 for t in trials.values() if t[m]) for m in METRICS}
        row["arms"][a]["n"] = len(trials)
    for hi, lo in (("B", "A"), ("B", "C"), ("C", "A")):
        if hi in arms and lo in arms:
            row["%s_vs_%s" % (hi, lo)] = paired(arms[hi], arms[lo], "exact")
    # Which fields each arm actually produced. A stale rule shows up here
    # before it shows up in the score: an agent told two conflicting things
    # about a field often stops emitting it rather than choosing.
    row["coverage"] = {a: sum(1 for t in trials.values() if (t.get("got") or "").strip())
                       for a, trials in arms.items()}
    return row


def seed_result(seed_dir, profile):
    out = {"seed_dir": os.path.basename(seed_dir), "checkpoints": {}}
    for name in sorted(os.listdir(seed_dir)):
        m = re.match(r"at_(\d+)$", name)
        p = os.path.join(seed_dir, name, "trials.json")
        if m and os.path.exists(p):
            at = int(m.group(1))
            row = checkpoint(p)
            row["files"] = ledger_profile.as_of(profile, at).get("date_name")
            out["checkpoints"][str(at)] = row

    v = os.path.join(seed_dir, "verify", "regress.summary.json")
    if os.path.exists(v):
        s = json.load(open(v))
        verdicts = []
        for step in s.get("steps", []):
            for o in step.get("outcomes", []) or []:
                verdicts.append({k: o.get(k) for k in
                                 ("verdict", "baseline", "current", "metric")})
        out["verify"] = {"checks": {k: c["ok"] for k, c in s.get("checks", {}).items()},
                         "verdicts": verdicts}
    ex = os.path.join(seed_dir, "experience.summary.json")
    if os.path.exists(ex):
        e = json.load(open(ex))
        out["experience"] = {k: e[k] for k in
                             ("learn_passes", "lessons_applied", "lessons_rejected")
                             if k in e}
    return out


def flip_at(profile):
    reg = profile.get("regimes") or []
    return reg[-1][0] if len(reg) > 1 else None


def render(results, profile):
    flip = flip_at(profile)
    L = []
    L.append("Flip at document %s: the ledger switches to %s.\n"
             % (flip, ledger_profile.as_of(profile, flip or 0).get("date_name")))
    L.append("| seed | checkpoint | files | A | C | B | B vs A | B vs C |")
    L.append("|---|---|---|---|---|---|---|---|")
    for r in results:
        for at in sorted(r["checkpoints"], key=int):
            c = r["checkpoints"][at]
            a = c["arms"]
            g = lambda k: ("%d/%d" % (a[k]["exact"], a[k]["n"])) if k in a else "-"
            pv = lambda k: ("%d/%d" % (c[k]["wins"], c[k]["losses"])) if k in c else "-"
            L.append("| %s | %s%s | %s | %s | %s | %s | %s | %s |"
                     % (r["seed_dir"], at,
                        " **flip**" if flip and int(at) == flip else "",
                        c.get("files", "?"), g("A"), g("C"), g("B"),
                        pv("B_vs_A"), pv("B_vs_C")))
    L.append("")
    for r in results:
        v = r.get("verify")
        if not v:
            continue
        vs = v["verdicts"]
        held = sum(1 for x in vs if x["verdict"] == "held")
        reg = sum(1 for x in vs if x["verdict"] == "regressed")
        L.append("%s verify: %d verdict(s) — %d held, %d regressed%s"
                 % (r["seed_dir"], len(vs), held, reg,
                    "  (a stale rule that is never measured as regressed is "
                    "never reverted)" if reg == 0 and vs else ""))
    return "\n".join(L)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--profile", default="sroie_drift")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    seeds = sorted(os.path.join(args.root, d) for d in os.listdir(args.root)
                   if re.match(r"seed\d+$", d))
    if not seeds:
        raise SystemExit("no seedN/ directories under %s" % args.root)
    results = [seed_result(d, profile) for d in seeds]
    print(render(results, profile))

    if args.write:
        out = {"profile": args.profile, "flip_at": flip_at(profile), "seeds": results}
        p = os.path.join(args.root, "DRIFT.json")
        with open(p, "w", encoding="utf-8") as fh:
            json.dump(out, fh, indent=1, sort_keys=True)
        print("\nwrote", p)


if __name__ == "__main__":
    sys.exit(main())
