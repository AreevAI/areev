#!/usr/bin/env python3
"""Turn a governed PAST-Bench run into a training corpus for a small model.

    corpus.py --run ROOT/<agent> --out DIR [--min-score 0.6] [--valid 0.1]
              [--families a,b,c | --exclude-families a,b,c]

Rows are the run's own record: for every learn and cold episode of the
with-persistence variant whose graded score cleared `--min-score`, the
system prompt the benchmark composed WITHOUT the injected memory section,
the task as the user turn, and the trajectory the agent produced — its text,
its task-tool calls and their results, its final answer — with the
persistence tool calls (`memory`, `skill_manage`, …) and their results
removed, because a model that carries the memory in its weights has no
memory tool to call. Evaluation and control episodes never enter the
corpus. The validation split mlx_lm/train_lora need is carved from the
training rows themselves.

`--families` / `--exclude-families` make the seen / unseen split of
PERSIST.md §3.3: train on 20 families, hold 6 out entirely.

Format: one `{"messages": [...], "tools": [...]}` per line — the shape
`receipts/slm_corpus.py` writes, plus the task's tool schemas so the chat
template can render the tool-call turns. Read by `persist/tune/train_lora.py`
and by mlx_lm alike.
"""
from __future__ import annotations

import argparse
import json
import random
import re
from pathlib import Path

PERSIST_TOOLS = {"memory", "skill_manage", "skills_list", "skill_view", "session_search"}
TOOL_RESULT_CAP = 1200
MEMORY_SECTION = re.compile(r"\n## Persistent memory\n.*\Z", re.S)


def load_trace(path):
    rows = []
    for line in Path(path).read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return rows


def episode_rows(trace_rows):
    """(system, user, turns, tools, score) from one benchmark trace."""
    system = user = None
    turns = []
    score = None
    tools = []
    for r in trace_rows:
        t = r.get("type")
        if t == "trace_start":
            d = r.get("payload") or r
            for k in ("tools", "task_tools"):
                if isinstance(d.get(k), list):
                    tools = d[k]
        elif t == "message":
            m = r.get("message") or {}
            role, content = m.get("role"), m.get("content") or []
            if role == "system" and system is None:
                system = "\n".join(b.get("text", "") for b in content if b.get("type") == "text")
            elif role == "user" and user is None:
                user = "\n".join(b.get("text", "") for b in content if b.get("type") == "text")
            elif role == "user":
                results = [b for b in content if b.get("type") == "tool_result"]
                for b in results:
                    turns.append({"role": "tool", "tool_use_id": b.get("tool_use_id"),
                                  "content": "\n".join(c.get("text", "") for c in (b.get("content") or []))})
            elif role == "assistant":
                text = "\n".join(b.get("text", "") for b in content if b.get("type") == "text")
                calls = [{"id": b.get("id"), "type": "function",
                          "function": {"name": b.get("name"), "arguments": json.dumps(b.get("input") or {})}}
                         for b in content if b.get("type") == "tool_use"]
                turns.append({"role": "assistant", "content": text, "tool_calls": calls})
        elif t == "grading_result":
            score = r.get("task_score")
    return system, user, turns, tools, score


def strip_persistence(turns):
    """Drop persistence tool calls and their results; drop an assistant turn
    that then has neither text nor calls."""
    dropped_ids = set()
    out = []
    for t in turns:
        if t["role"] == "assistant":
            keep = [c for c in t.get("tool_calls") or [] if c["function"]["name"] not in PERSIST_TOOLS]
            dropped_ids |= {c["id"] for c in (t.get("tool_calls") or []) if c["function"]["name"] in PERSIST_TOOLS}
            if not keep and not (t.get("content") or "").strip():
                continue
            row = {"role": "assistant", "content": t.get("content") or ""}
            if keep:
                row["tool_calls"] = keep
            out.append(row)
        else:
            if t.get("tool_use_id") in dropped_ids:
                continue
            # a tool result is context, not a target: capped so a trajectory
            # of a dozen calls still fits the trainer's 2K-token window with
            # its assistant turns inside it
            content = t.get("content") or ""
            if len(content) > TOOL_RESULT_CAP:
                content = content[:TOOL_RESULT_CAP] + "\n[... truncated for training ...]"
            out.append({"role": "tool", "content": content})
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True, help="ROOT/<agent> with one family dir per family")
    ap.add_argument("--out", required=True)
    ap.add_argument("--min-score", type=float, default=0.6)
    ap.add_argument("--valid", type=float, default=0.1)
    ap.add_argument("--families", default="")
    ap.add_argument("--exclude-families", default="")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    run = Path(a.run)
    only = {x for x in a.families.split(",") if x}
    excl = {x for x in a.exclude_families.split(",") if x}
    rows, manifest = [], {"families": {}, "min_score": a.min_score, "run": str(run)}
    for fam_dir in sorted(p for p in run.iterdir() if p.is_dir()):
        fam = fam_dir.name
        if (only and fam not in only) or fam in excl:
            continue
        kept = skipped = 0
        for ep_dir in sorted((fam_dir / "with_persistence").glob("0*")):
            name = ep_dir.name
            if "eval" in name or "control" in name:
                continue
            traces = list(ep_dir.glob("*.jsonl"))
            if not traces:
                continue
            system, user, turns, tools, score = episode_rows(load_trace(traces[0]))
            # The benchmark's trace carries neither the composed system prompt
            # nor the tool schemas; the adapter writes both into its ledger at
            # close (memory section already stripped). Older runs lack them
            # and are skipped, counted, and named in the manifest.
            ledger = {}
            try:
                ledger = json.loads((ep_dir / "artifacts" / "areev_ledger.json").read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                pass
            system = ledger.get("system_prompt") or system
            tools = ledger.get("task_tools") or tools
            if not system or user is None or score is None or score < a.min_score:
                skipped += 1
                continue
            system = MEMORY_SECTION.sub("", system)
            turns = strip_persistence(turns)
            if not turns or turns[-1]["role"] != "assistant":
                skipped += 1
                continue
            oa_tools = [{"type": "function", "function": {"name": t["name"], "description": t.get("description", ""),
                                                           "parameters": t.get("input_schema") or {"type": "object", "properties": {}}}}
                        for t in tools if t.get("name") not in PERSIST_TOOLS]
            rows.append({"family": fam, "episode": name, "score": score,
                         "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}] + turns,
                         "tools": oa_tools})
            kept += 1
        manifest["families"][fam] = {"kept": kept, "skipped": skipped}
    rng = random.Random(a.seed)
    rng.shuffle(rows)
    n_valid = max(2, int(len(rows) * a.valid)) if len(rows) >= 6 else max(1, len(rows) // 4)
    valid, train = rows[:n_valid], rows[n_valid:]
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    for name, part in (("train.jsonl", train), ("valid.jsonl", valid)):
        with open(out / name, "w", encoding="utf-8") as fh:
            for r in part:
                fh.write(json.dumps({"messages": r["messages"], "tools": r["tools"]}, ensure_ascii=False) + "\n")
    manifest.update({"rows_train": len(train), "rows_valid": len(valid),
                     "episodes": [{"family": r["family"], "episode": r["episode"], "score": r["score"]} for r in rows]})
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
    print("corpus: %d train / %d valid rows from %d families -> %s" % (len(train), len(valid), len(manifest["families"]), out))


if __name__ == "__main__":
    main()
