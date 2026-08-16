#!/usr/bin/env python3
"""Turn a `AREEV_LLM_DEBUG=1` accuracy log into publishable, canonical results:
a summary JSON (config + overall + retrieval + per-category) and a transcripts
JSONL (one row per QA). Works on any run of the `accuracy` bin.

    usage: parse_results.py <log> <out_prefix> <reader> <judge> <embedder> <date>
    writes: <out_prefix>.summary.json  and  <out_prefix>.transcripts.jsonl
"""
import json
import re
import sys

CATS = {1: "multi-hop", 2: "temporal", 3: "open-domain", 4: "single-hop", 5: "adversarial"}


def main() -> None:
    log, out_prefix, reader, judge, embedder, date = sys.argv[1:7]
    lines = open(log, encoding="utf-8", errors="replace").read().splitlines()

    rows = []
    i = 0
    hdr = re.compile(r"^\[(✓|✗)\] cat(\d+) Q: (.*)$")
    while i < len(lines):
        m = hdr.match(lines[i])
        if not m:
            i += 1
            continue
        mark, cat, q = m.group(1), int(m.group(2)), m.group(3)
        get = lambda j, pre: lines[j][len(pre):] if j < len(lines) and lines[j].startswith(pre) else ""
        rows.append(
            {
                "category": cat,
                "category_name": CATS.get(cat, "other"),
                "correct": mark == "✓",
                "question": q,
                "gold": get(i + 1, "    gold: "),
                "answer": get(i + 2, "    got:  "),
                "verdict": get(i + 3, "    judge: "),
            }
        )
        i += 4

    total = len(rows)
    correct = sum(r["correct"] for r in rows)
    by_cat = {}
    for r in rows:
        c = by_cat.setdefault(r["category_name"], {"correct": 0, "total": 0})
        c["total"] += 1
        c["correct"] += r["correct"]
    for c in by_cat.values():
        c["accuracy_pct"] = round(c["correct"] / c["total"] * 100, 1) if c["total"] else 0.0

    def grab(pat):
        for ln in lines:
            g = re.search(pat, ln)
            if g:
                return float(g.group(1))
        return None

    summary = {
        "benchmark": "LoCoMo (snap-research/locomo, data/locomo10.json)",
        "date": date,
        "config": {"reader": reader, "judge": judge, "embedder": embedder, "top_k": 10},
        "questions": total,
        "answer_accuracy_pct": round(correct / total * 100, 1) if total else 0.0,
        "answer_correct": correct,
        "retrieval": {
            "hit@1": grab(r"hit@1\s*\|\s*([\d.]+)%"),
            "hit@5": grab(r"hit@5\s*\|\s*([\d.]+)%"),
            "hit@10": grab(r"hit@10\s*\|\s*([\d.]+)%"),
            "MRR@10": grab(r"MRR@10\s*\|\s*([\d.]+)"),
        },
        "by_category": by_cat,
        "notes": "Plain retrieve-then-read; session dates fed to the reader for temporal "
        "resolution. Number depends on reader/judge models + retrieval, not the store alone. "
        "LoCoMo answer key is ~6% wrong (dev.to/penfieldlabs).",
    }

    json.dump(summary, open(out_prefix + ".summary.json", "w"), indent=2, ensure_ascii=False)
    with open(out_prefix + ".transcripts.jsonl", "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(json.dumps(summary, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
