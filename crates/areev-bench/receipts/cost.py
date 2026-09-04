#!/usr/bin/env python3
"""What a run cost, from journaled tokens — never from a card statement.

    cost.py <run-dir> [--documents N] [--json OUT] [--refresh-prices]

Every model call in the harness is metered into a `usage.jsonl` (see
scripts/openrouter_*.py `_meter` and mem0_arm.py `meter_openai`). This sums
them per (script, model) under a run directory and prices them with the
pinned table below — OpenRouter list prices on the day they were pinned,
recorded here so the number is reproducible offline. `--refresh-prices`
prints today's list beside the pinned one; it never rewrites the table.

A model absent from the table is summed but priced at zero AND flagged, so
a cost chart cannot quietly omit a leg. Local models (the tuned SLM served
by mlx, ollama embeddings) are priced at zero marginal and reported by call
count; the SLM gets a SHADOW price too — what the same tokens would cost at
a hosted small-model rate — so "free because it ran on my laptop" is not
the only reading.
"""
import argparse
import glob
import json
import os
import sys
import urllib.request

# $ per million tokens, (prompt, completion). Pinned 2026-09-04 from
# https://openrouter.ai/api/v1/models.
PRICES = {
    "qwen/qwen3-30b-a3b-instruct-2507": (0.048, 0.193),
    "openai/gpt-4o-mini": (0.150, 0.600),
    "openai/gpt-4o": (2.500, 10.000),
    "openai/gpt-oss-120b": (0.037, 0.170),
}
# What a hosted ~1-2B instruct model costs per million tokens; the SLM's
# shadow price. Qwen's smallest hosted tier on OpenRouter is priced near
# this; it is a stand-in, labelled as one wherever it is shown.
SLM_SHADOW = (0.020, 0.080)
LOCAL = {"mlx-slm", "ollama"}


def load_usage(root):
    rows = []
    for p in glob.glob(os.path.join(root, "**", "usage.jsonl"), recursive=True):
        rel = os.path.relpath(os.path.dirname(p), root)
        for line in open(p, encoding="utf-8"):
            line = line.strip()
            if line:
                r = json.loads(line)
                r["where"] = rel
                rows.append(r)
    return rows


def price(model, pt, ct):
    if model in PRICES:
        i, o = PRICES[model]
        return pt / 1e6 * i + ct / 1e6 * o, True
    return 0.0, False


def summarize(rows):
    by = {}
    for r in rows:
        k = (r.get("script", "?"), r.get("model", "?"))
        b = by.setdefault(k, {"calls": 0, "prompt_tokens": 0, "completion_tokens": 0})
        b["calls"] += 1
        b["prompt_tokens"] += int(r.get("prompt_tokens") or 0)
        b["completion_tokens"] += int(r.get("completion_tokens") or 0)
    out, total, unpriced = [], 0.0, []
    for (script, model), b in sorted(by.items()):
        usd, priced = price(model, b["prompt_tokens"], b["completion_tokens"])
        shadow = None
        if model.startswith("mlx-slm"):
            shadow = b["prompt_tokens"] / 1e6 * SLM_SHADOW[0] + b["completion_tokens"] / 1e6 * SLM_SHADOW[1]
            priced = True
        if not priced and not any(model.startswith(l) for l in LOCAL):
            unpriced.append(model)
        total += usd
        out.append({"script": script, "model": model, **b, "usd": round(usd, 5),
                    "shadow_usd": None if shadow is None else round(shadow, 5)})
    return out, total, unpriced


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--documents", type=int, default=0, help="normalise to $/document")
    ap.add_argument("--json", default=None)
    ap.add_argument("--refresh-prices", action="store_true")
    args = ap.parse_args()

    if args.refresh_prices:
        with urllib.request.urlopen("https://openrouter.ai/api/v1/models", timeout=20) as r:
            live = {m["id"]: m["pricing"] for m in json.load(r)["data"]}
        for m, (i, o) in PRICES.items():
            p = live.get(m)
            now = ("%.3f / %.3f" % (float(p["prompt"]) * 1e6, float(p["completion"]) * 1e6)) if p else "absent"
            print("%-36s pinned %.3f / %.3f   today %s" % (m, i, o, now))
        return

    rows = load_usage(args.root)
    if not rows:
        raise SystemExit("no usage.jsonl under %s" % args.root)
    lines, total, unpriced = summarize(rows)
    print("| script | model | calls | prompt | completion | USD | shadow |")
    print("|---|---|---:|---:|---:|---:|---:|")
    for l in lines:
        print("| %s | %s | %d | %d | %d | %.4f | %s |" % (
            l["script"], l["model"], l["calls"], l["prompt_tokens"], l["completion_tokens"],
            l["usd"], "" if l["shadow_usd"] is None else "%.4f" % l["shadow_usd"]))
    print("\ntotal: $%.4f over %d call(s)" % (total, len(rows)))
    if args.documents:
        print("per document: $%.5f" % (total / args.documents))
    if unpriced:
        print("UNPRICED (in the table at $0 — add to PRICES): %s" % ", ".join(sorted(set(unpriced))),
              file=sys.stderr)
    if args.json:
        json.dump({"total_usd": round(total, 5), "calls": len(rows), "by": lines,
                   "unpriced": sorted(set(unpriced)), "documents": args.documents or None,
                   "per_document_usd": round(total / args.documents, 6) if args.documents else None},
                  open(args.json, "w"), indent=1)
        print("wrote", args.json)


if __name__ == "__main__":
    sys.exit(main())
