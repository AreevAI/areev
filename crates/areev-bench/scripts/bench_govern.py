#!/usr/bin/env python3
"""The host-tool seam for the governed-learning workflow.

`areev run` executes a host tool by running ONE command per effect: the tool's
input JSON arrives on stdin, `$AREEV_TOOL_NAME` says which node is running,
and the result JSON leaves on stdout. This file is that command.

    python3 scripts/bench_govern.py --harness receipts     # invoked BY the run

It is never called by hand. Each track's own `govern.py` names it as its
`--tool-cmd`, and `bench_run.govern()` drives the pass.

## Why a subprocess and not a callback

The driver holds the JOURNAL memory's writer handle for the whole run. A host
tool that opened the AGENT memory in the same process would be a second handle
on a different file, which is fine -- but the loop pass inside `propose` opens
and closes that memory several times, and doing it inside the driver's frame
is exactly how a handle outlives its scope and the next open fails STO-E002.
A subprocess gets its own process, its own handles, and its own exit.

## What each node returns

  propose   {"pending": [...], "count": n, "funnel": {...},
             "prior": [...],     what the reviewer already decided (90d)
             "outcomes": [...]}  the held-out series the Verify gate reads
  apply     {"applied": n, "rejected": n, "decisions": [...], "errors": [...]}

`review` never reaches here: it is a client node, so the run parks and the
answer arrives through `run_respond`. There is no `evaluate` node — the
held-out pass is a separate phase on its own cadence, and a node that skipped
itself on every learn pass would be furniture.
"""
from __future__ import annotations

import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import cal_assemble as cal  # noqa: E402


def harness(track, name="memory"):
    """Load `<track>/<name>.py` under a unique module name.

    Every track calls its bridge `memory.py`, so a plain import binds whichever
    directory is first on the path.
    """
    import importlib.util

    key = "areev_bench_%s_%s" % (track, name)
    if key in sys.modules:
        return sys.modules[key]
    path = os.path.join(ROOT, track)
    if path not in sys.path:
        sys.path.insert(0, path)
    spec = importlib.util.spec_from_file_location(key, os.path.join(path, name + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[key] = module
    spec.loader.exec_module(module)
    return module


# --------------------------------------------------------------------------
# the nodes
# --------------------------------------------------------------------------

def node_propose(mem, payload):
    """Run the loop over the agent's memory and return what it proposed.

    Keyless when `llm_cmd` is absent: the deterministic analyzers still run,
    which is what lets the whole workflow be exercised in CI. That proves the
    plumbing and the governance edge. It proves nothing about learning.
    """
    db_path = payload["agent_db"]
    policy = mem.policy_file(db_path, payload.get("policy"))
    report = json.loads(mem.with_memory(
        db_path, mem.RUNNER,
        lambda db: db.loop_run(llm_cmd=payload.get("llm_cmd"),
                               ground_cmd=payload.get("ground_cmd"),
                               policy=policy,
                               full_sweep=bool(payload.get("full_sweep")))))

    def read(db):
        pend = json.loads(db.recommendations('{"status":"pending"}'))
        return [{"hash": r["hash"], "summary": r.get("summary") or "",
                 "target_ref": r.get("target_ref") or "",
                 "analyzer": r.get("analyzer") or ""} for r in pend]

    pending = mem.with_memory(db_path, mem.RUNNER, read)

    # What the reviewer is about to decide, AND what it already decided. A
    # reviewer that sees only the pending batch will approve a rewording of
    # something it declined last month: the reword carries a different
    # `dedup_key`, so the engine's rejection cooldown never sees it. The
    # outcome series rides along so a reviewer can tell whether the last
    # approvals moved anything before approving more.
    def context(db):
        # Not every track journals eval runs (receipts scores outside the
        # memory), so the harness namespace is optional -- but the default is
        # named rather than guessed, because a typo would mint one.
        harness_ns = getattr(mem, "HARNESS_NS", "agent:harness")
        return cal.review_history(db), cal.outcomes(db, harness_ns)

    prior, outcomes = mem.with_memory(db_path, mem.RUNNER, context)
    return {"pending": pending, "count": len(pending),
            "funnel": report.get("llm_funnel"),
            "prior": prior, "outcomes": outcomes}


def node_apply(mem, payload):
    """Apply what the reviewer approved; dismiss the rest with its reason.

    Runs under the REVIEWER actor: applying under a different identity from
    the one that proposed is the Review gate's separation of duties, and the
    runtime has already enforced the same rule on the ask this decision came
    from.
    """
    db_path = payload["agent_db"]
    decisions = payload.get("decisions") or []
    out = {"applied": 0, "rejected": 0, "errors": [], "decisions": []}

    def review(db):
        for d in decisions:
            why = (d.get("why") or "reviewed")[:200]
            try:
                if d.get("approved"):
                    db.apply_recommendation(d["hash"], why)
                    out["applied"] += 1
                else:
                    db.dismiss_recommendation(d["hash"], why)
                    out["rejected"] += 1
                out["decisions"].append({"hash": d["hash"],
                                         "approved": bool(d.get("approved")), "why": why})
            except ValueError as e:
                out["errors"].append(str(e)[:160])

    mem.with_memory(db_path, mem.REVIEWER, review)
    return out


NODES = {"propose": node_propose, "apply": node_apply}


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--harness", required=True,
                    help="track directory: receipts, tau2, appworld, …")
    args = ap.parse_args(argv)

    name = os.environ.get("AREEV_TOOL_NAME") or ""
    if name not in NODES:
        # A non-zero exit with the reason on stderr is a Failed effect the run
        # journals -- which is what should happen when a plan binds a node this
        # seam does not implement.
        sys.stderr.write("bench_govern: no such node %r; have %s\n"
                         % (name, ", ".join(sorted(NODES))))
        return 2

    payload = json.loads(sys.stdin.read() or "{}")
    mem = harness(args.harness)
    try:
        result = NODES[name](mem, payload)
    except Exception as exc:  # noqa: BLE001 -- the run wants the reason, not a traceback
        sys.stderr.write("bench_govern %s: %s: %s\n" % (name, type(exc).__name__, exc))
        return 1
    sys.stdout.write(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
