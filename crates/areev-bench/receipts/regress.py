#!/usr/bin/env python3
"""The verify-then-revert leg: does the loop catch a lesson that hurts?

Approval is not the end of governance. A lesson a reviewer approved from
its text alone can still be useless or harmful in use, and the only honest
check is to measure it. This driver runs that check on a real memory, over
the real held-out receipts, through the real API, and deliberately breaks
things once so the revert path is exercised rather than asserted:

  1. VERIFY the lessons the experience phase applied. Every one carries the
     held-out set as its metric (run.py --measure); with A0 journaled before
     them and B after, a loop pass a day later records a verdict per lesson:
     `held` if B scored at least A0, `regressed` otherwise.
  2. ADMIT a harmful lesson through the governed path — authored by a
     fixture "model", grounded and verified by the same stub, and approved
     by the reviewer on purpose ("forced regression"). It renders into the
     prompt like any other rule.
  3. MEASURE: read the held-out set under it (arm H) and journal the pass.
  4. The next loop pass, a day later, finds H below B, records `regressed`,
     and proposes the REVERT. The reviewer approves it; applying it retracts
     the lesson through the same rollback path a person would use.
  5. Read the held-out set again (arm R): the prompt is back to B's.
  6. Ask the same fixture model again: the retracted lesson is NOT
     re-proposed — a measured revert puts the finding on cooldown.

Time is the engine's clock, pinned with AREEV_LOOP_NOW_MS for the passes
that must be "a day later"; the journaled runs are pinned to the same
timeline, because a run journaled before the apply is never evidence.
Every step's assertion is recorded, pass or fail, in regress.summary.json.
"""
import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dataset
import evalrun
import ledger_profile
import memory as mem

DAY = 86_400_000
HOUR = 3_600_000


def loop_pass(db_path, now_ms, llm_cmd=None, ground_cmd=None, policy=None, fixture=None,
              full_sweep=False, policy_dir=None):
    """One loop pass under the runner at a pinned engine time.

    A pass reflects over what is NEW since the last pass (the watermark); the
    passes that ask the fixture model to author over the whole history are
    full sweeps (`areev loop reflect`), which is the only way an injected
    lesson can cite evidence an earlier pass already saw."""
    env = dict(os.environ)
    os.environ["AREEV_LOOP_NOW_MS"] = str(now_ms)
    if fixture:
        os.environ["AREEV_MOCK_LLM_FIXTURE"] = fixture
    policy = mem.policy_file(db_path, policy, name="regress-policy.json", policy_dir=policy_dir)
    try:
        return json.loads(mem.with_memory(
            db_path, mem.RUNNER,
            lambda db: db.loop_run(llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=policy,
                                   full_sweep=full_sweep)))
    finally:
        os.environ.clear()
        os.environ.update(env)


def at(now_ms, fn, db_path, actor):
    env = dict(os.environ)
    os.environ["AREEV_LOOP_NOW_MS"] = str(now_ms)
    try:
        return mem.with_memory(db_path, actor, fn)
    finally:
        os.environ.clear()
        os.environ.update(env)


def applied_lessons(db_path):
    """How many lessons are live in the memory right now.

    The verdict check below needs this to be honest on a seed that learned
    NOTHING: cell D's seed 2 applied zero lessons across nineteen passes, and
    a check that simply asserted "some lesson got a verdict" failed it, while
    reporting under a name that claims something else. Zero applied lessons
    and zero verdicts is a pass — there was nothing to verify — and the gate
    still has to handle the planted rule, which is a separate check."""
    def count(db):
        return sum(1 for g in mem._facts(db)
                   if g.get("fields", {}).get("relation") in ("lesson", "fails_with")
                   and (g["fields"].get("object") or "").strip())
    return mem.with_memory(db_path, mem.REVIEWER, count)


def pending(db_path):
    return json.loads(mem.with_memory(db_path, mem.REVIEWER,
                                      lambda db: db.recommendations('{"status":"pending"}')))


def outcomes(db_path):
    return json.loads(mem.with_memory(db_path, mem.REVIEWER, lambda db: db.loop_outcomes()))


def main():
    ap = argparse.ArgumentParser()
    here = os.path.dirname(os.path.abspath(__file__))
    repo = os.path.abspath(os.path.join(here, "..", "..", ".."))
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--learned-db", required=True,
                    help="the experience memory, with A0 and B already journaled")
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--run-config", default=None,
                    help="the run's run.config.json, whose host policy this leg "
                         "inherits (default: beside the file memory; for a DSN, "
                         "the parent of --workdir)")
    ap.add_argument("--harmful", default=os.path.join(here, "fixtures", "lesson_harmful.json"))
    ap.add_argument("--mock-llm", default=os.path.join(repo, "examples", "llm", "mock.py"))
    ap.add_argument("--no-plant", action="store_true",
                    help="run only step 1 — verify the lessons the run actually "
                         "learned — and skip the planted-harmful sub-test. For a "
                         "ledger whose conventions changed there is no need to "
                         "plant anything: the run's own rules went stale when the "
                         "business changed, and whether the gate says so is the "
                         "experiment.")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    os.makedirs(args.workdir, exist_ok=True)
    py = sys.executable
    agent_argv = os.environ["AGENT_CMD"].split()
    _, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval)
    evalset = evalrun.evalset_hash(heldout)
    db = mem.bench_db(args.learned_db)
    if mem.is_dsn(db):
        print("memory: %s" % mem.redact(db))
    # Inherit the run's host policy rather than assuming one. The leg that
    # verifies a run should not quietly differ from it: cell C ran its
    # experience phase with `evidence_attribution: anonymous` and its regress
    # leg, hard-coding the policy here, ran named. Inert for that leg's claims
    # — its authoring is fixture-driven — but it is drift, and drift between a
    # run and the thing checking it is the kind that goes unnoticed.
    # `outcome_evalset` is always ours: regress recomputes the evalset hash.
    policy = {"discover_objective": "learner"}
    cfg = args.run_config
    if not cfg:
        # Beside a file memory; a DSN has no beside, so the parent of this
        # leg's work dir — where dryrun.sh puts the run's.
        base = os.path.dirname(os.path.abspath(args.workdir)) if mem.is_dsn(db) \
            else os.path.dirname(os.path.abspath(db))
        cfg = os.path.join(base, "run.config.json")
    if os.path.exists(cfg):
        ran_under = json.load(open(cfg)).get("policy")
        if ran_under:
            policy = json.loads(ran_under)
    policy["outcome_evalset"] = {"hash": evalset, "field": "exact", "higher_is_better": True}
    policy = json.dumps(policy)
    harmful_text = json.load(open(args.harmful))["recommendations"][0]["proposal"]["lesson"]
    mock_cmd = "%s %s" % (py, args.mock_llm)
    report = {"evalset": evalset, "steps": [], "checks": {}}

    def check(name, ok, detail=""):
        report["checks"][name] = {"ok": bool(ok), "detail": detail}
        print("  [%s] %s %s" % ("ok" if ok else "FAIL", name, detail))

    journal = open(os.path.join(args.workdir, "regress.jsonl"), "w", encoding="utf-8")
    t0 = int(time.time() * 1000)

    # 1. Verify the good lessons: a pass a day after the last apply. B was
    #    journaled after every apply and A0 before every proposal.
    rep = loop_pass(db, t0 + 2 * DAY, policy=policy, policy_dir=args.workdir)
    verdicts = outcomes(db)
    report["steps"].append({"step": "verify", "loop": rep, "outcomes": verdicts})
    n_lessons = len({o["rec_hash"] for o in verdicts})
    n_applied = applied_lessons(db)
    check("every applied lesson got a verdict", n_lessons >= n_applied,
          "%d verdict(s) for %d applied lesson(s): %s"
          % (n_lessons, n_applied, ", ".join(sorted({o["verdict"] for o in verdicts})) or "none"))
    check("no revert proposed for the lessons that held",
          not any(r.get("analyzer", "").startswith("loop.outcome_review") for r in pending(db))
          or any(o["verdict"] == "regressed" for o in verdicts),
          "pending: %d" % len(pending(db)))

    if args.no_plant:
        report["checks_note"] = ("verify-only: the planted-harmful sub-test was "
                                 "skipped, the run's own stale rules are the test")
        report["all_ok"] = all(c["ok"] for c in report["checks"].values())
        json.dump(report, open(os.path.join(args.workdir, "regress.summary.json"), "w"), indent=1)
        print("\nverify-only: %d lesson verdict(s) recorded"
              % len({o["rec_hash"] for o in verdicts}))
        return 0 if report["all_ok"] else 1

    # 2. Admit the harmful lesson through the governed path.
    t_h = t0 + 2 * DAY + HOUR
    rep = loop_pass(db, t_h, llm_cmd=mock_cmd, ground_cmd=mock_cmd, policy=policy, fixture=args.harmful, policy_dir=args.workdir,
                    full_sweep=True)
    cand = [r for r in pending(db) if harmful_text in (r.get("summary") or "")]
    check("the harmful lesson reached the queue", len(cand) == 1, json.dumps(rep.get("llm_funnel")))
    if not cand:
        json.dump(report, open(os.path.join(args.workdir, "regress.summary.json"), "w"), indent=1)
        sys.exit(1)
    harmful = cand[0]
    at(t_h + 1, lambda d: d.apply_recommendation(
        harmful["hash"], "forced regression: deliberately admitted to test the verify gate"),
       db, mem.REVIEWER)
    lessons_h = mem.with_memory(db, mem.REVIEWER, lambda _db: mem.lessons_markdown(_db, profile))
    check("it renders into the prompt", harmful_text in lessons_h)
    report["steps"].append({"step": "admit", "hash": harmful["hash"], "metric": harmful.get("metric"),
                            "lessons": lessons_h})

    # 3. Measure under it.
    trials_h, usage_h = evalrun.run_arm("H", profile, lessons_h, heldout, agent_argv, journal)
    sum_h = evalrun.journal_eval_run(db, evalset, "eval-h", trials_h, note="harmful lesson applied",
                                     at_ms=t_h + 2 * HOUR)
    report["steps"].append({"step": "measure-H", "summary": sum_h, "usage": usage_h})

    # 4. A day later: regressed → revert proposed → approved → applied.
    t_r = t_h + DAY + HOUR
    rep = loop_pass(db, t_r, policy=policy, policy_dir=args.workdir)
    verdicts = [o for o in outcomes(db) if o["rec_hash"] == harmful["hash"]]
    check("it was measured against the evalset (the metric the policy attached)",
          any(o["metric"] == "evalset:%s:exact" % evalset for o in verdicts), json.dumps(verdicts))
    check("the harmful lesson is measured as regressed",
          any(o["verdict"] == "regressed" for o in verdicts))
    reverts = [r for r in pending(db) if r.get("analyzer", "").startswith("loop.outcome_review")]
    check("a revert is proposed", len(reverts) >= 1, "%d pending revert(s)" % len(reverts))
    if reverts:
        at(t_r + 1, lambda d: d.apply_recommendation(
            reverts[0]["hash"], "the gate measured a regression on the held-out set"),
           db, mem.REVIEWER)
    rolled = json.loads(mem.with_memory(db, mem.REVIEWER,
                                        lambda d: d.recommendations('{"status":"rolled_back"}')))
    check("applying the revert rolls the lesson back",
          any(r["hash"] == harmful["hash"] for r in rolled))
    lessons_r = mem.with_memory(db, mem.REVIEWER, lambda _db: mem.lessons_markdown(_db, profile))
    check("the prompt no longer carries it", harmful_text not in lessons_r)
    report["steps"].append({"step": "revert", "loop": rep, "verdicts": verdicts,
                            "reverts": [r["hash"] for r in reverts], "lessons": lessons_r})

    # 5. Measure again: back to B.
    trials_r, usage_r = evalrun.run_arm("R", profile, lessons_r, heldout, agent_argv, journal)
    sum_r = evalrun.journal_eval_run(db, evalset, "eval-r", trials_r, note="harmful lesson reverted",
                                     at_ms=t_r + 2 * HOUR)
    report["steps"].append({"step": "measure-R", "summary": sum_r, "usage": usage_r})
    check("R recovers past H", sum_r["exact"] > sum_h["exact"],
          "H %d → R %d exact" % (sum_h["exact"], sum_r["exact"]))

    # 6. The same fixture model, the next pass: not re-proposed.
    rep = loop_pass(db, t_r + 3 * HOUR, llm_cmd=mock_cmd, ground_cmd=mock_cmd, policy=policy, policy_dir=args.workdir,
                    fixture=args.harmful, full_sweep=True)
    again = [r for r in pending(db) if harmful_text in (r.get("summary") or "")]
    check("the reverted lesson is not re-proposed", len(again) == 0,
          "funnel %s" % json.dumps(rep.get("llm_funnel")))
    report["steps"].append({"step": "re-propose", "loop": rep, "pending_harmful": len(again)})

    with open(os.path.join(args.workdir, "regress.trials.json"), "w", encoding="utf-8") as fh:
        json.dump(trials_h + trials_r, fh, indent=1)
    report["all_ok"] = all(c["ok"] for c in report["checks"].values())
    with open(os.path.join(args.workdir, "regress.summary.json"), "w") as fh:
        json.dump(report, fh, indent=1)
    print("\nregress: %s — %s" % ("ALL CHECKS PASSED" if report["all_ok"] else "CHECKS FAILED",
                                  os.path.join(args.workdir, "regress.summary.json")))
    sys.exit(0 if report["all_ok"] else 1)


if __name__ == "__main__":
    main()
