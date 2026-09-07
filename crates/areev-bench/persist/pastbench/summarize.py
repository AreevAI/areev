#!/usr/bin/env python3
"""One table from a pilot or full-run root: per agent × family, the
benchmark's own Δ and mechanism numbers, the agent's tokens per episode,
the loop legs' metered tokens, and the key-usage bound per run.

    summarize.py ROOT [--json OUT.json] [--md OUT.md]

ROOT is what pilot.sh wrote: ROOT/<agent>/<family_id>/ with the benchmark's
sequence_comparison.json and per-variant sequence_summary.json, usage.jsonl
beside them, and ROOT/spend.jsonl. Nothing here is recomputed from scratch
— every score is read from the benchmark's files; this only lays them side
by side. Prices are the pinned table in receipts/cost.py where a model is
listed; the judge is not token-metered by the benchmark, so its cost is
inside the key-usage bound and nowhere else.
"""
from __future__ import annotations

import argparse
import collections
import json
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / "receipts"))
try:
    from cost import PRICES  # receipts/cost.py — $ per M tokens (prompt, completion)
except Exception:
    PRICES = {}
PRICES.setdefault("minimax/minimax-m2.7", (0.30, 1.20))  # OpenRouter list, 2026-09-06


def load(p):
    try:
        return json.loads(Path(p).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def usage_by_model(path):
    out = collections.defaultdict(lambda: {"calls": 0, "prompt": 0, "completion": 0})
    if not Path(path).exists():
        return {}
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        m = str(r.get("model") or "?")
        out[m]["calls"] += 1
        out[m]["prompt"] += int(r.get("prompt_tokens") or 0)
        out[m]["completion"] += int(r.get("completion_tokens") or 0)
    return dict(out)


def priced(usage):
    total = 0.0
    unpriced = []
    for m, u in usage.items():
        if m in PRICES:
            pp, cp = PRICES[m]
            total += u["prompt"] * pp / 1e6 + u["completion"] * cp / 1e6
        else:
            unpriced.append(m)
    return round(total, 4), unpriced


def episode_tokens(variant_dir):
    """Agent tokens per episode from the benchmark's own traces
    (`[end] tokens=` is printed, but the trace holds the numbers)."""
    tot_in = tot_out = n = 0
    for tr in Path(variant_dir).glob("*/*.jsonl"):
        ein = eout = 0
        for line in tr.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            if r.get("type") == "runtime_response":
                u = (r.get("payload") or {}).get("usage") or {}
                ein += int(u.get("input_tokens") or 0)
                eout += int(u.get("output_tokens") or 0)
        if ein or eout:
            tot_in += ein
            tot_out += eout
            n += 1
    return {"episodes": n, "input": tot_in, "output": tot_out,
            "input_per_episode": round(tot_in / n) if n else None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    root = Path(a.root)
    spend = {}
    if (root / "spend.jsonl").exists():
        for line in (root / "spend.jsonl").read_text().splitlines():
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            spend[(r["agent"], os.path.basename(r["family"]))] = r
    rows = []
    for agent_dir in sorted(p for p in root.iterdir() if p.is_dir()):
        for fam_dir in sorted(p for p in agent_dir.iterdir() if p.is_dir()):
            cmp_ = load(fam_dir / "sequence_comparison.json")
            if not cmp_:
                continue
            fam = fam_dir.name
            d = cmp_.get("delta") or {}
            with_ = (cmp_.get("with_persistence") or {}).get("family_summary", {}).get(fam, {}).get("bucket_summary", {})
            without = (cmp_.get("without_persistence") or {}).get("family_summary", {}).get(fam, {}).get("bucket_summary", {})

            def bucket(bs, b, k="avg_task_score"):
                v = (bs.get(b) or {}).get(k)
                return round(float(v), 3) if v is not None else None

            usage = usage_by_model(fam_dir / "usage.jsonl")
            loop_cost, unpriced = priced(usage)
            sp = spend.get((agent_dir.name, fam), {})
            bound = None
            if sp.get("usage_before") is not None and sp.get("usage_after") is not None:
                bound = round(float(sp["usage_after"]) - float(sp["usage_before"]), 4)
            tok_with = episode_tokens(fam_dir / "with_persistence")
            tok_without = episode_tokens(fam_dir / "without_persistence")
            rows.append({
                "agent": agent_dir.name, "family": fam,
                "delta_eval": d.get("evaluation_avg_task_score"),
                "ablation_outcome_delta": d.get("ablation_outcome_delta"),
                "mechanism": d.get("avg_mechanism_score"),
                "evolve_score": d.get("avg_evolve_score"),
                "eval_with": bucket(with_, "evaluation"), "eval_without": bucket(without, "evaluation"),
                "control_with": bucket(with_, "control"), "control_without": bucket(without, "control"),
                "learn_with": bucket(with_, "learn"), "baseline": bucket(with_, "baseline"),
                "injection_with": (with_.get("evaluation") or {}).get("memory_injection_count"),
                "tokens_in_per_episode_with": tok_with["input_per_episode"],
                "tokens_in_per_episode_without": tok_without["input_per_episode"],
                "loop_usage": usage, "loop_cost_usd": loop_cost, "unpriced": unpriced,
                "key_usage_bound_usd": bound, "wall_s": sp.get("wall_s"), "exit": sp.get("exit"),
            })
    out = {"root": str(root), "rows": rows}
    if a.json:
        Path(a.json).write_text(json.dumps(out, indent=1), encoding="utf-8")
    lines = ["| agent | family | Δ eval | mech | eval on/off | control on/off | learn | inj | tok/ep on/off | loop $ | key Δ$ | wall |",
             "|---|---|---:|---:|---|---|---:|---:|---|---:|---:|---:|"]
    for r in rows:
        lines.append("| %s | %s | %s | %s | %s / %s | %s / %s | %s | %s | %s / %s | %s | %s | %ss |" % (
            r["agent"], r["family"], r["delta_eval"], r["mechanism"], r["eval_with"], r["eval_without"],
            r["control_with"], r["control_without"], r["learn_with"], r["injection_with"],
            r["tokens_in_per_episode_with"], r["tokens_in_per_episode_without"], r["loop_cost_usd"],
            r["key_usage_bound_usd"], r["wall_s"]))
    md = "\n".join(lines)
    if a.md:
        Path(a.md).write_text(md + "\n", encoding="utf-8")
    print(md)


if __name__ == "__main__":
    main()
