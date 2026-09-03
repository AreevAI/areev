#!/usr/bin/env python3
"""Paired evaluation: do the learned rules actually improve capture?

This does not compare runs. Each held-out document is its own control: the
same document is read under prompts that are byte-identical except that the
learned rules are present or withdrawn. Only the pairs that disagree carry
information (McNemar), and the reviewer's variance is quarantined in the
experience phase, which is over before this starts.

Arms:
  B   rules in force, exactly as the experience phase left them
  B2  the same state, run again — the noise floor of the agent itself
  A   every applied recommendation rolled back through the real API

A is produced by genuine rollback, not by declining to render: the claim is
that the governed apply is the lever, so withdrawing it has to travel the
governance path.

`--journal ARM=RUN_ID` records that arm's pass as an evalset run in the
primary memory (`--learned-db`), which is what lets the loop's Verify gate
measure the applied lessons against it (see evalrun.py, regress.py).
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dataset
import evalrun
import ledger_profile
import memory as mem


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40,
                    help="must match the experience run — it fixes the split")
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--learned-db", required=True, help="memory left by the experience phase")
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--arms", default="B,B2,A")
    ap.add_argument("--journal", action="append", default=[],
                    help="ARM=RUN_ID: journal that arm's pass as an evalset run in --learned-db")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    os.makedirs(args.workdir, exist_ok=True)
    agent_argv = os.environ["AGENT_CMD"].split()
    journal_as = dict(j.split("=", 1) for j in args.journal)

    # The held-out rows are evaluated at their arc position AFTER the whole
    # experience phase, so every field the accountant ever asked for is
    # required on every held-out document — the bar the learning curve
    # holds constant.
    _, rows = dataset.split(dataset.load(args.dataset), args.seed, args.experience, args.eval)
    evalset = evalrun.evalset_hash(rows)
    print("held-out documents: %d (seq %d..%d) evalset %s"
          % (len(rows), rows[0]["seq"], rows[-1]["seq"], evalset))

    # Two independent copies: the arms must not disturb each other, and the
    # experience-phase memory itself is never written to by an arm.
    db_b = os.path.join(args.workdir, "arm_b.db")
    db_a = os.path.join(args.workdir, "arm_a.db")
    mem.copy_memory(args.learned_db, db_b)
    mem.copy_memory(args.learned_db, db_a)
    rolled = mem.with_memory(db_a, mem.REVIEWER, mem.rollback_all)
    print("arm A: rolled back %d recommendation(s)" % len(rolled))

    journal = open(os.path.join(args.workdir, "eval.jsonl"), "w", encoding="utf-8")
    trials, usage, journaled = [], {}, {}
    for arm in [a.strip() for a in args.arms.split(",") if a.strip()]:
        db = db_a if arm == "A" else db_b
        lessons = mem.with_memory(db, mem.REVIEWER, mem.lessons_markdown)
        t, u = evalrun.run_arm(arm, profile, lessons, rows, agent_argv, journal)
        trials += t
        usage[arm] = u
        if arm in journal_as:
            journaled[arm] = evalrun.journal_eval_run(args.learned_db, evalset, journal_as[arm], t)
            print("  journaled arm %s as %s: %s" % (arm, journal_as[arm], json.dumps(journaled[arm])))

    with open(os.path.join(args.workdir, "trials.json"), "w", encoding="utf-8") as fh:
        json.dump(trials, fh, indent=1)
    with open(os.path.join(args.workdir, "eval.summary.json"), "w") as fh:
        json.dump({"profile": args.profile, "seed": args.seed, "held_out": len(rows),
                   "evalset": evalset, "rolled_back": rolled, "usage": usage,
                   "journaled": journaled}, fh, indent=1)
    print("\nwrote %s" % os.path.join(args.workdir, "trials.json"))


if __name__ == "__main__":
    sys.exit(main())
