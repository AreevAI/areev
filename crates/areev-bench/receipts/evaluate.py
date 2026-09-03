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
that the governed apply is the lever, so withdrawing it has to go through the
governance path.
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accountant as acct
import dataset
import ledger_profile
import memory as mem
from agent import propose


def run_arm(name, profile, db_path, rows, agent_argv, journal):
    """One pass over the held-out documents at a fixed memory state."""
    lessons = mem.with_memory(db_path, mem.REVIEWER, mem.lessons_markdown)
    n_rules = lessons.count("\n- ")
    print("\n=== arm %s — %d rule(s) in the prompt" % (name, n_rules))

    trials, usage_tot = [], {"prompt_tokens": 0, "completion_tokens": 0}
    for r in rows:
        seq = r["seq"]
        req = ledger_profile.required_fields(profile, seq)
        out, usage = propose(agent_argv, profile, r["text"], lessons)
        usage_tot["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
        usage_tot["completion_tokens"] += int(usage.get("completion_tokens") or 0)
        for field in req:
            want = r["truth"].get(field, "")
            if not want:
                continue
            got = (out["fields"] or {}).get(field, "")
            ex, sem = acct.compare(profile, field, got, want)
            trials.append({"arm": name, "seq": seq, "id": r["id"], "field": field,
                           "exact": bool(ex), "semantic": bool(sem),
                           "got": got, "want": want})
        journal.write(json.dumps({
            "arm": name, "seq": seq, "id": r["id"], "rules_in_prompt": n_rules,
            "proposed": out["fields"], "parked": out["park"], "usage": usage,
        }, ensure_ascii=False) + "\n")
        journal.flush()
        ex = sum(t["exact"] for t in trials if t["seq"] == seq)
        sm = sum(t["semantic"] for t in trials if t["seq"] == seq)
        tot = sum(1 for t in trials if t["seq"] == seq)
        print("  seq %3d  exact %d/%-2d semantic %d/%-2d %s"
              % (seq, ex, tot, sm, tot, "PARK" if out["park"] else ""))

    e = sum(t["exact"] for t in trials)
    s = sum(t["semantic"] for t in trials)
    print("  arm %s: exact %d/%d (%.1f%%)  semantic %d/%d (%.1f%%)  tokens %d+%d"
          % (name, e, len(trials), 100.0 * e / max(len(trials), 1),
             s, len(trials), 100.0 * s / max(len(trials), 1),
             usage_tot["prompt_tokens"], usage_tot["completion_tokens"]))
    return trials, usage_tot


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
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    os.makedirs(args.workdir, exist_ok=True)
    agent_argv = os.environ["AGENT_CMD"].split()

    # The held-out rows are evaluated at their arc position AFTER the whole
    # experience phase, so every field the accountant ever asked for is
    # required on every held-out document — the same seven-field bar the
    # learning curve holds constant.
    _, rows = dataset.split(dataset.load(args.dataset), args.seed, args.experience, args.eval)
    print("held-out documents: %d (seq %d..%d)" % (len(rows), rows[0]["seq"], rows[-1]["seq"]))

    # Two independent copies: the arms must not disturb each other, and the
    # experience-phase memory itself is never written to.
    db_b = os.path.join(args.workdir, "arm_b.db")
    db_a = os.path.join(args.workdir, "arm_a.db")
    mem.copy_memory(args.learned_db, db_b)
    mem.copy_memory(args.learned_db, db_a)
    rolled = mem.with_memory(db_a, mem.REVIEWER, mem.rollback_all)
    print("arm A: rolled back %d recommendation(s)" % len(rolled))

    journal = open(os.path.join(args.workdir, "eval.jsonl"), "w", encoding="utf-8")
    trials, usage = [], {}
    for arm in [a.strip() for a in args.arms.split(",") if a.strip()]:
        db = db_a if arm == "A" else db_b
        t, u = run_arm(arm, profile, db, rows, agent_argv, journal)
        trials += t
        usage[arm] = u

    with open(os.path.join(args.workdir, "trials.json"), "w", encoding="utf-8") as fh:
        json.dump(trials, fh, indent=1)
    with open(os.path.join(args.workdir, "eval.summary.json"), "w") as fh:
        json.dump({"profile": args.profile, "seed": args.seed, "held_out": len(rows),
                   "rolled_back": rolled, "usage": usage}, fh, indent=1)
    print("\nwrote %s" % os.path.join(args.workdir, "trials.json"))


if __name__ == "__main__":
    sys.exit(main())
