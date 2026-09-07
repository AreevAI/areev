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

# A CONVENTION is a memory that would tell the agent what to capture or how to
# write it on any document: it names a field, says must/required/format/always,
# and carries no particular value -- no registration number, no date, no
# quoted name. "The form ... must be filed with the Department of Justice" is
# boilerplate about forms, not a convention, and the first version of this
# probe counted it as one.
FIELD = re.compile(r"\b(registrant name|file date|signer name|registration number|date|name)\b", re.I)
DIRECTIVE = re.compile(r"\b(must be (captured|recorded|written|entered|included|extracted)|required field|is required|always (capture|record|include)|format(ted)? as|in the format|should be (captured|recorded|written))\b", re.I)
SPECIFIC = re.compile(r"\b(19|20)\d\d\b|\b\d{3,5}\b|'[^']{3,}'|\bfor (form|amendment|the amendment|this)\b", re.I)


def is_convention(text):
    return bool(FIELD.search(text) and DIRECTIVE.search(text) and not SPECIFIC.search(text))


INSTRUCTION = re.compile(r"(?!)")  # kept for the CLI's older summaries; unused


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
    instr = [r for r in rows if is_convention(r)]
    events = dict(con.execute("select event, count(*) from history group by event").fetchall())
    print("store: %d memories added; %d are conventions (%.1f%%), %d are about particular documents; events %s"
          % (len(rows), len(instr), 100.0 * len(instr) / max(len(rows), 1), len(rows) - len(instr), events))
    for r in instr[:5]:
        print("  convention e.g.:", r[:120])

    # what a held-out form retrieves
    out = {"store_memories": len(rows), "store_conventions": len(instr), "events": events, "retrievals": [],
           "task_query": None}
    # the fairness check: would a TASK-phrased query have found the conventions?
    tq = "Which fields must be captured on every registration form, and in what format must each be written?"
    res = m.search(tq, filters={"user_id": USER}, top_k=args.top_k)
    tmems = [r.get("memory", "") for r in (res or {}).get("results", [])]
    out["task_query"] = {"query": tq, "retrieved": len(tmems), "conventions": sum(1 for x in tmems if is_convention(x)), "memories": tmems}
    print("\ntask-phrased query retrieves %d memories, %d of them conventions" % (len(tmems), out["task_query"]["conventions"]))
    for x in tmems[:4]:
        print("   %s %s" % ("RULE" if is_convention(x) else "doc ", x[:110]))
    for row in heldout[:args.docs]:
        q = (row["text"] or "").strip()[:1500]
        res = m.search(q, filters={"user_id": USER}, top_k=args.top_k)
        mems = [r.get("memory", "") for r in (res or {}).get("results", [])]
        n_i = sum(1 for x in mems if is_convention(x))
        print("\nheld-out %s (%s): %d memories retrieved, %d of them instructions" % (row["id"][:40], row["filed_at"], len(mems), n_i))
        for x in mems:
            print("   %s %s" % ("RULE" if is_convention(x) else "doc ", x[:110]))
        out["retrievals"].append({"id": row["id"], "retrieved": len(mems), "instructions": n_i, "memories": mems})
    json.dump(out, open(os.path.join(args.workdir, "peek.json"), "w"), indent=1)
    print("\nwrote", os.path.join(args.workdir, "peek.json"))


if __name__ == "__main__":
    sys.exit(main())
