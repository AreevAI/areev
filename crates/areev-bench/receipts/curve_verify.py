#!/usr/bin/env python3
"""The verify leg of the learning curve: feed the loop the deployment's own
checkpoint reads and let it record its verdicts.

The governed run journaled one evalset run -- day one, before any rule --
and read the held-out set at every checkpoint against a SNAPSHOT of its
memory, never journaling the read back. So when a rule approved at document
120 took the LLM from 86% to 66% on registrants it never saw, the loop's
Verify gate had nothing after the apply to compare, and the only number it
could have compared against was day one's 26%.

This leg puts those reads where the engine can see them: every checkpoint's
LLM read is journaled into a COPY of the final ledger as an evalset run,
timestamped at the document the checkpoint stood at, so the timeline is the
deployment's own. A loop pass two days after the last document then records
a verdict per applied lesson -- baseline the newest run before the apply,
current the newest after it -- and proposes a revert where it measured a
regression. The reviewer applies the revert, and the LLM reads the unseen
set once more under the rules that remain (arm R), so "the gate would have
caught it" is a measurement with a receipt, not an argument.

    PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 sh curve_verify.sh <seed-dir>

Nothing the main run wrote is touched: the ledger is copied first.
"""
import argparse
import json
import os
import shutil
import sys
from datetime import datetime

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dataset          # noqa: E402
import evalrun          # noqa: E402
import ledger_profile   # noqa: E402
import memory as mem    # noqa: E402
import regress          # noqa: E402

DAY = 86_400_000
HOUR = 3_600_000


def iso_ms(s):
    return int(datetime.fromisoformat(s).timestamp() * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="vrdu_reg")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=320)
    ap.add_argument("--eval", type=int, default=100)
    ap.add_argument("--seed-dir", required=True, help="the curve_tune.sh seed directory")
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--no-read", action="store_true", help="record verdicts and reverts; skip the arm-R read")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    os.makedirs(args.workdir, exist_ok=True)
    os.environ.setdefault("AREEV_USAGE_LOG", os.path.join(args.workdir, "usage.jsonl"))
    sd = args.seed_dir
    report = {"seed": args.seed, "seed_dir": sd, "steps": [], "checks": {}}

    def check(name, ok, detail=""):
        report["checks"][name] = {"ok": bool(ok), "detail": detail}
        print("  [%s] %s %s" % ("ok" if ok else "FAIL", name, detail))

    # The deployment's own timeline and policy.
    docs = {}
    for line in open(os.path.join(sd, "journal.jsonl"), encoding="utf-8"):
        j = json.loads(line)
        if "seq" in j and "at" in j:
            docs[j["seq"]] = iso_ms(j["at"])
    last_seq = max(docs)
    cfg = json.load(open(os.path.join(sd, "run.config.json")))
    policy = json.loads(cfg["policy"]) if isinstance(cfg.get("policy"), str) else (cfg.get("policy") or {})
    evalset = (policy.get("outcome_evalset") or {}).get("hash")
    if not evalset:
        raise SystemExit("the run's policy names no outcome evalset; nothing for the gate to measure against")
    report["evalset"] = evalset
    report["policy"] = policy

    # A copy of the final ledger: the main run's artifacts stay untouched.
    db = os.path.join(args.workdir, "ledger.db")
    for ext in ("", "-wal", ".blobs", ".telemetry.db", ".telemetry.db-wal"):
        src = os.path.join(sd, "ledger.db" + ext)
        if os.path.exists(src):
            if os.path.isdir(src):
                shutil.copytree(src, db + ext, dirs_exist_ok=True)
            else:
                shutil.copy2(src, db + ext)

    # 1. Journal every checkpoint's LLM read at the document it stood at.
    journaled = []
    for ck in sorted(d for d in os.listdir(sd) if d.startswith("ck_")):
        k = int(ck[3:])
        p = os.path.join(sd, ck, "eval_llm_unseen", "trials.json")
        if not os.path.exists(p):
            print("  checkpoint %d: no LLM read yet -- skipped" % k)
            continue
        trials = [t for t in json.load(open(p)) if t["arm"] == "B"]
        at_ms = docs[min(k, last_seq)] + (1000 if k >= last_seq else 1)
        s = evalrun.journal_eval_run(db, evalset, "eval-ck%03d" % k, trials,
                                     note="the LLM with the rules as of document %d, unseen set" % k, at_ms=at_ms)
        journaled.append({"checkpoint": k, "at_ms": at_ms, **s})
        print("  journaled checkpoint %3d: %d/%d exact at t(doc %d)" % (k, s["exact"], s["total"], min(k, last_seq)))
    report["steps"].append({"step": "journal", "runs": journaled})
    check("checkpoint reads journaled", len(journaled) >= 2, "%d run(s)" % len(journaled))

    # 2. The loop, two days after the last document: verdicts and reverts.
    t_pass = docs[last_seq] + 2 * DAY
    rep = regress.loop_pass(db, t_pass, policy=json.dumps(policy))
    verdicts = regress.outcomes(db)
    applied = regress.at(t_pass, lambda d: json.loads(d.recommendations('{"status":"applied"}')), db, mem.REVIEWER)
    text = {r["hash"]: (r.get("summary") or "") for r in applied}
    rows = []
    for o in verdicts:
        rows.append({"rec_hash": o["rec_hash"], "lesson": text.get(o["rec_hash"], "")[:160], "metric": o["metric"],
                     "baseline": o["baseline"], "current": o["current"], "verdict": o["verdict"],
                     "horizon_ms": o.get("horizon_ms")})
        print("  %-9s baseline %5.0f -> current %5.0f  %s" % (o["verdict"], o["baseline"], o["current"], text.get(o["rec_hash"], "")[:80]))
    reverts = [r for r in regress.pending(db) if r.get("analyzer", "").startswith("loop.outcome_review")]
    report["steps"].append({"step": "verify", "loop": rep, "verdicts": rows, "reverts": [r["hash"] for r in reverts]})
    check("every applied lesson got a verdict", len({o["rec_hash"] for o in verdicts}) >= len(applied),
          "%d verdict(s) for %d applied" % (len({o["rec_hash"] for o in verdicts}), len(applied)))
    check("a revert is proposed for each regressed lesson",
          len(reverts) >= len({o["rec_hash"] for o in verdicts if o["verdict"] == "regressed"}),
          "%d regressed, %d revert(s) proposed" % (len({o["rec_hash"] for o in verdicts if o["verdict"] == "regressed"}), len(reverts)))

    # 3. The reviewer applies the reverts; the prompt loses those rules.
    before = mem.with_memory(db, mem.REVIEWER, lambda d: mem.lessons_markdown(d, profile))
    for i, r in enumerate(reverts):
        regress.at(t_pass + HOUR + i, lambda d, h=r["hash"]: d.apply_recommendation(
            h, "the gate measured a regression on the deployment's own held-out reads"), db, mem.REVIEWER)
    after = mem.with_memory(db, mem.REVIEWER, lambda d: mem.lessons_markdown(d, profile))
    rolled = regress.at(t_pass + HOUR + 60, lambda d: json.loads(d.recommendations('{"status":"rolled_back"}')), db, mem.REVIEWER)
    report["steps"].append({"step": "revert", "rolled_back": [{"hash": r["hash"], "lesson": (r.get("summary") or "")[:160]} for r in rolled],
                            "rules_before": before.count("\n- "), "rules_after": after.count("\n- ")})
    if reverts:
        check("applying the reverts retracts the lessons", len(rolled) >= len(reverts) and after.count("\n- ") < before.count("\n- "),
              "%d -> %d rule(s) in the prompt" % (before.count("\n- "), after.count("\n- ")))

    # 4. Read again under what remains (arm R): the receipt.
    if reverts and not args.no_read:
        agent_argv = os.environ["AGENT_CMD"].split()
        _, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval, holdout="unseen")
        with open(os.path.join(args.workdir, "eval.jsonl"), "w", encoding="utf-8") as jf:
            trials_r, usage_r = evalrun.run_arm("R", profile, after, heldout, agent_argv, jf)
        s_r = evalrun.journal_eval_run(db, evalset, "eval-r", trials_r, note="after the measured reverts", at_ms=t_pass + 2 * HOUR)
        final = journaled[-1]
        report["steps"].append({"step": "measure-R", "summary": s_r, "usage": usage_r, "final_before_revert": final})
        check("R recovers past the final read", s_r["exact"] > final["exact"], "final %d -> R %d exact of %d" % (final["exact"], s_r["exact"], s_r["total"]))
        with open(os.path.join(args.workdir, "verify.trials.json"), "w", encoding="utf-8") as fh:
            json.dump(trials_r, fh, indent=1)
    elif not reverts:
        print("  no regression measured; nothing to revert")

    report["all_ok"] = all(c["ok"] for c in report["checks"].values())
    json.dump(report, open(os.path.join(args.workdir, "verify.summary.json"), "w"), indent=1)
    print("\nverify: %s -- %s" % ("ALL CHECKS PASSED" if report["all_ok"] else "CHECKS FAILED", os.path.join(args.workdir, "verify.summary.json")))
    return 0 if report["all_ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
