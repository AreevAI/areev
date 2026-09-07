#!/usr/bin/env python3
"""The τ² retail run: deploy on an incomplete policy, learn from what goes wrong.

    run.py --workdir DIR [--withhold authenticate,modify_once]
           [--experience 30] [--eval 40] [--learn-every 5]

Phases, in order:

  A0        the held-out tasks, run by the agent as deployed — the redacted
            policy and nothing learned. Journaled as the evalset baseline.
  EXPERIENCE  the training tasks, in order. Every episode's tool calls, the
            customer's own pushback, and one outcome record go into memory.
            Every `--learn-every` episodes the loop reflects, the supervisor
            decides, and approved rules render into every later prompt.
  (eval.py then measures the held-out tasks at B, B2 and A.)

The withheld clauses are removed from BOTH the policy and the tool
descriptions the agent sees, and `--audit` asserts they are really gone
(redact.py explains why the policy alone is not enough). The environment,
the user simulator, the orchestrator and the evaluator are τ²-bench's own.
"""
import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import episode
import memory as mem
import redact


def evalset_hash(tasks):
    import hashlib
    h = hashlib.sha256()
    for t in tasks:
        h.update(str(t.id).encode()); h.update(b"\n")
    return h.hexdigest()[:16]


def add_common_args(ap):
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--withhold", default=",".join(redact.DEFAULT_WITHHELD),
                    help="comma list of clause names from redact.CLAUSES; '' withholds nothing")
    ap.add_argument("--split", default="base")
    ap.add_argument("--experience", type=int, default=30)
    ap.add_argument("--eval", type=int, default=40)
    ap.add_argument("--max-steps", type=int, default=60)
    ap.add_argument("--seed", type=int, default=300)


def setup(args):
    """The agent's view of the world, and the task split. Fails loudly if a
    withheld clause is still readable somewhere."""
    env = episode.build_env()
    full_policy = env.get_policy()
    tools = episode.tool_schemas(env)
    withheld = [w.strip() for w in args.withhold.split(",") if w.strip()]
    for w in withheld:
        if w not in redact.CLAUSES:
            raise SystemExit("unknown clause %r (known: %s)" % (w, ", ".join(redact.CLAUSES)))
    leaked = redact.audit(full_policy, tools, withheld)
    if leaked:
        raise SystemExit("withheld clause(s) still readable by the agent: %s" % ", ".join(leaked))
    policy = redact.redact_policy(full_policy, withheld)
    agent_tools = redact.redact_tools(tools, withheld)

    tasks = episode.load_tasks(args.split)
    # A fixed, seed-free order: the split is the benchmark's, and slicing it
    # by position keeps experience and held-out disjoint by construction.
    exp = tasks[:args.experience]
    held = tasks[args.experience:args.experience + args.eval]
    if len(held) < args.eval:
        raise SystemExit("split %r has %d tasks; need %d + %d"
                         % (args.split, len(tasks), args.experience, args.eval))
    return policy, agent_tools, withheld, exp, held, full_policy, tools


def run_arm(name, tasks, policy, tools, lessons, args, verbose=True):
    records = []
    if verbose:
        print("\n=== arm %s — %d rule(s) in the prompt" % (name, lessons.count("\n- ") + (1 if lessons.strip() else 0)))
    for t in tasks:
        rec, _calls = episode.run_task(
            t, policy, tools, os.environ["AGENT_CMD"], lessons,
            os.environ.get("USER_LLM", "openrouter/qwen/qwen3-30b-a3b-instruct-2507"),
            json.loads(os.environ.get("USER_LLM_ARGS", '{"temperature":0}')),
            max_steps=args.max_steps, seed=args.seed)
        rec["arm"] = name
        records.append(rec)
        if verbose:
            print("  task %-6s reward %.0f  %-22s turns %-3d tool-errors %d"
                  % (t.id, rec.get("reward", 0), rec.get("terminated", "")[:22],
                     rec.get("steps", 0), rec.get("tool_errors", 0)))
    solved = sum(1 for r in records if r.get("reward", 0) >= 1.0)
    if verbose:
        print("  arm %s: %d/%d solved (%.1f%%)" % (name, solved, len(records),
                                                   100.0 * solved / max(len(records), 1)))
    return records


def main():
    ap = argparse.ArgumentParser()
    add_common_args(ap)
    ap.add_argument("--learn-every", type=int, default=5)
    ap.add_argument("--measure", action="store_true",
                    help="give every applied rule the held-out set as its outcome metric")
    ap.add_argument("--journal-baseline", action="store_true", help="run and journal A0 first")
    ap.add_argument("--audit", action="store_true", help="print the leak report and exit")
    args = ap.parse_args()

    policy, agent_tools, withheld, exp, held, full_policy, full_tools = setup(args)
    if args.audit:
        print(json.dumps({"withheld": withheld,
                          "clauses_restated_by_tool_descriptions": redact.leak_report(full_tools),
                          "policy_chars": [len(full_policy), len(policy)]}, indent=1))
        return

    os.makedirs(args.workdir, exist_ok=True)
    db_path = os.path.join(args.workdir, "retail.db")
    if os.path.exists(db_path):
        raise SystemExit("%s exists — a stale memory would poison A0" % db_path)
    llm_cmd = os.environ.get("LOOP_LLM_CMD")
    ground_cmd = os.environ.get("LOOP_GROUND_CMD")
    judge = mem.make_judge(os.environ.get("REVIEW_CMD"))
    policy_json = os.environ.get("LOOP_POLICY") or None
    evalset = evalset_hash(held)
    if args.measure:
        pol = json.loads(policy_json) if policy_json else {}
        pol["outcome_evalset"] = {"hash": evalset, "field": "passed", "higher_is_better": True}
        policy_json = json.dumps(pol)

    with open(os.path.join(args.workdir, "run.config.json"), "w") as fh:
        json.dump({"withheld": withheld, "split": args.split, "evalset": evalset,
                   "experience_tasks": [t.id for t in exp], "held_out_tasks": [t.id for t in held],
                   "policy_chars": [len(full_policy), len(policy)],
                   "leak_report": redact.leak_report(full_tools),
                   "agent_cmd": os.environ.get("AGENT_CMD"), "user_llm": os.environ.get("USER_LLM"),
                   "loop_llm_cmd": llm_cmd, "loop_ground_cmd": ground_cmd,
                   "review_cmd": os.environ.get("REVIEW_CMD"), "policy": policy_json,
                   "max_steps": args.max_steps, "seed": args.seed}, fh, indent=1)
    print("withheld: %s | experience %d | held-out %d | evalset %s"
          % (", ".join(withheld) or "(nothing)", len(exp), len(held), evalset))

    journal = open(os.path.join(args.workdir, "journal.jsonl"), "a", encoding="utf-8")

    if args.journal_baseline:
        mem.with_memory(db_path, mem.REVIEWER, lambda db: None)
        recs = run_arm("A0", held, policy, agent_tools, "", args)
        for r in recs:
            journal.write(json.dumps(r) + "\n")
        journal.flush()
        summary = mem.journal_eval_run(db_path, evalset, "eval-a0", recs,
                                       note="as deployed, nothing learned")
        json.dump(recs, open(os.path.join(args.workdir, "a0.records.json"), "w"), indent=1)
        json.dump(summary, open(os.path.join(args.workdir, "a0.summary.json"), "w"), indent=1)
        print("journaled A0: %s" % json.dumps(summary))

    totals = {"episodes": 0, "solved": 0, "learn_passes": 0, "applied": 0, "rejected": 0,
              "prompt_tokens": 0, "completion_tokens": 0}
    since_learn = 0
    for i, t in enumerate(exp, start=1):
        lessons = mem.with_memory(db_path, mem.RUNNER, mem.lessons_markdown)
        rec, calls = episode.run_task(
            t, policy, agent_tools, os.environ["AGENT_CMD"], lessons,
            os.environ.get("USER_LLM", "openrouter/qwen/qwen3-30b-a3b-instruct-2507"),
            json.loads(os.environ.get("USER_LLM_ARGS", '{"temperature":0}')),
            max_steps=args.max_steps, seed=args.seed)
        rec["arm"] = "experience"
        totals["episodes"] += 1
        totals["solved"] += int(rec.get("reward", 0) >= 1.0)
        totals["prompt_tokens"] += rec["usage"]["prompt_tokens"]
        totals["completion_tokens"] += rec["usage"]["completion_tokens"]
        journal.write(json.dumps(rec) + "\n")
        journal.flush()
        print("%3d/%d task %-6s reward %.0f  %-20s tool-errors %d  rules=%d"
              % (i, len(exp), t.id, rec.get("reward", 0), rec.get("terminated", "")[:20],
                 rec.get("tool_errors", 0), lessons.count("\n- ") + (1 if lessons.strip() else 0)))

        mem.with_memory(db_path, mem.RUNNER, lambda db: mem.record_episode(db, rec, calls))
        since_learn += 1
        if since_learn >= args.learn_every:
            since_learn = 0
            res = mem.learn(db_path, llm_cmd, ground_cmd, judge, policy_json)
            totals["learn_passes"] += 1
            totals["applied"] += res["applied"]
            totals["rejected"] += res["rejected"]
            print("   loop: %d proposed, %d approved, %d rejected"
                  % (res["pending"], res["applied"], res["rejected"]))
            journal.write(json.dumps({"learn_after": t.id, **res}) + "\n")
            journal.flush()

    json.dump({"withheld": withheld, **totals},
              open(os.path.join(args.workdir, "experience.summary.json"), "w"), indent=1)
    print("\nexperience: %d episodes, %d solved | %d learn passes, %d applied, %d rejected"
          % (totals["episodes"], totals["solved"], totals["learn_passes"],
             totals["applied"], totals["rejected"]))


if __name__ == "__main__":
    sys.exit(main())
