#!/usr/bin/env python3
"""sanctions screening: the whole agent, one file, embedded Areev.

The difference from `invoice-to-accounting` is WHERE THE LOGIC LIVES. The
screening rule is not a handler in this file and not a `--tool-cmd` script:
it is `../src/screen.py`, stored in the memory as a content-addressed CAS
blob and named by the `screen` Tool definition's `executor_uri`. The host
authorizes exactly those bytes with a pin computed from the checkout, so
code and authorization stay separate -- the blob travels in a bundle, the
permission never does.

Two subcommands are subprocess seams the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). They never open the memory --
the party that spawned them is holding it:

    agent.py tools        the host tools     ($AREEV_TOOL_NAME picks one)
    agent.py connector    the payment queue  (fixtures in, items+cursor out)

Everything else is the driver:

    agent.py seed         author the plan, the tool definitions, the code
                          blob, the saved CAL queries, the queue trigger
    agent.py pin          the executor address, derived from ../src/
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick)
    agent.py ingest --unpinned    a tick with NO pin: refuses, holds the cursor
    agent.py pin-check    start one payment unpinned -- must refuse (RUN-E018)
    agent.py trigger-state        what has actually fired, and what failed
    agent.py await-due [SECS]     block until every trigger is eligible
    agent.py asks         the parked runs waiting on a compliance officer
    agent.py decide FILE  apply an officer's decision to its parked run
    agent.py improve      the loop reads the desk's own history back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py revise       seed the REVISED rule -- a new content address
    agent.py provenance   which rule version screened which payment
    agent.py teach NS SUBJECT RELATION OBJECT
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the acts assert on this)

To make it real, replace `tools` and `connector` with processes that call
your payment rail and your case manager. The plan, the journal, the
approval gate, the pin and the audit trail do not change.
"""

import hashlib
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
SRC = os.path.join(EXAMPLE, "src")
FIXTURES = os.environ.get("PAY_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
QUEUE = os.path.join(FIXTURES, "payments")
PAY_UPTO = os.environ.get("PAY_UPTO", "04")   # the acts advance this "clock"
# Which rule the OPERATOR'S CHECKOUT holds. The pin derives from this file,
# so "syncing the checkout" after an approved revision is what re-arms the
# desk -- and until it happens, the runtime refuses rather than drifting.
RULE_FILE = os.environ.get("RULE_FILE", "screen.py")
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
LEDGER = os.path.join(OUT, "ledger.jsonl")
CASES = os.path.join(OUT, "cases.jsonl")

NS = "org.ops"               # plan, tool definitions, triggers, journals
PSP = "org.psp"              # the desk's own rules
PARTIES = "org.psp.counterparties"   # what officers taught it
DESK = "agent:screening-desk"        # the agent -- it can never approve
QUEUE_SCOPE = "queue:payments-out"

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# Above this score a person must look. It is a FACT in org.psp, not a
# constant here -- which is what lets the loop propose changing it.
DEFAULT_MATCH_FLOOR = 0.45


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def rule_bytes(name=None):
    name = name or RULE_FILE
    with open(os.path.join(SRC, name), "rb") as fh:
        return fh.read()


def rule_address(name=None):
    """The pin, computed from the workshop -- never read out of the memory.

    put_blob stores exactly these bytes under sha256 of exactly these bytes,
    so the host can authorize the code in its own checkout without opening
    the file. If the memory's rule has moved ahead (an applied revision),
    this address no longer covers it and the run refuses -- loudly.
    """
    return hashlib.sha256(rule_bytes(name or RULE_FILE)).hexdigest()


def marker(payment_id):
    return hashlib.sha256(payment_id.encode()).hexdigest()[:12]


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state: the queue item under "item", the
# trigger's declared context under "context". Never opens the memory.

def walk_grains(node, out):
    if isinstance(node, dict):
        if "relation" in node and "subject" in node:
            out.append(node)
        for v in node.values():
            walk_grains(v, out)
    elif isinstance(node, list):
        for v in node:
            walk_grains(v, out)
    return out


def tool_main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    grains = walk_grains(state.get("context") or {}, [])
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "triage":
        # The floor is a fact delivered through the trigger's declared
        # context, not a constant -- so the loop can propose moving it.
        floor = DEFAULT_MATCH_FLOOR
        for g in grains:
            if g.get("relation") == "match_floor" and g.get("subject") == "screening":
                floor = float(g["object"])
        score = float(state.get("match_score") or 0.0)
        prior = state.get("prior_disposition")
        needs_review = score >= floor and not prior
        emit({
            "needs_review": needs_review,
            "case_key": "%s#%s" % (item.get("payment_id", "?"), state.get("rule_version")),
            "review_reason": ("prior disposition on file" if prior else
                              ("possible list match" if needs_review else "no match")),
            "match_floor": floor,
        })

    elif tool == "open_case":
        # Always the compliance queue, never the counterparty. The marker in
        # the subject is how a decision finds its run again.
        append(CASES, {
            "case_key": state.get("case_key"),
            "marker": marker(item.get("payment_id", "?")),
            "counterparty": item.get("counterparty"),
            "amount": item.get("amount"),
            "currency": item.get("currency"),
            "match_name": state.get("match_name"),
            "match_id": state.get("match_id"),
            "match_score": state.get("match_score"),
            "rule_version": state.get("rule_version"),
            "decide_with": "release | block | false_positive (+ Because: ...)",
        })
        emit({"case_opened": True})

    elif tool == "record_disposition":
        # The officer called it a false positive. The DRIVER writes the fact
        # afterwards -- a tool process must never open the memory the
        # runtime is holding.
        emit({"disposition": "false_positive",
              "disposition_for": item.get("counterparty"),
              "disposition_list_id": state.get("match_id")})

    elif tool == "release":
        row = {
            "payment_id": item.get("payment_id"),
            "counterparty": item.get("counterparty"),
            "amount": item.get("amount"),
            "currency": item.get("currency"),
            "rule_version": state.get("rule_version"),
            "match_score": state.get("match_score"),
            "outcome": "released",
            "released_by": state.get("responder", "auto"),
        }
        append(LEDGER, row)
        emit({"released": 1, "payment_id": row["payment_id"]})

    elif tool == "block":
        append(LEDGER, {
            "payment_id": item.get("payment_id"),
            "counterparty": item.get("counterparty"),
            "amount": item.get("amount"),
            "currency": item.get("currency"),
            "rule_version": state.get("rule_version"),
            "match_id": state.get("match_id"),
            "outcome": "blocked",
            "blocked_by": state.get("responder", "auto"),
        })
        emit({"blocked": 1})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the connector seam -----------------------------------------------------
# An ABSENT cursor means "seed and fire nothing", so declaring a trigger
# never replays the queue. PAY_UPTO is the clock the acts advance.

def connector_main():
    req = json.load(sys.stdin)
    with open(os.path.join(FIXTURES, "watchlist.json")) as fh:
        watchlist = json.load(fh)
    names = sorted(n for n in os.listdir(QUEUE)
                   if n.endswith(".json") and n[:2] <= PAY_UPTO)
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(QUEUE, name)) as fh:
            payload = json.load(fh)
        # The list is external data and rides with the item; the learned
        # dispositions come from memory, through the context query.
        payload["watchlist"] = watchlist
        items.append({"id": payload["payment_id"], "payload": payload})
    emit({"items": items, "cursor": str(consumed + len(items)),
          "more": consumed + len(items) < len(names)})
    return 0


# -- the driver -------------------------------------------------------------

def open_db(actor=DESK):
    import areev
    os.makedirs(OUT, exist_ok=True)
    return areev.Areev(DB, ns=NS, actor=actor)


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


def seed():
    db = open_db()

    # 1. the code, as a grain. put_blob is idempotent: the address IS the
    #    content, so re-seeding unchanged bytes stores nothing new.
    uri = db.put_blob(rule_bytes())

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    # 2. the definitions. `screen` is code-carrying; the rest are host tools.
    screen = tool_def("screen", "match the counterparty against the list",
                      executor_uri=uri, runtime="native")
    triage = tool_def("triage", "decide whether a compliance officer must look")
    open_case = tool_def("open_case", "open a case for the compliance queue")
    review = tool_def("officer_review", "an officer decides: release, block, "
                      "or record a false positive", executor_kind="client")
    disposition = tool_def("record_disposition", "note a signed-off false positive")
    release = tool_def("release", "release the payment to the rail")
    block = tool_def("block", "stop the payment and file the case")

    wf = db.add("workflow", json.dumps({
        "name": "sanctions-screening",
        "nodes": ["screen", "triage", "open_case", "officer_review",
                  "record_disposition", "release", "block"],
        "edges": [
            {"src": "screen", "dst": "triage"},
            {"src": "triage", "dst": "release", "cond": "needs_review == false"},
            {"src": "triage", "dst": "open_case", "cond": "needs_review == true"},
            {"src": "open_case", "dst": "officer_review"},
            {"src": "officer_review", "dst": "release", "cond": 'decision == "release"'},
            {"src": "officer_review", "dst": "block", "cond": 'decision == "block"'},
            {"src": "officer_review", "dst": "record_disposition",
             "cond": 'decision == "false_positive"'},
            {"src": "record_disposition", "dst": "release"},
        ],
        "bindings": {"screen": screen, "triage": triage, "open_case": open_case,
                     "officer_review": review, "record_disposition": disposition,
                     "release": release, "block": block},
        "retries": {"screen": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "screening-judgment",
        "description": "how this desk reads a possible list match",
        "instructions": "A false clear is the expensive failure; a stopped "
                        "payment is the cheap one. Never screen a name you "
                        "cannot read. A disposition an officer signed applies "
                        "to that counterparty only, never to the whole list "
                        "entry.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The desk's own rules, and what officers have taught it.
    db.add_fact("screening", "match_floor", str(DEFAULT_MATCH_FLOOR),
                ns=PSP, idempotent=True)
    db.add_fact("screening", "list_version", "2026-08-01", ns=PSP, idempotent=True)

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE case_line AS '
           '"- {{subject}} {{relation}} {{object}} ({{confidence}})"')
    db.cal('DEFINE QUERY "screen_ctx"($session) '
           'DESCRIPTION "what the screening rule should know before it runs" '
           'AS { ASSEMBLE "screen_ctx" FROM '
           'judgment: (RECALL skills LIMIT 2), '
           'desk: (RECALL facts WHERE namespace = "org.psp.*" LIMIT 200) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, rule, dispositions" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'lessons: (RECALL facts WHERE namespace = "org.psp.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    trigger = db.trigger_add(json.dumps({
        "kind": "polling",
        "connector": "mock",
        "scope": QUEUE_SCOPE,
        "interval_secs": 1,
        "workflow": wf,
        "dedup_key": ["/payment_id"],
        "context_query": "screen_ctx($session = /payment_id)",
    }), "screen every outbound payment before it leaves", NS)

    emit({"workflow": wf, "trigger": trigger, "rule": uri,
          "pin": rule_address()})
    return 0


def pin():
    emit({"pin": rule_address(), "uri": "cas://sha256:%s" % rule_address()})
    return 0


def ingest(argv):
    """One heartbeat tick. The host authorizes exactly the bytes in ../src/.

    `--unpinned` withholds the pin, which is what an operator's machine looks
    like when the memory's rule has moved ahead of their checkout. Note what
    that does to the TRIGGER: the firing records the RUN-E018, increments
    consecutive_failures, backs off, and HOLDS the cursor -- the payments
    survive until pin and memory agree. Safe, but doing nothing, which is
    why `trigger status` is the thing to watch.
    """
    unpinned = "--unpinned" in argv
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        allow_executor=None if unpinned else rule_address(),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))
    emit(report)
    return 0


def pin_check():
    """Prove the pin governs execution, without disturbing the trigger.

    Starts one payment directly with NO host pin. The runtime refuses before
    the first journal write: the blob travels with the memory, the
    authorization to run it never does.
    """
    db = open_db()
    heads = json.loads(db.cal('RECALL workflows LIMIT 5 FORMAT json'))["grains"]
    wf = next(g["hash"] for g in heads
              if g["fields"].get("name") == "sanctions-screening")
    with open(os.path.join(QUEUE, sorted(os.listdir(QUEUE))[0])) as fh:
        payload = json.load(fh)
    try:
        db.run_start(workflow=wf, run_id="pin-check", tool_cmd=self_cmd("tools"),
                     input_json=json.dumps({"item": payload}))
    except ValueError as e:
        emit({"refused": True, "error": str(e)})
        return 0
    emit({"refused": False})
    return 1


def await_due(argv):
    """Block until every enabled trigger is eligible, then return.

    The desk's triggers are clocked (`interval_secs`), and after a refused
    start they also back off. Sleeping a guessed amount bets on how long the
    previous step took -- a bet that holds on an idle laptop and loses in CI
    behind ten other agents. Poll the evaluator's own predicate instead, and
    fail loudly by timeout naming what is still blocked.
    """
    timeout = float(argv[0]) if argv else 60.0
    db = open_db()
    deadline = time.time() + timeout
    while True:
        rows = [t for t in json.loads(db.trigger_status())
                if t.get("enabled") and not t.get("paused")]
        blocked = [t for t in rows if not t.get("due")]
        if not blocked:
            emit({"due": len(rows), "waited_sec": round(
                timeout - (deadline - time.time()), 2)})
            return 0
        if time.time() >= deadline:
            sys.stderr.write(
                "still not due after %ss: %s\n" % (timeout, json.dumps(
                    [{"trigger": t["trigger"][:12],
                      "next_due_at": t.get("next_due_at"),
                      "consecutive_failures": t.get("consecutive_failures"),
                      "last_error": (t.get("last_error") or "")[:120]}
                     for t in blocked])))
            return 6
        time.sleep(0.25)


def trigger_state():
    db = open_db()
    emit(json.loads(db.trigger_status()))
    return 0


def pending_asks(db):
    out = []
    for run_id in json.loads(db.run_list(100)):
        inspect = json.loads(db.run_inspect(run_id))
        if inspect.get("phase") != "open":
            continue
        for ask_id, entry in (inspect.get("pending_asks") or {}).items():
            out.append((run_id, ask_id, (entry.get("ask") or {}).get("input") or {}))
    return out


def asks():
    db = open_db()
    rows = []
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        rows.append({"run_id": run_id, "ask": ask_id,
                     "marker": marker(item.get("payment_id", "?")),
                     "counterparty": item.get("counterparty"),
                     "match_name": state.get("match_name"),
                     "match_score": state.get("match_score"),
                     "reason": state.get("review_reason")})
    emit(rows)
    return 0


def decide(path):
    """An officer's decision, read from a fixture the way a case-manager
    webhook would deliver it."""
    with open(path) as fh:
        note = json.load(fh)
    principal = note.get("officer", "user:unknown")
    ref = note.get("marker")
    verdict = note.get("decision")
    because = note.get("because", "")
    if verdict not in ("release", "block", "false_positive") or not because:
        sys.stderr.write("a screening decision needs a verdict and a reason\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        if marker(item.get("payment_id", "?")) != ref:
            continue
        result = {"decision": verdict, "responder": principal, "because": because}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        # A signed-off false positive is the lesson worth keeping: it clears
        # THIS counterparty next time, without clearing the list entry.
        if verdict == "false_positive":
            db.add_fact(item.get("counterparty"), "mg:screened_clear",
                        state.get("match_id") or "unknown",
                        ns=PARTIES, idempotent=True)
            db.record_tool_call(
                "screen", "fp:%s" % (state.get("match_id") or "unknown"),
                is_error=False, run_id=run_id)
        outcome = json.loads(db.run_resume(
            run_id, tool_cmd=self_cmd("tools"), allow_executor=rule_address()))
        emit({"run_id": run_id, "decision": verdict, "responder": principal,
              "outcome": outcome})
        return 0
    sys.stderr.write("no parked run matches marker %s\n" % ref)
    return 5


def improve():
    db = open_db()
    # Tune the analyzers to this desk's volume -- a recorded act of
    # configuration, not a fork.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_failure_ratio": 0.3}))
    report = json.loads(db.loop_run(llm_cmd=os.environ.get("LOOP_LLM_CMD")))
    recs = json.loads(db.recommendations('{"status": "pending"}'))
    emit({"loop": report,
          "pending": [{"hash": r.get("hash"), "severity": r.get("severity"),
                       "summary": r.get("summary"), "analyzer": r.get("analyzer"),
                       "target": r.get("target_ref")} for r in recs]})
    return 0


def govern(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: govern <rec> approve|apply|dismiss "
                         "--because ... --as user:X\n")
        return 2
    rec_prefix, action = argv[0], argv[1]
    because = actor = None
    it = iter(argv[2:])
    for flag in it:
        if flag == "--because":
            because = next(it, None)
        elif flag == "--as":
            actor = next(it, None)
    if not because:
        sys.stderr.write("a decision with no written reason is refused\n")
        return 2
    db = open_db(actor=actor or "user:anonymous")
    rec = next((r["hash"] for r in json.loads(db.recommendations(None))
                if r["hash"].startswith(rec_prefix)), rec_prefix)
    try:
        if action == "approve":
            out = db.approve_recommendation(rec, because)
        elif action == "apply":
            out = db.apply_recommendation(rec, because)
        elif action == "dismiss":
            out = db.dismiss_recommendation(rec, because)
        else:
            sys.stderr.write("unknown action %r\n" % action)
            return 2
    except ValueError as e:
        sys.stderr.write("refused: %s\n" % e)
        return 4
    print(out)
    return 0


def revise():
    """Seed the REVISED rule, and walk every reference forward.

    A revision is not one supersession, it is a chain -- because everything
    downstream names its input by content address:

        new bytes  -> new blob address
                   -> the Tool definition must be superseded to name it
                   -> the Workflow binds tools BY HASH, so the plan must be
                      superseded too, which mints a NEW PLAN HASH
                   -> triggers do NOT follow supersession heads, so the
                      trigger must be re-pointed at the new plan

    Skip any link and nothing happens: the old plan keeps binding the old
    definition, which keeps naming the old blob, and the desk goes on running
    the rule you thought you replaced. (Cursor and dedup survive the
    re-point -- they are keyed on the root of the trigger's chain.)
    """
    db = open_db()
    uri = db.put_blob(rule_bytes("screen_v2.py"))

    tools = json.loads(db.cal('RECALL tools WHERE kind = "definition" '
                              'LIMIT 50 FORMAT json'))["grains"]
    head = next(g for g in tools if g["fields"].get("tool_name") == "screen")
    new_tool = db.supersede(head["hash"], "tool", json.dumps({
        "tool_name": "screen", "kind": "definition",
        "tool_description": "match the counterparty against the list",
        "executor_uri": uri, "runtime": "native", "created_at": EPOCH_MS,
    }), ns=NS)

    plans = json.loads(db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
    plan = next(g for g in plans
                if g["fields"].get("name") == "sanctions-screening")
    fields = dict(plan["fields"])
    for drop in ("namespace", "type", "confidence"):
        fields.pop(drop, None)
    fields["bindings"] = dict(fields["bindings"], screen=new_tool)
    new_plan = db.supersede(plan["hash"], "workflow",
                            json.dumps(fields), ns=NS)

    triggers = json.loads(db.trigger_list())
    old_trigger = triggers[0]["trigger"]
    show = json.loads(db.trigger_show(old_trigger))
    tfields = {"kind": "polling", "connector": "mock", "scope": QUEUE_SCOPE,
               "interval_secs": 1, "workflow": new_plan,
               "dedup_key": ["/payment_id"],
               "context_query": "screen_ctx($session = /payment_id)"}
    new_trigger = db.supersede(old_trigger, "trigger",
                               json.dumps(tfields), ns=NS)

    emit({"rule": uri, "pin": rule_address("screen_v2.py"),
          "tool": new_tool, "superseded_tool": head["hash"],
          "workflow": new_plan, "superseded_workflow": plan["hash"],
          "trigger": new_trigger, "superseded_trigger": old_trigger,
          "cursor_kept": show.get("cursor")})
    return 0


def provenance():
    """Which rule version screened which payment -- the question an examiner
    asks, answered from the content address."""
    db = open_db()
    rows = []
    for line in open(LEDGER, encoding="utf-8") if os.path.exists(LEDGER) else []:
        row = json.loads(line)
        rows.append({"payment_id": row["payment_id"],
                     "outcome": row["outcome"],
                     "rule_version": row.get("rule_version")})
    heads = json.loads(db.cal('RECALL tools WHERE kind = "definition" '
                              'LIMIT 50 FORMAT json'))["grains"]
    screen = [g for g in heads if g["fields"].get("tool_name") == "screen"]
    emit({"decisions": rows,
          "rule_addresses": [g["fields"].get("executor_uri") for g in screen],
          "runs_touching_rule": json.loads(
              db.runs_touching(screen[0]["hash"]))["runs"] if screen else []})
    return 0


def teach(argv):
    if len(argv) != 4:
        sys.stderr.write("usage: teach NS SUBJECT RELATION OBJECT\n")
        return 2
    ns, subject, relation, obj = argv
    db = open_db()
    print(db.add_fact(subject, relation, obj, ns=ns, idempotent=True))
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.psp.*" LIMIT 20 '
                 'FORMAT TEMPLATE case_line'))
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal(
        'RECALL observations WHERE namespace = "agent:harness" RECENT 200 FORMAT json'))
    outcome = {}
    for g in obs.get("grains") or []:
        fields = g.get("fields") or {}
        if fields.get("observation_kind") == "run_outcome":
            outcome[fields.get("run_id")] = fields.get("object")
    emit([{"run_id": r, "outcome": outcome.get(r, "open")}
          for r in json.loads(db.run_list(100))])
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "tools":
        return tool_main()
    if cmd == "connector":
        return connector_main()
    if cmd == "seed":
        return seed()
    if cmd == "pin":
        return pin()
    if cmd == "pin-check":
        return pin_check()
    if cmd == "trigger-state":
        return trigger_state()
    if cmd == "await-due":
        return await_due(sys.argv[2:])
    if cmd == "ingest":
        return ingest(sys.argv[2:])
    if cmd == "asks":
        return asks()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(sys.argv[2:])
    if cmd == "revise":
        return revise()
    if cmd == "provenance":
        return provenance()
    if cmd == "teach":
        return teach(sys.argv[2:])
    if cmd == "brief":
        return brief()
    if cmd == "runs":
        return runs()
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
