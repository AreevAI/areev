#!/usr/bin/env python3
"""Read a tuning learning curve: quality against corpus size, two ways to tune,
two held-out sets, and the loss curves that say whether anything overfit.

    curve_stats.py <runs-root> [--write]

Layout, per seedN/ (curve_tune.sh): ck_NNN/{adapter_scratch,adapter_continual}
each with adapter.manifest.json (val/train loss, kept checkpoint), and
ck_NNN/eval_{scratch,continual,llm}_{unseen,seen}/trials.json (arm B);
eval_base_{unseen,seen}/trials.json once per seed.

Reads, per checkpoint: exact of N on the UNSEEN set (organisations the agent
never learned from -- the number that can claim generalisation) and on the
SEEN set (the gap is what familiarity buys); scratch against continual,
paired per (document, field); each against the LLM carrying the same rules;
and validation loss at the kept checkpoint against the last one, which is
the overfitting receipt.
"""
import argparse
import collections
import glob
import json
import os
import re
import sys
from math import comb

MODES = ("llm", "scratch", "continual")
HOLD = ("unseen", "seen")


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def trials(path):
    if not os.path.exists(path):
        return None
    return {"%s|%s" % (t["seq"], t["field"]): t for t in json.load(open(path)) if t["arm"] == "B"}


def exact(tr):
    return sum(1 for t in tr.values() if t["exact"])


def paired(x, y):
    keys = set(x) & set(y)
    w = sum(1 for k in keys if x[k]["exact"] and not y[k]["exact"])
    l = sum(1 for k in keys if y[k]["exact"] and not x[k]["exact"])
    return {"n": len(keys), "wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    out = {"seeds": {}, "pooled": {}}
    pooled = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))  # (ck, mode, hold) -> [exact, n]
    pairs = collections.defaultdict(lambda: [0, 0])                                     # (ck, a, b, hold) -> [w, l]
    for sd in sorted(glob.glob(os.path.join(args.root, "seed*"))):
        s = int(re.search(r"seed(\d+)", sd).group(1))
        rec = {"checkpoints": {}, "base": {}}
        for h in HOLD:
            tr = trials(os.path.join(sd, "eval_base_%s" % h, "trials.json"))
            if tr:
                rec["base"][h] = {"exact": exact(tr), "n": len(tr)}
                pooled[("base", "base", h)][0] += exact(tr); pooled[("base", "base", h)][1] += len(tr)
        for ck in sorted(glob.glob(os.path.join(sd, "ck_*"))):
            k = int(os.path.basename(ck)[3:])
            row = {"modes": {}, "train": {}}
            got = {}
            for mode in MODES:
                for h in HOLD:
                    tr = trials(os.path.join(ck, "eval_%s_%s" % (mode, h), "trials.json"))
                    if tr:
                        got[(mode, h)] = tr
                        row["modes"]["%s|%s" % (mode, h)] = {"exact": exact(tr), "n": len(tr)}
                        pooled[(k, mode, h)][0] += exact(tr); pooled[(k, mode, h)][1] += len(tr)
            for mode in ("scratch", "continual"):
                man = os.path.join(ck, "adapter_%s" % mode, "adapter.manifest.json")
                if os.path.exists(man):
                    m = json.load(open(man))
                    last_val = m["val_loss"][-1][1] if m.get("val_loss") else None
                    row["train"][mode] = {"rows": m["corpus"]["train"], "iters": m["iters"], "seconds": m["train_seconds"],
                                          "best_val_iter": m.get("best_val_iter"), "best_val_loss": m.get("best_val_loss"),
                                          "last_val_loss": last_val, "kept": m.get("kept_checkpoint"),
                                          "resumed_from": m.get("resumed_from")}
            for h in HOLD:
                for a, b in (("scratch", "llm"), ("continual", "llm"), ("scratch", "continual")):
                    if (a, h) in got and (b, h) in got:
                        p = paired(got[(a, h)], got[(b, h)])
                        row.setdefault("paired", {})["%s_over_%s|%s" % (a, b, h)] = p
                        pairs[(k, a, b, h)][0] += p["wins"]; pairs[(k, a, b, h)][1] += p["losses"]
            rec["checkpoints"][k] = row
        out["seeds"][s] = rec

    cks = sorted({k for (k, _m, _h) in pooled if k != "base"})
    for k in cks:
        out["pooled"][k] = {"%s|%s" % (m, h): {"exact": v[0], "n": v[1], "rate": round(v[0] / v[1], 3) if v[1] else None}
                            for (kk, m, h), v in pooled.items() if kk == k}
        out["pooled"][k]["paired"] = {"%s_over_%s|%s" % (a, b, h): {"wins": v[0], "losses": v[1], "p": round(mcnemar(*v), 6)}
                                      for (kk, a, b, h), v in pairs.items() if kk == k}
    out["pooled"]["base"] = {h: {"exact": v[0], "n": v[1], "rate": round(v[0] / v[1], 3) if v[1] else None}
                             for (kk, _m, h), v in pooled.items() if kk == "base"}

    n_seeds = len(out["seeds"])
    print("Tuning learning curve, %d seed(s). Exact-match RATE; (wins/losses) paired against the LLM carrying the same rules.\n" % n_seeds)
    for h in HOLD:
        print("### held-out: %s\n" % h)
        print("| documents learned from | LLM + rules | tuned from scratch | tuned continually | scratch vs continual |")
        print("|---|---:|---:|---:|---:|")
        for k in cks:
            p = out["pooled"][k]
            def cell(mode):
                c = p.get("%s|%s" % (mode, h))
                if not c or not c["n"]:
                    return "—"
                q = p["paired"].get("%s_over_llm|%s" % (mode, h))
                return "%.0f%% (%d/%d)" % (100 * c["rate"], q["wins"], q["losses"]) if q else "%.0f%%" % (100 * c["rate"])
            llm = p.get("llm|%s" % h)
            sc = p["paired"].get("scratch_over_continual|%s" % h)
            print("| %d | %s | %s | %s | %s |" % (k, ("%.0f%%" % (100 * llm["rate"])) if llm and llm["n"] else "—",
                                                cell("scratch"), cell("continual"),
                                                ("%d/%d p=%.3f" % (sc["wins"], sc["losses"], sc["p"])) if sc else "—"))
        b = out["pooled"]["base"].get(h)
        if b and b["n"]:
            print("| untuned base, final rules | %.0f%% | | | |" % (100 * b["rate"]))
        print()
    print("### overfitting receipt (per seed, per checkpoint): kept = lowest-validation checkpoint; last = final iteration\n")
    print("| seed | documents | mode | rows | iters | best val (iter) | last val | kept |")
    print("|---|---:|---|---:|---:|---:|---:|---|")
    for s, rec in sorted(out["seeds"].items()):
        for k, row in sorted(rec["checkpoints"].items()):
            for mode, t in row["train"].items():
                print("| %d | %d | %s | %d | %d | %s (%s) | %s | %s |" % (
                    s, k, mode, t["rows"], t["iters"],
                    ("%.3f" % t["best_val_loss"]) if t["best_val_loss"] is not None else "—", t["best_val_iter"],
                    ("%.3f" % t["last_val_loss"]) if t["last_val_loss"] is not None else "—", t["kept"]))
    if args.write:
        p = os.path.join(args.root, "CURVE.json")
        json.dump(out, open(p, "w"), indent=1, sort_keys=True, default=str)
        print("\nwrote", p)


if __name__ == "__main__":
    sys.exit(main())
