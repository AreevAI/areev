#!/usr/bin/env python3
"""What did mem0 actually put in front of the agent? Opens a finished mem0
arm's store and, for a few held-out documents, runs the same search() the
checkpoint reads ran, printing the memories that came back -- and counts,
over the whole store, how many memories are instructions (a rule about a
field, a required field, a format) against per-document facts. The
checkpoint reads do not journal their prompt section, so this is the
receipt for "retrieval returned neighbouring filings' facts, not the
conventions". Read-only: search() does not write.

    mem0_peek.py --workdir W --profile vrdu_reg --dataset D --seed S [--experience 320] [--eval 100] [--docs 3] [--top-k 10]
"""
import argparse
import json
import os
import re
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dataset          # noqa: E402
import ledger_profile   # noqa: E402
from mem0_arm import USER, build_memory  # noqa: E402

INSTRUCTION = re.compile(r"\b(must|required|format|every registration form|always|should be written|as YYYY)\b", re.I)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--profile", default="vrdu_reg")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=320)
    ap.add_argument("--eval", type=int, default=100)
    ap.add_argument("--docs", type=int, default=3)
    ap.add_argument("--top-k", type=int, default=10)
    args = ap.parse_args()
    os.environ.setdefault("MEM0_TELEMETRY", "false")
    cfg = json.load(open(os.path.join(args.workdir, "run.config.json")))
    profile = ledger_profile.get(args.profile)
    _, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval)
    key = os.environ.get("OPENROUTER_API_KEY") or "unused-for-search"
    m = build_memory(args.workdir, cfg.get("mode", "default"), cfg.get("mem0_llm", "x"), key)

    # the store as a whole: instructions vs facts
    con = sqlite3.connect(os.path.join(args.workdir, "mem0_history.db"))
    rows = [r[0] or "" for r in con.execute("select new_memory from history where event='ADD'")]
    instr = [r for r in rows if INSTRUCTION.search(r)]
    print("store: %d memories added; %d read as instructions (%.0f%%), %d as per-document facts"
          % (len(rows), len(instr), 100.0 * len(instr) / max(len(rows), 1), len(rows) - len(instr)))
    for r in instr[:5]:
        print("  instruction e.g.:", r[:120])

    # what a held-out form retrieves
    out = {"store_memories": len(rows), "store_instructions": len(instr), "retrievals": []}
    for row in heldout[:args.docs]:
        q = (row["text"] or "").strip()[:1500]
        res = m.search(q, filters={"user_id": USER}, top_k=args.top_k)
        mems = [r.get("memory", "") for r in (res or {}).get("results", [])]
        n_i = sum(1 for x in mems if INSTRUCTION.search(x))
        print("\nheld-out %s (%s): %d memories retrieved, %d of them instructions" % (row["id"][:40], row["filed_at"], len(mems), n_i))
        for x in mems:
            print("   %s %s" % ("RULE" if INSTRUCTION.search(x) else "fact", x[:110]))
        out["retrievals"].append({"id": row["id"], "retrieved": len(mems), "instructions": n_i, "memories": mems})
    json.dump(out, open(os.path.join(args.workdir, "peek.json"), "w"), indent=1)
    print("\nwrote", os.path.join(args.workdir, "peek.json"))


if __name__ == "__main__":
    sys.exit(main())
