#!/usr/bin/env python3
"""Did the tuned small model learn the business, or memorise its vendors?

    slm_overfit.py --areev RUN --slm ROOT --dataset D [--json OUT]

The held-out receipts are disjoint from the training corpus by construction
(slm_corpus.py asserts it). But SROIE is 612 receipts from 225 shops, so
about half of any held-out set comes from a VENDOR whose other receipts were
in the corpus — and a model that memorised "B & BEST RESTAURANT, No.12 Jalan
SS4C/5" would ace that vendor's next receipt without having learned anything
about how this business files.

So split every held-out trial by whether its receipt's vendor appeared in
that seed's training corpus, and pair the tuned model against the LLM it was
distilled from on each half separately. Generalisation is what survives on
the unseen half; the difference between the halves is memorisation.
"""
import argparse
import collections
import json
import os
import re
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dataset

FIELDS = ("Invoice Date", "Vendor Name", "Amount", "Vendor Address")


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def norm(s):
    return re.sub(r"\s+", " ", (s or "").strip().lower())


def trials(path, arm):
    return {"%s|%s" % (t["seq"], t["field"]): t for t in json.load(open(path)) if t["arm"] == arm}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--areev", required=True)
    ap.add_argument("--slm", required=True)
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--json", default=None)
    args = ap.parse_args()

    rows = dataset.load(args.dataset)
    out = {"vendors_in_dataset": len({norm(r["truth"].get("Vendor Name")) for r in rows}),
           "receipts_in_dataset": len(rows), "seeds": {}, "pooled": {}}
    agg = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))   # (arm, half) -> [exact, n]
    fld = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))   # (arm, field, half)
    pair = collections.defaultdict(lambda: [0, 0])                                   # (a, b, half) -> [wins, losses]
    for sd in sorted(os.listdir(args.slm)):
        m = re.match(r"seed(\d+)$", sd)
        if not m:
            continue
        s = int(m.group(1))
        _, held = dataset.split(rows, s, args.experience, args.eval)
        train = [json.loads(l) for l in open(os.path.join(args.slm, sd, "corpus", "train.jsonl"))]
        seen_vendors = {norm(json.loads(x["messages"][2]["content"])["fields"].get("Vendor Name")) for x in train}
        seen_vendors.discard("")
        seen_seq = {r["seq"] for r in held if norm(r["truth"].get("Vendor Name")) in seen_vendors}
        arms = {"tuned": trials(os.path.join(args.slm, sd, "eval-tuned", "trials.json"), "B"),
                "untuned": trials(os.path.join(args.slm, sd, "eval-base", "trials.json"), "B"),
                "llm": trials(os.path.join(args.areev, sd, "eval", "trials.json"), "B"),
                "none": trials(os.path.join(args.areev, sd, "eval", "trials.json"), "A")}
        half = lambda k: "seen" if int(k.split("|")[0]) in seen_seq else "unseen"
        out["seeds"][s] = {"held_out": len(held), "seen_vendor_receipts": len(seen_seq),
                           "distinct_vendors_in_corpus": len(seen_vendors)}
        for name, tr in arms.items():
            for k, t in tr.items():
                h = half(k)
                agg[name][h][0] += t["exact"]; agg[name][h][1] += 1
                fld[(name, t["field"])][h][0] += t["exact"]; fld[(name, t["field"])][h][1] += 1
        for a, b in (("tuned", "llm"), ("tuned", "untuned"), ("llm", "none")):
            for k in set(arms[a]) & set(arms[b]):
                h = half(k)
                if arms[a][k]["exact"] and not arms[b][k]["exact"]:
                    pair[(a, b, h)][0] += 1
                elif arms[b][k]["exact"] and not arms[a][k]["exact"]:
                    pair[(a, b, h)][1] += 1

    for name in ("none", "untuned", "llm", "tuned"):
        out["pooled"][name] = {h: {"exact": v[0], "n": v[1], "rate": round(v[0] / v[1], 3)} for h, v in agg[name].items()}
        out["pooled"][name]["by_field"] = {f: {h: {"exact": fld[(name, f)][h][0], "n": fld[(name, f)][h][1]} for h in ("seen", "unseen")} for f in FIELDS}
    out["paired"] = {"%s_over_%s|%s" % (a, b, h): {"wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}
                     for (a, b, h), (w, l) in sorted(pair.items())}

    print("held-out receipts whose vendor was in the training corpus: %s" % ", ".join(
        "seed%d %d/%d" % (s, d["seen_vendor_receipts"], d["held_out"]) for s, d in sorted(out["seeds"].items())))
    print("\n| arm | seen vendor | unseen vendor |\n|---|---:|---:|")
    for name, label in (("none", "no memory"), ("untuned", "untuned 1.5B + rules"), ("llm", "30B LLM + rules"), ("tuned", "tuned 1.5B")):
        p = out["pooled"][name]
        print("| %s | %d/%d = %.0f%% | %d/%d = %.0f%% |" % (label, p["seen"]["exact"], p["seen"]["n"], 100 * p["seen"]["rate"],
                                                        p["unseen"]["exact"], p["unseen"]["n"], 100 * p["unseen"]["rate"]))
    print("\npaired, tuned over LLM:  seen %d/%d (p=%.4f)   unseen %d/%d (p=%.4f)" % (
        out["paired"]["tuned_over_llm|seen"]["wins"], out["paired"]["tuned_over_llm|seen"]["losses"], out["paired"]["tuned_over_llm|seen"]["p"],
        out["paired"]["tuned_over_llm|unseen"]["wins"], out["paired"]["tuned_over_llm|unseen"]["losses"], out["paired"]["tuned_over_llm|unseen"]["p"]))
    print("\n| field, unseen vendors only | tuned | LLM | untuned |\n|---|---:|---:|---:|")
    for f in FIELDS:
        g = lambda a: out["pooled"][a]["by_field"][f]["unseen"]
        print("| %s | %d/%d | %d/%d | %d/%d |" % (f, g("tuned")["exact"], g("tuned")["n"], g("llm")["exact"], g("llm")["n"], g("untuned")["exact"], g("untuned")["n"]))
    if args.json:
        json.dump(out, open(args.json, "w"), indent=1, sort_keys=True)
        print("\nwrote", args.json)


if __name__ == "__main__":
    sys.exit(main())
