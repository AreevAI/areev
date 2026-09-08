#!/usr/bin/env python3
"""Keyless proof that the governed learning pass really is a governed run.

    python3 scripts/selftest_run.py

No model, no network, no API key. It drives the whole workflow end to end on
a seeded receipts memory and checks the four claims that make "governed" mean
something more than a Python function that happens to be named `review`:

  1. THE RUN PARKS. `propose` executes, then the run stops at the client node
     and returns a `requires_action` envelope with an ask. Nothing is applied
     while it waits.
  2. SELF-APPROVAL IS REFUSED. Answering the ask as the principal that
     triggered it is refused by the RUNTIME, before any policy check. This is
     the separation of duties the harnesses used to implement by convention.
  3. THE DECISION IS THE ONLY LEVER. A decision drives the `decided == true`
     edge, `apply` records it, and an APPROVED rule reaches the agent's
     prompt. Nothing reaches it while the gate waits.
  4. THE PASS REPLAYS. `run_verify` byte-compares a replay of the journal, so
     a learning claim can be checked against the journal rather than against
     the harness's own log lines.

It proves plumbing and governance, never learning: with no `llm_cmd` the loop
runs its deterministic analyzers only.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import bench_govern  # noqa: E402
import bench_run  # noqa: E402

FAILURES: list[str] = []


def check(name, ok, detail=""):
    print(("  ok   " if ok else "  FAIL ") + name + (("  " + str(detail)[:200]) if detail else ""))
    if not ok:
        FAILURES.append(name)


def seed_receipts(mem, db_path):
    """A memory with enough shape for the deterministic analyzers to have
    something to say, and one rule already in force."""
    def go(db):
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "lesson",
                                   "object": "always capture the Category field"}), ns=mem.NS)
        for seq in range(6):
            db.add("observation", json.dumps({
                "content": "you missed the Category again", "observer_id": mem.REVIEWER,
                "observer_type": "human", "subject": "receipt_capture", "seq": seq}), ns=mem.NS)
            db.add("fact", json.dumps({"subject": "document_%04d" % seq,
                                       "relation": "total", "object": "%d.00" % (seq + 1)}),
                   ns=mem.NS)
            db.record_tool_call("capture", "parked: no total found", True,
                                thread="doc-%d" % seq, input=json.dumps({"seq": seq}),
                                status="failed", failure_cause="executor_error")
    mem.with_memory(db_path, mem.RUNNER, go)


def main():
    root = tempfile.mkdtemp(prefix="areev-bench-run-")
    try:
        return run(root)
    finally:
        shutil.rmtree(root, ignore_errors=True)


def run(root):
    mem = bench_govern.harness("receipts")
    agent_db = os.path.join(root, "ledger.db")
    runs_db = os.path.join(root, "runs.db")
    seed_receipts(mem, agent_db)

    tool_cmd = "%s %s --harness receipts" % (
        sys.executable, os.path.join(HERE, "bench_govern.py"))

    before = mem.with_memory(agent_db, mem.RUNNER, mem.lessons_markdown)

    # -- 1. the run parks on the human gate --------------------------------
    parked = {}

    def peek(ask_input):
        parked.update(ask_input or {})
        return {"decided": False, "approved": False, "decisions": []}

    def pass_one(db):
        return bench_run.govern(
            db, "learn-1", tool_cmd, peek,
            responder=mem.REVIEWER, agent_db=agent_db, workdir=root)

    report = bench_run.with_runs(runs_db, mem.RUNNER, pass_one)
    check("the run parks on the human gate and asks", report["asks"] == 1,
          json.dumps({k: report[k] for k in ("asks", "kind", "finished")}))
    check("`propose` ran before the gate and its result reached the ask",
          "pending" in parked or "count" in parked, sorted(parked)[:8])
    # The gate is handed the HISTORY too, not just the batch: what this
    # reviewer already decided, and the held-out outcome series. A reviewer
    # that sees only the batch will re-approve a reworded restatement of
    # something it declined -- the reword carries a different `dedup_key`, so
    # the engine's rejection cooldown never sees it.
    check("the ask carries what the reviewer already decided",
          "prior" in parked, sorted(parked))
    check("and the outcome series the Verify gate reads",
          "outcomes" in parked, sorted(parked))

    def unchanged(db):
        return mem.lessons_markdown(db)

    check("nothing was applied while the gate waited",
          mem.with_memory(agent_db, mem.RUNNER, unchanged) == before,
          "%d chars, unchanged" % len(before))

    # -- 2. self-approval is refused by the runtime -------------------------
    refused = {"raised": None}

    def self_approve(ask_input):
        return {"decided": True, "approved": True, "decisions": []}

    def pass_two(db):
        try:
            return bench_run.govern(
                db, "learn-2", tool_cmd, self_approve,
                # The principal that STARTED the run is the one triggering the
                # ask, so answering as itself must be refused.
                responder=mem.RUNNER, agent_db=agent_db, workdir=root)
        except Exception as exc:  # noqa: BLE001
            refused["raised"] = "%s: %s" % (type(exc).__name__, str(exc)[:200])
            return None

    bench_run.with_runs(runs_db, mem.RUNNER, pass_two)
    check("the runtime refuses a responder equal to the triggering principal",
          refused["raised"] is not None, refused["raised"] or "it was ACCEPTED")

    # -- 3. approving is the only thing that changes the prompt -------------
    def approve_first(ask_input):
        pending = (ask_input or {}).get("pending") or []
        if not pending:
            return {"approved": False, "decisions": []}
        return {"decided": True, "approved": True,
                "decisions": [{"hash": pending[0]["hash"], "approved": True,
                               "why": "selftest: the reviewer approved this one"}]}

    def pass_three(db):
        return bench_run.govern(
            db, "learn-3", tool_cmd, approve_first,
            responder=mem.REVIEWER, agent_db=agent_db, workdir=root)

    third = bench_run.with_runs(runs_db, mem.RUNNER, pass_three)
    after = mem.with_memory(agent_db, mem.RUNNER, unchanged)
    if third["responded"] and third["responded"][0]["approved"]:
        check("an approved pass reaches `apply` and finishes",
              third.get("finished") in ("Completed", "completed"), json.dumps(third))
        # The real assertion: the approved rule is now IN the prompt, and it
        # was not there while the gate was waiting.
        results = bench_run.with_runs(
            runs_db, mem.RUNNER, lambda db: bench_run.node_results(db, "learn-3"))
        applied_n = int((results.get("apply") or {}).get("applied") or 0)
        check("`apply` recorded the approval in the journal", applied_n == 1,
              "applied=%d" % applied_n)
        check("the approved rule reached the prompt, and only then",
              after != before, "before=%dch after=%dch" % (len(before), len(after)))
    else:
        # The deterministic analyzers proposed nothing on this seed. That is a
        # real outcome, not a failure -- say so rather than passing silently.
        print("       (the analyzers proposed nothing on this seed; the approve "
              "path was not exercised)")

    # -- 4. the pass replays ------------------------------------------------
    def verify(db):
        return bench_run.verify(db, "learn-1"), bench_run.trace(db, "learn-1")

    verdict, tr = bench_run.with_runs(runs_db, mem.RUNNER, verify)
    check("the parked pass replays byte-for-byte", bool(verdict.get("verified")),
          json.dumps(verdict)[:200])
    steps = tr.get("trace") if isinstance(tr, dict) else tr
    check("the journal records the pass", bool(steps),
          "%d journal entries" % len(steps or []))

    # -- the plan and its trigger are IN the file ---------------------------
    def declared(db):
        return (json.loads(db.cal('RECALL workflows WHERE namespace = "%s" LIMIT 10 FORMAT json'
                                  % bench_run.RUNS_NS))["grains"],
                json.loads(db.trigger_list()))

    plans, triggers = bench_run.with_runs(runs_db, mem.RUNNER, declared)
    check("the plan is a Workflow grain in the memory", len(plans) >= 1, len(plans))
    check("re-authoring the plan mints one hash, so a trigger cannot be orphaned",
          len({p["hash"] for p in plans}) == 1,
          "%d distinct plan hashes" % len({p["hash"] for p in plans}))

    # -- 5. the harness's OWN learn() is this path, not a second one --------
    #
    # The point of the rewiring: `memory.learn()` no longer has an unjournaled
    # review loop of its own. Calling it must produce a run in the journal.
    pass2 = os.path.join(root, "pass2")
    os.makedirs(pass2, exist_ok=True)
    fresh_agent = os.path.join(pass2, "ledger.db")
    seed_receipts(mem, fresh_agent)
    report = mem.learn(None, fresh_agent, llm_cmd=None, ground_cmd=None,
                       judge=None, verbose=False)
    want = {"pending", "applied", "rejected", "errors", "funnel", "decisions"}
    check("memory.learn() returns the report shape it always returned",
          want <= set(report), "missing %s" % sorted(want - set(report)))
    check("memory.learn() names the run that produced it",
          bool(report.get("run_id")), report.get("run_id"))
    # With no judge, nothing can be approved -- and every rejection is still
    # recorded, which is the half of the ledger the `decided` edge protects.
    print("       report:", json.dumps({k: report[k] for k in
          ("pending", "applied", "rejected", "run_id")}),
          json.dumps(report["decisions"])[:200])
    check("with no reviewer, nothing is approved", report["applied"] == 0,
          "applied=%d" % report["applied"])
    turned_down = sum(1 for d in report["decisions"] if not d["approved"])
    check("but the rejections are still recorded",
          report["rejected"] == turned_down and turned_down > 0,
          "dismissed=%d judged=%d" % (report["rejected"], turned_down))
    journal = os.path.join(os.path.dirname(fresh_agent), "runs.db")
    check("the pass left a journal beside the memory", os.path.exists(journal), journal)

    # -- 6. a declined rule stays declined, however it is reworded ----------
    #
    # The engine's cooldown is keyed on `dedup_key`, so it cannot catch a
    # RESTATEMENT. This is the harness half of the same guarantee.
    declined = mem._declined_before({"prior": [
        {"status": "rejected",
         "summary": 'record lesson: "always capture the Category field on every receipt"'},
        {"status": "applied",
         "summary": 'record lesson: "park a document with no total"'},
    ]})
    check("only the REJECTED prior decisions are treated as declined",
          len(declined) == 1, json.dumps(declined))
    hit = __import__("cal_assemble").restates_a_decision(
        "capture the Category field on receipts, always",
        declined, mem.acct.normalize_rule, mem.acct._content_words)
    check("a reworded restatement of a declined rule is recognised",
          hit is not None, json.dumps(hit))
    miss = __import__("cal_assemble").restates_a_decision(
        "vendor name comes from the top line", declined,
        mem.acct.normalize_rule, mem.acct._content_words)
    check("an unrelated rule is not", miss is None, json.dumps(miss))

    print()
    if FAILURES:
        print("FAILED: %d check(s): %s" % (len(FAILURES), ", ".join(FAILURES)))
        return 1
    print("the governed learning pass is a journaled run: it parks on a human, "
          "refuses self-approval, and replays.")
    print("Plumbing and governance only -- no model was called, so this says "
          "nothing about learning.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
