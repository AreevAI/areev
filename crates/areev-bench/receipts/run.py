#!/usr/bin/env python3
"""The governed self-improvement run over public receipts — the experience phase.

Per document: the agent reads the OCR text and proposes a ledger row; the
accountant reviews it against the filed ledger and replies; the reply is
recorded into Areev memory as a human observation; the loop turns accumulated
corrections into proposed lessons; the accountant's reviewer approves or
rejects each; and the approved lessons render into the agent's prompt for
every later document.

The only lever on the agent's behaviour is what memory holds — the prompt is
assembled live from the file on every document — so the learning curve is
attributable to the governed apply and nothing else.

Environment: AGENT_CMD (chat adapter), LOOP_LLM_CMD / LOOP_GROUND_CMD (loop
adapters), REVIEW_CMD (the rule reviewer, chat adapter), LOOP_POLICY (optional
host policy JSON, e.g. {"discover_objective": "learner"}).
"""
import argparse
import json
import os
import shutil
import sys
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accountant as acct
import dataset
import ledger_profile
import memory as mem
from agent import propose


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--dataset", required=True, help="the corpus JSONL a builder emitted")
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60,
                    help="held-out size — reserved here so the split matches evaluate.py")
    ap.add_argument("--learn-every", type=int, default=2,
                    help="run the loop after this many corrected documents")
    ap.add_argument("--no-learn", action="store_true",
                    help="never record or apply anything — the A0 baseline")
    ap.add_argument("--snapshot-every", type=int, default=0,
                    help="copy the memory aside every N documents, so held-out "
                         "accuracy can be measured as a function of experience")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    os.makedirs(args.workdir, exist_ok=True)
    db_path = os.path.join(args.workdir, "ledger.db")
    if os.path.exists(db_path):
        raise SystemExit("%s already exists — a stale memory would poison the run; "
                         "use a fresh --workdir" % db_path)
    agent_argv = os.environ["AGENT_CMD"].split()
    llm_cmd = os.environ.get("LOOP_LLM_CMD")
    ground_cmd = os.environ.get("LOOP_GROUND_CMD")
    policy = os.environ.get("LOOP_POLICY") or None
    judge = mem.make_judge(os.environ.get("REVIEW_CMD"))

    exp_rows, _ = dataset.split(dataset.load(args.dataset), args.seed,
                                args.experience, args.eval)

    journal = open(os.path.join(args.workdir, "journal.jsonl"), "a", encoding="utf-8")
    categories_known, since_learn = [], 0
    totals = {"exact": 0, "semantic": 0, "scored": 0, "parked": 0,
              "approved": 0, "documents": 0, "prompt_tokens": 0,
              "completion_tokens": 0, "learn_passes": 0,
              "lessons_applied": 0, "lessons_rejected": 0}

    for r in exp_rows:
        seq = r["seq"]
        lessons = "" if args.no_learn else mem.with_memory(db_path, mem.RUNNER, mem.lessons_markdown)

        # The agent is NOT told what the accountant now wants. Day one it
        # knows one field; every later requirement has to reach it the long
        # way — the accountant says it, the loop proposes it, a reviewer
        # applies it, and it renders here as a lesson. Injecting the field
        # list would hand over exactly the thing being measured.
        req = ledger_profile.required_fields(profile, seq)
        out, usage = propose(agent_argv, profile, r["text"], lessons)
        totals["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
        totals["completion_tokens"] += int(usage.get("completion_tokens") or 0)

        approved, message, corrections = acct.review(profile, seq, out, r["truth"], categories_known)

        ex = sem = scored = 0
        for k in req:
            want = r["truth"].get(k, "")
            if not want:
                continue
            scored += 1
            e, s = acct.compare(profile, k, (out["fields"] or {}).get(k, ""), want)
            ex += bool(e)
            sem += bool(s)
        totals["exact"] += ex
        totals["semantic"] += sem
        totals["scored"] += scored
        totals["parked"] += bool(out["park"])
        totals["approved"] += bool(approved)
        totals["documents"] += 1

        n_lessons = lessons.count("\n- ")
        journal.write(json.dumps({
            "seq": seq, "id": r["id"],
            "at": datetime.now(timezone.utc).isoformat(),
            "required": req, "proposed": out["fields"], "parked": out["park"],
            "park_reason": out["reason"], "approved": approved,
            "accountant_said": message, "corrections": corrections,
            "exact": ex, "semantic": sem, "scored": scored,
            "lessons_in_prompt": n_lessons, "usage": usage,
        }, ensure_ascii=False) + "\n")
        journal.flush()

        print("seq %3d  exact %d/%-2d  semantic %d/%-2d  %s%-9s lessons=%d" % (
            seq, ex, scored, sem, scored,
            "PARK " if out["park"] else "",
            "ok" if approved else "corrected", n_lessons))

        if not args.no_learn and (message or corrections):
            mem.with_memory(db_path, mem.RUNNER,
                            lambda db: mem.record_correction(db, seq, message, corrections))
            since_learn += 1
            if since_learn >= args.learn_every:
                since_learn = 0
                res = mem.learn(profile, db_path, llm_cmd, ground_cmd, judge, policy)
                totals["learn_passes"] += 1
                totals["lessons_applied"] += res["applied"]
                totals["lessons_rejected"] += res["rejected"]
                print("   loop: %d proposed, %d approved, %d rejected"
                      % (res["pending"], res["applied"], res["rejected"]))
                journal.write(json.dumps({"learn_after_seq": seq, **res},
                                         ensure_ascii=False) + "\n")
                journal.flush()

        # A checkpoint of the memory as it stands after this many documents.
        # Held-out accuracy measured against each one is a learning curve
        # with the TASK held constant — unlike the running score of this
        # phase, which falls as the accountant adds fields and so measures the
        # goalpost moving, not the agent learning.
        if args.snapshot_every and totals["documents"] % args.snapshot_every == 0:
            snap = os.path.join(args.workdir, "snap_%03d.db" % totals["documents"])
            mem.copy_memory(db_path, snap)
            print("   snapshot -> %s" % os.path.basename(snap))

    with open(os.path.join(args.workdir, "experience.summary.json"), "w") as fh:
        json.dump({"profile": args.profile, "seed": args.seed,
                   "experience": args.experience, "learn_every": args.learn_every,
                   **totals}, fh, indent=1)
    print("\n" + "-" * 64)
    print("documents %d | exact %d/%d (%.1f%%) | semantic %d/%d (%.1f%%) | "
          "parked %d | first-pass approved %d | learn passes %d | applied %d rejected %d" % (
              totals["documents"], totals["exact"], totals["scored"],
              100.0 * totals["exact"] / max(totals["scored"], 1),
              totals["semantic"], totals["scored"],
              100.0 * totals["semantic"] / max(totals["scored"], 1),
              totals["parked"], totals["approved"], totals["learn_passes"],
              totals["lessons_applied"], totals["lessons_rejected"]))


if __name__ == "__main__":
    sys.exit(main())
