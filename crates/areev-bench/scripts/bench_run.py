#!/usr/bin/env python3
"""The governed learning pass, as an `areev run` workflow.

Every harness in this crate does the same three things when it learns:

    propose  ->  review  ->  apply

and every harness drove them from a Python function, so the one part of the
loop that is a GOVERNANCE claim -- a human approving a rule, as a different
identity from the one that proposed it -- was a convention of the harness
rather than something the engine enforced and journaled. This module makes it
a Workflow grain the `areev run` driver executes:

  * `propose` is a host tool: the loop's analyzers over the agent's memory;
  * `review` is a **client** tool, so the run PARKS and waits. Answering it
    is `run_respond`, which structurally refuses a responder equal to the
    principal that triggered the ask -- separation of duties enforced by the
    runtime, not by the harness remembering to open a second handle;
  * `apply` runs on the `decided == true` edge and records EVERY decision --
    an approve through `apply_recommendation`, a reject through
    `dismiss_recommendation`, each with its reason. The edge is "the reviewer
    answered", not "the reviewer approved something": gating it on approval
    would drop the rejections, and the ledger showing what was turned down is
    half of what makes the gate evidence.

The held-out evaluation is deliberately NOT a node here. It is a separate
phase in each harness (`run.py`, `evaluate.py`), it runs on its own cadence,
and a node that skipped itself on every learn pass would be furniture.

Everything is journaled: `run_trace` shows what ran, `runs_touching` joins a
grain back to the pass that wrote it, and `run_verify` byte-compares a replay.
A published learning claim can now be checked against a run journal instead of
against the harness's own log lines.

## Two files, on purpose

The journal lives in its own memory (`runs.db`), NOT in the agent's. The
embedded backend is single-writer per file, so a host tool that opened the
agent memory while the driver held it would fail `STO-E002`. Two files also
keep the separation the harnesses already have: an eval score in the agent's
namespace is an agent that can read its own grade.

## Keyless

`propose` with no `llm_cmd` runs the deterministic analyzers only, so the
whole workflow -- park, respond, resume, verify -- exercises end to end with
no API key. That is what `selftest_run.py` does. It proves the plumbing and
the governance edge, never a learning claim.
"""
from __future__ import annotations

import json
import os
import shlex
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

# The journal's own namespace. `agent:*` is deliberately skipped by the loop's
# all-namespace evidence scan, which is what keeps a run journal from becoming
# evidence the next pass reflects over.
RUNS_NS = "agent:harness"
PLAN_NAME = "governed-learning"

# The three nodes. `review` is the only client node: it is the human gate, and
# the runtime parks on it.
NODES = ("propose", "review", "apply")


# Definition grains carry a `created_at`, so authoring the plan twice would
# mint two different Tool hashes, hence two different `bindings`, hence two
# different Workflow hashes -- and a Trigger points at a plan BY HASH and does
# not follow heads, so the second authoring would silently orphan it. Pinning
# the timestamp makes the plan what it should be: one address, forever, for
# everyone who authors it. Same technique as `examples/agents/`, where three
# language stacks mint one hash.
PLAN_EPOCH_MS = 1_756_000_000_000  # 2025-08-24T02:26:40Z, fixed by fiat


def _tool(db, name, description, executor_kind=None):
    fields = {"tool_name": name, "kind": "definition", "tool_description": description,
              "created_at": PLAN_EPOCH_MS}
    if executor_kind:
        fields["executor_kind"] = executor_kind
    return db.add("tool", json.dumps(fields), ns=RUNS_NS)


def author_plan(db):
    """Author (or re-author) the governed-learning Workflow. Returns its hash.

    Content-addressed: the same plan is the same hash forever, so calling this
    on every pass mints nothing new and every run points at one plan.
    """
    bindings = {
        "propose": _tool(db, "propose",
                         "run the loop's analyzers over the agent's memory and "
                         "return the pending recommendations"),
        "review": _tool(db, "review",
                        "a person decides which proposed rules take effect",
                        executor_kind="client"),
        "apply": _tool(db, "apply",
                       "apply the approved recommendations and dismiss the rest, "
                       "each with its stated reason"),
    }
    return db.add("workflow", json.dumps({
        "name": PLAN_NAME,
        "created_at": PLAN_EPOCH_MS,
        "nodes": list(NODES),
        "edges": [
            {"src": "propose", "dst": "review"},
            # The governance edge. `decided`, not `approved`: `apply` records
            # rejections too, and a pass that approved nothing still has a
            # ledger to write. A run where the reviewer answered nothing at
            # all -- there was nothing pending -- ends here, journaled.
            {"src": "review", "dst": "apply", "cond": "decided == true"},
        ],
        "bindings": bindings,
    }), ns=RUNS_NS)


def author_trigger(db, workflow_hash, every_episodes):
    """The cadence, as a Trigger grain rather than as a Python modulo.

    The harnesses learn "every N episodes" and that N lived in a command-line
    flag. As a Trigger it is data in the file: `trigger_list` shows it, the
    console draws it in the Workflows canvas beside the plan it starts, and a
    reader of the memory can see what was supposed to start the pass without
    reading the harness.

    `kind: manual` is the honest one. The eight kinds are interval, schedule,
    once, polling, memory, webhook, manual and composite; "every N episodes"
    is none of them -- an episode is not a clock tick and not a grain
    predicate the evaluator could count. So the grain DECLARES the rule and
    names the harness as its evaluator, rather than claiming a cadence
    `areev trigger run` would silently never fire.
    """
    return db.add("trigger", json.dumps({
        "name": "learn-every-%d-episodes" % every_episodes,
        "kind": "manual",
        "workflow": workflow_hash,
        "scope": "episodes:%d" % every_episodes,
    }), ns=RUNS_NS)


# --------------------------------------------------------------------------
# driving one pass
# --------------------------------------------------------------------------

def govern(db, run_id, tool_cmd, decide, responder, agent_db, workdir,
           input_extra=None, tool_env=None, on_event=None):
    """Run one governed learning pass and return the journal's own account.

    `decide(asks_input) -> {"approved": bool, "decisions": [...]}` is the
    human. It is called with what the run parked on, and its answer is
    recorded through `run_respond` under `responder` -- which the runtime
    refuses if it equals the principal that triggered the ask.
    """
    workflow = author_plan(db)
    payload = {"agent_db": agent_db, "workdir": workdir}
    payload.update(input_extra or {})

    session = json.loads(db.run_start(
        workflow=workflow, run_id=run_id, input_json=json.dumps(payload),
        tool_cmd=tool_cmd, tool_env=tool_env, on_event=on_event))

    out = {"run_id": run_id, "workflow": workflow, "asks": 0, "responded": [],
           "kind": _kind(session), "finished": session.get("finished")}

    # A run either finished or parked; a parked one carries its
    # `requires_action` envelope under `parked`. Asks are addressed by
    # `tool_call_id`, NEVER by index -- an index is a race with the scheduler.
    #
    # The loop is `while`, not `for`: a plan may park more than once, and
    # answering the first ask and walking away would leave a run waiting
    # forever while the harness reported success.
    while True:
        asks = _asks(session)
        if not asks:
            break
        for ask in asks:
            answer = decide(ask.get("input") or {})
            db.run_respond(run_id, ask["tool_call_id"], json.dumps(answer),
                           responder=responder)
            out["asks"] += 1
            out["responded"].append({"node": ask.get("node"),
                                     "approved": bool(answer.get("approved"))})
        # Responding and resuming are deliberately separate acts: recording a
        # human's answer must not hold the run's writer handle open while the
        # human thinks.
        session = json.loads(db.run_resume(run_id, tool_cmd=tool_cmd,
                                           tool_env=tool_env, on_event=on_event))

    out["kind"] = _kind(session)
    out["finished"] = session.get("finished")
    return out


def _asks(session):
    parked = session.get("parked") or {}
    return parked.get("asks") or []


def _kind(session):
    if session.get("finished"):
        return "finished"
    return (session.get("parked") or {}).get("kind")


def node_results(db, run_id, limit=80):
    """Each node's result, read back from the RUN JOURNAL.

    The journal is the audited record of what the pass did, so a harness that
    wants its own report reads it from there rather than from a side channel
    the run does not know about. Later entries win, which is what a re-entry
    generation should do.
    """
    out = {}
    for entry in json.loads(db.run_trace(run_id, limit)).get("trace", []):
        f = entry.get("fields") or {}
        name, body = f.get("tool_name"), f.get("tool_content")
        if not name or not body:
            continue
        try:
            out[name] = json.loads(body)
        except (TypeError, ValueError):
            out[name] = {"raw": body}
    return out


def learn(mem, db_path, llm_cmd, ground_cmd, decide, policy=None, verbose=True,
          full_sweep=False, runs_db=None, run_id=None, every_episodes=None):
    """One governed learning pass, executed as an `areev run`.

    THE path every harness's `learn()` takes -- there is no second,
    unjournaled one. `mem` is the track's bridge module (it supplies `NS`,
    `RUNNER`, `REVIEWER` and `policy_file`); `decide(pending) -> (approved,
    decisions)` is that track's reviewer, which judges but never applies:
    applying is the `apply` node's job, under the REVIEWER principal, after
    the runtime has already refused a self-approval.

    Returns the report shape every harness already returned -- pending,
    applied, rejected, errors, funnel, decisions -- plus the `run_id` that
    produced it, so a published count can be traced to a journal.
    """
    workdir = os.path.dirname(os.path.abspath(db_path))
    runs_db = runs_db or os.path.join(workdir, "runs.db")
    run_id = run_id or _next_run_id(runs_db)
    track = getattr(mem, "TRACK", None)
    if not track:
        raise RuntimeError("%s must declare TRACK (its directory name) to name "
                           "the --tool-cmd's harness" % getattr(mem, "__name__", mem))

    payload = {
        "llm_cmd": llm_cmd, "ground_cmd": ground_cmd, "full_sweep": bool(full_sweep),
        "policy": mem.policy_file(db_path, policy),
    }

    captured = {"decisions": [], "approved": 0}

    def gate(ask_input):
        # The WHOLE ask, not just the pending list: it also carries what this
        # reviewer already decided in the last 90 days and the held-out
        # outcome series, and a reviewer that ignores both will re-approve
        # what it declined.
        approved, decisions = decide(ask_input or {})
        captured["decisions"] = decisions
        captured["approved"] = approved
        # `decided` drives the edge: `apply` records rejections as well as
        # approvals, so a pass that approved nothing still has a ledger.
        return {"decided": bool(decisions), "approved": bool(approved),
                "decisions": [{"hash": d["hash"], "approved": bool(d["approved"]),
                               "why": d["why"]} for d in decisions]}

    def drive(db):
        author_plan(db)
        if every_episodes:
            author_trigger(db, author_plan(db), every_episodes)
        report = govern(db, run_id, tool_cmd(track), gate, responder=mem.REVIEWER,
                        agent_db=os.path.abspath(db_path), workdir=workdir,
                        input_extra=payload)
        return report, node_results(db, run_id)

    report, nodes = with_runs(runs_db, mem.RUNNER, drive)
    proposed = nodes.get("propose") or {}
    applied = nodes.get("apply") or {}
    out = {
        "pending": int(proposed.get("count") or 0),
        "applied": int(applied.get("applied") or 0),
        "rejected": int(applied.get("rejected") or 0),
        "errors": list(applied.get("errors") or []),
        "funnel": proposed.get("funnel"),
        "decisions": captured["decisions"],
        "run_id": run_id,
        "finished": report.get("finished"),
    }
    if verbose:
        if out["funnel"]:
            print("   funnel:", json.dumps(out["funnel"]))
        for d in out["decisions"]:
            if d["approved"]:
                print("   APPROVED %s" % (d.get("text") or "")[:100])
            else:
                print("   rejected (%s) %s" % (d["why"][:44], (d.get("text") or "")[:52]))
        for e in out["errors"]:
            print("   ERROR:", e)
    return out


def tool_cmd(track):
    """The `--tool-cmd` for one track. Absolute, and quoted: the runtime runs
    it through `/bin/sh -c`, so a path with a space would otherwise split."""
    return "%s %s --harness %s" % (
        shlex.quote(sys.executable),
        shlex.quote(os.path.join(HERE, "bench_govern.py")),
        shlex.quote(track))


def _next_run_id(runs_db):
    """A run id that does not collide with one already in the journal.

    Run ids are the addressing key for asks and for resume, so reusing one
    across passes would make the second pass's `run_respond` ambiguous.
    """
    if not os.path.exists(runs_db):
        return "learn-1"
    try:
        existing = with_runs(runs_db, "agent:harness",
                             lambda db: json.loads(db.run_list(500)))
    except Exception:  # noqa: BLE001 -- a fresh or unreadable journal starts at 1
        return "learn-1"
    used = {r.get("run_id") for r in (existing if isinstance(existing, list)
                                      else existing.get("runs", []))}
    n = 1
    while ("learn-%d" % n) in used:
        n += 1
    return "learn-%d" % n


def verify(db, run_id):
    """Byte-compare a replay of the pass. A learning claim whose journal does
    not replay is not a claim about anything."""
    return json.loads(db.run_verify(run_id))


def trace(db, run_id, limit=50):
    return json.loads(db.run_trace(run_id, limit))


def with_runs(runs_db, actor, fn):
    """Open the JOURNAL memory as `actor` and release the handle.

    Separate from every harness's own `with_memory`, which opens the AGENT
    memory: two files, and the driver must not be holding the one its host
    tools are about to open.
    """
    import areev
    import gc

    parent = os.path.dirname(os.path.abspath(runs_db))
    if parent:
        os.makedirs(parent, exist_ok=True)
    db = areev.Areev(runs_db, ns=RUNS_NS, actor=actor)
    try:
        return fn(db)
    finally:
        del db
        gc.collect()
