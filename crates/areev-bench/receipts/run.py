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
import evalrun
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
    ap.add_argument("--snapshot-at", default="",
                    help="comma-separated document counts to snapshot the memory at "
                         "(e.g. 20,40,80,160) -- checkpoints on a log axis for a learning curve")
    ap.add_argument("--snapshot-every", type=int, default=0,
                    help="copy the memory aside every N documents, so held-out "
                         "accuracy can be measured as a function of experience")
    ap.add_argument("--measure", action="store_true",
                    help="give every applied lesson the held-out set as its outcome "
                         "metric (Policy.outcome_evalset), so the loop's Verify gate "
                         "re-measures it once a later held-out pass is journaled")
    ap.add_argument("--journal-baseline", action="store_true",
                    help="before any learning, read the held-out set once with the "
                         "day-one agent and journal it as the evalset baseline (arm A0)")
    args = ap.parse_args()
    snapshot_at = {int(x) for x in args.snapshot_at.split(",") if x.strip()}
    # Every model call this phase makes is metered here (see scripts/*.py _meter),
    # so cost is read from journaled tokens, never estimated afterwards.
    os.makedirs(args.workdir, exist_ok=True)
    os.environ.setdefault("AREEV_USAGE_LOG", os.path.join(args.workdir, "usage.jsonl"))

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

    exp_rows, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed,
                                      args.experience, args.eval)
    evalset = evalrun.evalset_hash(heldout)

    # The host policy this run learns under. --measure names the held-out
    # set as the outcome evalset: from then on every applicable authored
    # lesson carries `evalset:<hash>:exact` as its metric, baseline = the
    # newest journaled pass before it was proposed.
    if args.measure:
        pol = json.loads(policy) if policy else {}
        pol["outcome_evalset"] = {"hash": evalset, "field": "exact", "higher_is_better": True}
        policy = json.dumps(pol)
    with open(os.path.join(args.workdir, "run.config.json"), "w") as fh:
        json.dump({"profile": args.profile, "seed": args.seed, "experience": args.experience,
                   "eval": args.eval, "evalset": evalset, "policy": policy,
                   "learn_every": args.learn_every, "measure": args.measure,
                   "journal_baseline": args.journal_baseline,
                   "agent_cmd": os.environ.get("AGENT_CMD"),
                   "loop_llm_cmd": llm_cmd, "loop_ground_cmd": ground_cmd,
                   "review_cmd": os.environ.get("REVIEW_CMD")}, fh, indent=1)

    # A0: the day-one agent over the held-out set, journaled before a single
    # lesson exists. It is the baseline every lesson is measured against, and
    # it is the same prompt arm A will produce later by rollback — so arm A is
    # a replication of it, the lessons-off noise floor.
    if args.journal_baseline:
        # Creating the memory first, so the baseline lands in the file the
        # run will learn into.
        mem.with_memory(db_path, mem.REVIEWER, lambda db: None)
        trials, usage = evalrun.run_arm("A0", profile, "", heldout, agent_argv)
        summary = evalrun.journal_eval_run(db_path, evalset, "eval-a0", trials,
                                           note="day-one agent, no lessons")
        with open(os.path.join(args.workdir, "a0.trials.json"), "w", encoding="utf-8") as fh:
            json.dump(trials, fh, indent=1)
        with open(os.path.join(args.workdir, "a0.summary.json"), "w") as fh:
            json.dump({"arm": "A0", "usage": usage, **summary}, fh, indent=1)
        print("journaled A0 as evalset %s baseline: %s" % (evalset, json.dumps(summary)))

    journal = open(os.path.join(args.workdir, "journal.jsonl"), "a", encoding="utf-8")
    categories_known, since_learn = [], 0
    totals = {"exact": 0, "semantic": 0, "scored": 0, "parked": 0,
              "approved": 0, "documents": 0, "prompt_tokens": 0,
              "completion_tokens": 0, "learn_passes": 0, "learn_failures": 0,
              "lessons_applied": 0, "lessons_rejected": 0}

    for r in exp_rows:
        seq = r["seq"]
        lessons = "" if args.no_learn else mem.with_memory(db_path, mem.RUNNER, lambda _db: mem.lessons_markdown(_db, profile))

        # The agent is NOT told what the accountant now wants. Day one it
        # knows one field; every later requirement has to reach it the long
        # way — the accountant says it, the loop proposes it, a reviewer
        # applies it, and it renders here as a lesson. Injecting the field
        # list would hand over exactly the thing being measured.
        req = ledger_profile.required_fields(profile, seq)
        # The ledger as it stands at THIS document. For a profile without
        # regimes these are the profile and the builder's truth unchanged;
        # for one with them, the convention and the filed values move
        # together, so the agent is never scored against a rule the business
        # has not stated yet.
        at = ledger_profile.as_of(profile, seq)
        truth_at = {k: ledger_profile.refile(at, k, v) for k, v in r["truth"].items()}
        try:
            out, usage = propose(agent_argv, at, r["text"], lessons)
        except Exception as e:
            # A provider having a bad minute must not end the deployment. The
            # document is treated as parked -- which is what the agent would
            # have done had it been told "no answer" -- the accountant fills it
            # in as they do for any park, and the failure is COUNTED in the
            # summary so a run that leaned on this is visibly not a clean one.
            # (Two seeds of the metered re-run died to a 429 at document 13
            # and 2 before this existed.)
            totals["model_call_failures"] = totals.get("model_call_failures", 0) + 1
            print("seq %3d  MODEL CALL FAILED (%s) -- treated as a park" % (seq, type(e).__name__))
            out, usage = {"fields": {}, "park": True, "reason": "model call failed"}, {}
        totals["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
        totals["completion_tokens"] += int(usage.get("completion_tokens") or 0)

        approved, message, corrections = acct.review(at, seq, out, truth_at, categories_known)

        ex = sem = scored = 0
        for k in req:
            want = truth_at.get(k, "")
            if not want:
                continue
            scored += 1
            e, s = acct.compare(at, k, (out["fields"] or {}).get(k, ""), want)
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
                            lambda db: mem.record_correction(db, seq, message, corrections, profile))
            since_learn += 1
            if since_learn >= args.learn_every:
                # A learn pass that cannot reach its model is skipped and
                # counted, not fatal: the evidence stays in the memory and the
                # next correction triggers the pass again. Seed 3 of the
                # learning curve died at document 26 when the learner's
                # provider rate-limited past its eight retries; agent calls
                # already survived that (park-on-failure), the loop did not.
                try:
                    res = mem.learn(profile, db_path, llm_cmd, ground_cmd, judge, policy)
                except (ValueError, RuntimeError) as e:
                    totals["learn_failures"] += 1
                    print("   loop: LEARN PASS FAILED (%s) -- skipped, retried at the next correction"
                          % str(e)[:120].replace("\n", " "))
                    journal.write(json.dumps({"learn_after_seq": seq, "failed": str(e)[:300]},
                                             ensure_ascii=False) + "\n")
                    journal.flush()
                    continue
                since_learn = 0
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
        if (args.snapshot_every and totals["documents"] % args.snapshot_every == 0) \
                or totals["documents"] in snapshot_at:
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
