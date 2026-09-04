#!/usr/bin/env python3
"""Paired evaluation over the held-out retail tasks.

    evaluate.py --workdir DIR --learned-db DIR/retail.db [--arms B,B2,A]

Each held-out task is its own control: the same task, the same customer
scenario, the same seed, under prompts that are byte-identical except that
the learned rules are present or withdrawn. Only pairs that disagree carry
information (McNemar's exact test), which is what a raw solve-rate delta
hides.

  B   the rules as the experience phase left them
  B2  the same state again — the noise floor, and it is not small here:
      the customer is a language model too, so two identical passes disagree
  A   every applied rule rolled back through the API

Arm A is a genuine rollback: the claim is that the governed apply is the
lever, so withdrawing it travels the governance path rather than a flag.
"""
import argparse
import json
import os
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import memory as mem
import run as runner


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def paired(left, right):
    keys = sorted(set(left) & set(right))
    b = sum(1 for k in keys if left[k] and not right[k])
    c = sum(1 for k in keys if right[k] and not left[k])
    return {"n": len(keys), "wins": b, "losses": c, "p": round(mcnemar_exact(b, c), 6)}


def main():
    ap = argparse.ArgumentParser()
    runner.add_common_args(ap)
    ap.add_argument("--learned-db", required=True)
    ap.add_argument("--arms", default="B,B2,A")
    ap.add_argument("--journal", action="append", default=[],
                    help="ARM=RUN_ID: journal that arm as an evalset run in --learned-db")
    ap.add_argument("--control", action="store_true",
                    help="also run the held-out tasks under the FULL policy with no "
                         "lessons — the ceiling the withheld clauses cost")
    args = ap.parse_args()

    policy, agent_tools, withheld, _exp, held, full_policy, full_tools = runner.setup(args)
    os.makedirs(args.workdir, exist_ok=True)
    journal_as = dict(j.split("=", 1) for j in args.journal)
    evalset = runner.evalset_hash(held)

    db_b = os.path.join(args.workdir, "arm_b.db")
    db_a = os.path.join(args.workdir, "arm_a.db")
    mem.copy_memory(args.learned_db, db_b)
    mem.copy_memory(args.learned_db, db_a)
    rolled = mem.with_memory(db_a, mem.REVIEWER, mem.rollback_all)
    print("arm A: rolled back %d rule(s)" % len(rolled))

    records, by_arm = [], {}
    for arm in [a.strip() for a in args.arms.split(",") if a.strip()]:
        db = db_a if arm == "A" else db_b
        lessons = mem.with_memory(db, mem.REVIEWER, mem.lessons_markdown)
        recs = runner.run_arm(arm, held, policy, agent_tools, lessons, args)
        records += recs
        by_arm[arm] = {r["task_id"]: bool(r.get("reward", 0) >= 1.0) for r in recs}
        if arm in journal_as:
            s = mem.journal_eval_run(args.learned_db, evalset, journal_as[arm], recs)
            print("  journaled arm %s as %s: %s" % (arm, journal_as[arm], json.dumps(s)))

    # The ceiling. Without it a zero at A means nothing: an agent that cannot
    # do the task with the full policy in front of it was never going to be
    # taught the missing clause. Run last, so it cannot influence anything,
    # and with the FULL policy and tool descriptions and no lessons at all.
    if args.control:
        recs = runner.run_arm("FULL", held, full_policy, full_tools, "", args)
        records += recs
        by_arm["FULL"] = {r["task_id"]: bool(r.get("reward", 0) >= 1.0) for r in recs}

    stats = {}
    for left, right in (("B", "B2"), ("B", "A"), ("B2", "A"), ("FULL", "A"), ("FULL", "B")):
        if left in by_arm and right in by_arm:
            stats["%s_vs_%s" % (left, right)] = paired(by_arm[left], by_arm[right])
    solved = {a: sum(v.values()) for a, v in by_arm.items()}

    json.dump(records, open(os.path.join(args.workdir, "records.json"), "w"), indent=1)
    json.dump({"withheld": withheld, "evalset": evalset, "held_out": len(held),
               "solved": solved, "paired": stats, "rolled_back": rolled},
              open(os.path.join(args.workdir, "eval.summary.json"), "w"), indent=1)

    print("\nsolved: %s of %d" % (json.dumps(solved), len(held)))
    for k, v in stats.items():
        note = "noise floor" if k == "B_vs_B2" else "the effect"
        print("  %-9s %-12s n=%d  %s-only %d  %s-only %d  p=%.4f"
              % (k, note, v["n"], k.split("_vs_")[0], v["wins"], k.split("_vs_")[1], v["losses"], v["p"]))
    print("\nwrote %s" % os.path.join(args.workdir, "records.json"))


if __name__ == "__main__":
    sys.exit(main())
