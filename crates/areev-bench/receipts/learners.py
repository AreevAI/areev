#!/usr/bin/env python3
"""What does each learner author from the SAME captured corrections?

The A/B/A/B evaluation answers "did the rules help". When the answer is
"barely", the next question is not a tuning knob but a diagnosis: over one
fixed memory, which rules does each candidate learner propose, and what does
the supervisor do with them? This runs that, cheaply — no held-out passes,
no agent calls, just the governed learn pass K times per model over copies of
one memory a real run left behind.

    learners.py --memory RUNDIR/ledger.db --out DIR --passes 5 \\
        --learner 'qwen3-30b=qwen/qwen3-30b-a3b-instruct-2507@coreweave/bf16' \\
        --learner 'gpt-oss-120b=openai/gpt-oss-120b@deepinfra/bf16'

One JSON row per pass: the DISCOVER funnel, every proposal with the
supervisor's verdict and reason, and — the column that matters here — whether
each approved rule NAMES A FIELD TO CAPTURE or only says how to write one
already captured. That distinction is the whole diagnosis: a corpus where the
agent never fills a field needs an additive rule, and a formatting rule
cannot supply it however cleanly it passes the gates (EXPENSE.md's second
defect).

Reads nothing about the held-out set and scores nothing. It explains a
result; it cannot improve one.
"""
import argparse
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ledger_profile
import memory as mem


def additive(text, fields):
    """Does this rule tell the agent to CAPTURE a field, rather than how to
    format one it already produced? Deliberately crude and stated in full:
    it names a ledger field AND an acquiring verb, and does not hedge the
    whole thing behind 'if the value already exists'."""
    t = (text or "").lower()
    names_field = any(f.lower() in t for f in fields)
    acquires = re.search(r"\b(capture|record|extract|include|add|read|put|fill|provide|"
                         r"always\s+\w+)\b", t) is not None
    conditioned_on_existing = re.search(
        r"if (a |an |the )?[\w\s]*\b(fact|value|field)\b[\w\s]*\b(exists|is present|is recorded)",
        t) is not None
    return bool(names_field and acquires and not conditioned_on_existing)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--memory", required=True, help="a ledger.db a run left behind")
    ap.add_argument("--out", required=True)
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--passes", type=int, default=5)
    ap.add_argument("--learner", action="append", required=True,
                    help="LABEL=model@provider (repeatable)")
    ap.add_argument("--ground", default="openai/gpt-4o-mini@openai")
    ap.add_argument("--objective", default="learner")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    fields = profile["fields"]
    os.makedirs(args.out, exist_ok=True)
    py = sys.executable
    scripts = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts")
    loop = os.path.join(os.path.abspath(scripts), "openrouter_loop.py")
    chat = os.path.join(os.path.abspath(scripts), "openrouter_toolcall.py")
    judge = mem.make_judge(os.environ.get("REVIEW_CMD"))
    gm, gp = args.ground.split("@")
    policy = json.dumps({"discover_objective": args.objective})

    summaries = []
    for spec in args.learner:
        label, rest = spec.split("=", 1)
        model, pin = rest.split("@")
        rows = []
        for p in range(1, args.passes + 1):
            work = os.path.join(args.out, label, "pass-%02d" % p)
            if os.path.exists(work):
                import shutil
                shutil.rmtree(work)
            os.makedirs(work)
            db = os.path.join(work, "ledger.db")
            mem.copy_memory(args.memory, db)
            res = mem.learn(
                profile, db,
                "%s %s %s --provider %s --seed %d" % (py, loop, model, pin, p),
                "%s %s %s --provider %s --seed %d" % (py, loop, gm, gp, p),
                judge, policy, verbose=False, policy_dir=work,
                # A full sweep, because every pass reflects over the same
                # already-seen history: without it the watermark leaves the
                # bundle empty and every model scores zero for the same
                # uninteresting reason.
                full_sweep=True)
            approved = [d for d in res["decisions"] if d["approved"]]
            row = {"learner": label, "model": model, "provider": pin, "pass": p,
                   "funnel": res["funnel"], "proposed": res["pending"],
                   "approved": res["applied"], "rejected": res["rejected"],
                   "additive_rules": sum(1 for d in approved if additive(d["text"], fields)),
                   "decisions": res["decisions"]}
            rows.append(row)
            with open(os.path.join(args.out, "%s.jsonl" % label), "a", encoding="utf-8") as fh:
                fh.write(json.dumps(row) + "\n")
            print("  %-16s pass %02d  proposed %d  approved %d (additive %d)  rejected %d"
                  % (label, p, row["proposed"], row["approved"], row["additive_rules"],
                     row["rejected"]))
        n = float(len(rows))
        s = {"learner": label, "model": model, "provider": pin, "passes": len(rows),
             "mean_proposed": sum(r["proposed"] for r in rows) / n,
             "mean_approved": sum(r["approved"] for r in rows) / n,
             "mean_additive": sum(r["additive_rules"] for r in rows) / n,
             "passes_with_an_additive_rule": sum(1 for r in rows if r["additive_rules"] > 0),
             "funnel_totals": {k: sum((r["funnel"] or {}).get(k, 0) for r in rows)
                               for k in ("evidence", "proposed", "cited", "grounded",
                                         "kept", "stored")}}
        summaries.append(s)
        print("%-16s approved %.2f/pass, additive %.2f/pass, %d/%d passes had one"
              % (label, s["mean_approved"], s["mean_additive"],
                 s["passes_with_an_additive_rule"], s["passes"]))

    with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as fh:
        json.dump({"memory": args.memory, "objective": args.objective,
                   "ground": args.ground, "learners": summaries}, fh, indent=1)
    print("\nwrote %s" % os.path.join(args.out, "summary.json"))


if __name__ == "__main__":
    sys.exit(main())
