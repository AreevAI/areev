#!/usr/bin/env python3
"""trade surveillance: the whole agent, one file, embedded Areev.

The property this example exists for: A STANDING RULE THAT FIRES ON A
CO-OCCURRENCE, NOT ON AN EVENT.

Two feeds arrive independently -- an order-book feed and a disclosures feed.
Neither is interesting on its own: a big order is Tuesday, a rebalance
notice is Tuesday. What a surveillance analyst must look at is a big order
in an instrument AND a material event on that same instrument, close
together in time. That is a `composite` Trigger: two member triggers, a
gate over their aliases, a `correlate` pointer naming the field the two
signals must agree on, and a `window_ms` past which a half-match expires.

Two subcommands are subprocess seams the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). They never open the memory --
the party that spawned them is holding it:

    agent.py tools        the host tools    ($AREEV_TOOL_NAME picks one)
    agent.py connector    both feeds        (fixtures in, items+cursor out)

Everything else is the driver:

    agent.py seed         author the two plans, the tool definitions, the
                          instrument book, the saved CAL queries, and the
                          two member triggers + the composite gate
    agent.py gate         read the gate declaration back out of the memory
    agent.py gate-check   three composites that must be REFUSED at authoring
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick)
    agent.py await-due    block until every trigger is due again (no sleeps)
    agent.py await-window block until the live half-match is past the window
    agent.py firings      what the gate did, from its own journal
    agent.py trigger-state        cursors, failures, backoff
    agent.py cases        the cases parked for an analyst
    agent.py decide FILE  apply an analyst's disposition to its parked run
    agent.py improve      the loop reads the desk's own case record back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the acts assert on this)

To make it real, replace `connector` with a process that reads your order
management system and your news/disclosure vendor, and `tools` with one
that writes your case manager. The gate, the window, the journal, the
approval and the audit trail do not change.

THIS IS A TEACHING EXAMPLE OF THE MECHANISM. It is not a compliant trade
surveillance system: two correlated signals are not a market-abuse model,
and every instrument, issuer, desk, order and headline below is invented.
Symbols are venue-qualified (`MRDN:VNTG`) so they cannot collide with a
listed instrument.
"""

import json
import os
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("FEED_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
# The acts advance this "clock": a fixture whose 2-digit prefix is above it
# has not happened yet. Both feeds share one sequence, so the prefixes are
# the order the desk saw things in.
FEED_UPTO = os.environ.get("FEED_UPTO", "06")
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
TAPE = os.path.join(OUT, "tape.jsonl")        # the desk's own feed archive
CASES = os.path.join(OUT, "cases.jsonl")
ALERTS = os.path.join(OUT, "alerts.jsonl")
DISMISSALS = os.path.join(OUT, "dismissals.jsonl")

NS = "org.ops"                       # plans, tool definitions, triggers, journals
BOOK = "org.surv.book"               # what the desk knows about the instruments
PRECEDENTS = "org.surv.precedents"   # what analysts decided, and why
DESK = "agent:surveillance-desk"     # the agent -- it can never dispose of a case

ORDER_SCOPE = "feed:order-book"
NEWS_SCOPE = "feed:disclosures"
FEED_DIRS = {ORDER_SCOPE: "orders", NEWS_SCOPE: "news"}

# The gate's aliases. A content address is not a legal identifier in any
# expression grammar, so members are declared under names the gate can say.
ORDER_ALIAS = "order_burst"
NEWS_ALIAS = "material_event"

# How long a half-match stays live. Measured in EVALUATION wall-clock between
# member firings -- when the desk SAW each signal, not the timestamps inside
# them. Fifteen seconds is a teaching number chosen for HEADROOM, not realism:
# the act scripts have to fit two ticks inside it on a loaded CI machine. A
# real desk would use minutes, and the number is a surveillance-policy
# decision rather than a tuning knob.
WINDOW_MS = int(os.environ.get("GATE_WINDOW_MS", "15000"))

# The member triggers' poll cadence. `await-due` waits this out by ASKING the
# evaluator, never by sleeping a guess.
POLL_SECS = int(os.environ.get("FEED_POLL_SECS", "1"))

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def read_jsonl(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def when(iso):
    return datetime.strptime(iso, "%Y-%m-%dT%H:%M:%SZ")


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state. For a member run that is the feed item;
# for a case run it is `{"correlation": "<symbol>"}` plus the context the
# EVALUATOR assembled -- because on the embedded backend a tool inside a run
# cannot open the file that run is holding.

def context_facts(state):
    """The facts the trigger's declared context query put in front of us."""
    grains = ((state.get("context") or {}).get("grains")) or []
    return [g.get("fields") or {} for g in grains
            if (g.get("fields") or {}).get("type") == "fact"]


def tool_main():
    state = json.load(sys.stdin)
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "normalize_signal":
        # A member run. Its whole job is to put one feed event on the tape in
        # the desk's own vocabulary -- the gate has already counted the
        # firing by the time this runs.
        item = state["item"]
        feed = "order-book" if state.get("scope") == ORDER_SCOPE else "disclosures"
        if feed == "order-book":
            pattern = "%s_%s" % (item["order_type"], item["side"])
            detail = ("%s %s %s @ %s%% ADV by %s"
                      % (item["side"], item["qty"], item["symbol"],
                         item["pct_adv"], item["desk"]))
        else:
            pattern = item["category"]
            detail = item["headline"]
        append(TAPE, {"feed": feed, "symbol": item["symbol"],
                      "event_id": item["event_id"], "at": item["at"],
                      "pattern": pattern, "detail": detail,
                      "raw": item})
        emit({"recorded": 1, "symbol": item["symbol"], "pattern": pattern})

    elif tool == "assemble_case":
        # A case run. The firing item carries ONLY the correlation value --
        # one firing per correlated set, however many members contributed --
        # so the case is rebuilt from the tape and from declared context.
        symbol = state["item"]["correlation"]
        tape = [row for row in read_jsonl(TAPE) if row["symbol"] == symbol]
        order = next((r for r in reversed(tape) if r["feed"] == "order-book"), None)
        event = next((r for r in reversed(tape) if r["feed"] == "disclosures"), None)
        if not order or not event:
            sys.stderr.write("case %s is missing a leg: order=%r event=%r\n"
                             % (symbol, bool(order), bool(event)))
            return 1
        issuer = next((f["object"] for f in context_facts(state)
                       if f.get("subject") == symbol
                       and f.get("relation") == "mg:issuer"), symbol)
        lag = int((when(order["at"]) - when(event["at"])).total_seconds())
        emit({
            "case_ref": symbol,
            "symbol": symbol,
            "issuer": issuer,
            # The pattern signature, not the instrument: this is what makes
            # one case comparable to another, and what a precedent is about.
            "signature": "%s+%s" % (order["pattern"], event["pattern"]),
            "order": order["detail"],
            "order_id": order["event_id"],
            "desk": order["raw"]["desk"],
            "event": event["detail"],
            "event_id": event["event_id"],
            "materiality": event["raw"]["materiality"],
            # Negative = the order was placed BEFORE the event was public.
            "order_lag_seconds": lag,
        })

    elif tool == "prior_art":
        # Has this exact shape been in front of an analyst before? The
        # precedents came through the SAME declared context -- the trigger
        # could not have parameterized them on the signature, because the
        # signature is computed by the run, not known at firing time.
        sig = state["signature"]
        facts = context_facts(state)
        reasons = [f["object"] for f in facts
                   if f.get("subject") == sig
                   and f.get("relation") == "mg:dismissed_benign"]
        by = [f["object"] for f in facts
              if f.get("subject") == sig and f.get("relation") == "mg:dismissed_by"]
        card = {k: v for k, v in state.items()
                if k not in ("context", "item", "trigger", "connector", "scope")}
        card.update({"has_precedent": bool(reasons),
                     "precedent": reasons[0] if reasons else "",
                     "precedent_by": by[0] if by else "",
                     "precedent_count": len(reasons),
                     "decide_with": "escalate | benign (+ because: ...)"})
        append(CASES, card)
        emit({"has_precedent": bool(reasons),
              "precedent": reasons[0] if reasons else "",
              "precedent_by": by[0] if by else "",
              "precedent_count": len(reasons)})

    elif tool == "file_alert":
        append(ALERTS, {
            "case_ref": state["case_ref"], "symbol": state["symbol"],
            "signature": state["signature"], "desk": state["desk"],
            "order_id": state["order_id"], "event_id": state["event_id"],
            "disposition": "escalated",
            "analyst": state.get("responder", "unsigned"),
            "because": state.get("because", ""),
        })
        emit({"alert_filed": 1})

    elif tool == "record_dismissal":
        append(DISMISSALS, {
            "case_ref": state["case_ref"], "symbol": state["symbol"],
            "signature": state["signature"],
            "disposition": "benign",
            "analyst": state.get("responder", "unsigned"),
            "because": state.get("because", ""),
            "on_precedent": bool(state.get("has_precedent")),
        })
        emit({"dismissal_recorded": 1})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the connector seam -----------------------------------------------------
# One process, both feeds: the trigger's `scope` says which one is being
# polled. An ABSENT cursor means "seed and fire nothing", so declaring a
# trigger never replays the feed.

def connector_main():
    req = json.load(sys.stdin)
    scope = req.get("scope") or ""
    folder = FEED_DIRS.get(scope)
    if folder is None:
        sys.stderr.write("no feed for scope %r\n" % scope)
        return 1
    root = os.path.join(FIXTURES, folder)
    names = sorted(n for n in os.listdir(root)
                   if n.endswith(".json") and n[:2] <= FEED_UPTO)
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(root, name), encoding="utf-8") as fh:
            payload = json.load(fh)
        items.append({"id": payload["event_id"], "payload": payload})
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


def member_fired(alias):
    """`alias = true` -- the gate's field names ARE the member aliases."""
    return {"kind": "comparison", "field": alias, "comparator": "eq",
            "value": {"kind": "boolean", "value": True}}


def both_fired(left, right):
    """`order_burst = true AND material_event = true`, as a Condition TREE.

    Not a string. `areev-run-core`'s condition grammar is frozen and new CAL
    syntax is an OMS conformance decision, so a gate is a data structure --
    which is also why it can be authored from Python with no parser.
    """
    return {"kind": "and", "left": member_fired(left), "right": member_fired(right)}


def seed():
    db = open_db()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    normalize = tool_def("normalize_signal", "put one feed event on the tape")
    assemble = tool_def("assemble_case", "rebuild the correlated pair as a case")
    prior = tool_def("prior_art", "attach how this shape was dispositioned before")
    review = tool_def("analyst_review", "a surveillance analyst decides: "
                      "escalate, or dismiss as benign with a reason",
                      executor_kind="client")
    alert = tool_def("file_alert", "refer the case to the market-abuse team")
    dismissal = tool_def("record_dismissal", "record a signed benign dismissal")

    # Plan one: what a single feed event is worth on its own. Nothing but
    # normalization -- the judgment is in the gate, not here.
    intake = db.add("workflow", json.dumps({
        "name": "signal-intake",
        "nodes": ["normalize_signal"],
        "edges": [],
        "bindings": {"normalize_signal": normalize},
        "created_at": EPOCH_MS,
    }), ns=NS)

    # Plan two: what a CORRELATED PAIR is worth. Every case parks. There is
    # deliberately no auto-close edge: a surveillance disposition is a
    # regulated judgment, and an agent that closes its own alerts is the
    # thing this example is arguing against.
    case = db.add("workflow", json.dumps({
        "name": "surveillance-case",
        "nodes": ["assemble_case", "prior_art", "analyst_review",
                  "file_alert", "record_dismissal"],
        "edges": [
            {"src": "assemble_case", "dst": "prior_art"},
            {"src": "prior_art", "dst": "analyst_review"},
            {"src": "analyst_review", "dst": "file_alert",
             "cond": 'disposition == "escalate"'},
            {"src": "analyst_review", "dst": "record_dismissal",
             "cond": 'disposition == "benign"'},
        ],
        "bindings": {"assemble_case": assemble, "prior_art": prior,
                     "analyst_review": review, "file_alert": alert,
                     "record_dismissal": dismissal},
        "retries": {"assemble_case": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "surveillance-judgment",
        "description": "how this desk reads a correlated pair",
        "instructions": "Two signals close together are a question, never an "
                        "answer. Ask what else explains the size: a published "
                        "schedule, a mandate, an index the desk tracks. A "
                        "precedent tells you which question was asked last "
                        "time -- it does not tell you the answer for this "
                        "desk, this instrument, this day.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The instrument book: what the desk knows before any signal arrives.
    for symbol, issuer, index in [
        ("MRDN:VNTG", "Vantry Grid Holdings", "Meridian 40"),
        ("MRDN:ORLN", "Orlune Biosystems", "Meridian 40"),
        ("MRDN:PDRA", "Pendra Offshore", "Meridian 40"),
    ]:
        db.add_fact(symbol, "mg:issuer", issuer, ns=BOOK, idempotent=True)
        db.add_fact(symbol, "mg:index_member", index, ns=BOOK, idempotent=True)

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE precedent_line AS '
           '"- {{subject}} {{relation}} {{object}}"')
    # What the EVALUATOR puts in front of a case run. `$symbol` binds from the
    # firing item -- and the firing item of a composite is the correlation
    # value, so the instrument is the only thing the query can be
    # parameterized on. Precedents come back unfiltered because the case's
    # signature is computed by the run, after this query has already run.
    db.cal('DEFINE QUERY "case_ctx"($symbol) '
           'DESCRIPTION "what an analyst should have in front of them" '
           'AS { ASSEMBLE "case_ctx" FROM '
           'book: (RECALL facts WHERE namespace = "org.surv.book" '
           'AND subject = $symbol LIMIT 10), '
           'precedents: (RECALL facts WHERE namespace = "org.surv.precedents" '
           'LIMIT 50), '
           'judgment: (RECALL skills LIMIT 2) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plans, gates, precedents" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plans: (RECALL workflows LIMIT 4), '
           'gates: (RECALL triggers LIMIT 6), '
           'precedents: (RECALL facts WHERE namespace = "org.surv.precedents" '
           'LIMIT 20), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    # -- the two feeds, as two standing rules ---------------------------
    orders = db.trigger_add(json.dumps({
        "kind": "polling", "connector": "mock", "scope": ORDER_SCOPE,
        "interval_secs": POLL_SECS, "workflow": intake,
        "dedup_key": ["/event_id"],
    }), "every block order the venue reports", NS)
    news = db.trigger_add(json.dumps({
        "kind": "polling", "connector": "mock", "scope": NEWS_SCOPE,
        "interval_secs": POLL_SECS, "workflow": intake,
        "dedup_key": ["/event_id"],
    }), "every disclosure the vendor flags as material", NS)

    # -- and the rule that only they together can satisfy ----------------
    # No `interval_secs`: a composite is not clocked. It is eligible on every
    # pass and gated by whether its members have arrived.
    gate = db.trigger_add(json.dumps({
        "kind": "composite",
        "workflow": case,
        "members": {ORDER_ALIAS: orders, NEWS_ALIAS: news},
        "predicate": both_fired(ORDER_ALIAS, NEWS_ALIAS),
        "correlate": "/symbol",
        "window_ms": WINDOW_MS,
        "context_query": "case_ctx($symbol = /correlation)",
    }), "a block order and a material event on the SAME instrument, "
        "inside one window, is a case for an analyst", NS)

    emit({"intake": intake, "case": case, "orders": orders, "news": news,
          "gate": gate, "window_ms": WINDOW_MS})
    return 0


def gate():
    """The gate declaration, read back out of the memory."""
    db = open_db()
    grains = json.loads(db.cal('RECALL triggers LIMIT 10 FORMAT json'))["grains"]
    comp = next(g for g in grains if g["fields"].get("kind") == "composite")
    f = comp["fields"]
    emit({"trigger": comp["hash"], "members": f.get("members"),
          "predicate": f.get("predicate"), "correlate": f.get("correlate"),
          "window_ms": f.get("window_ms"), "workflow": f.get("workflow"),
          "context_query": f.get("context_query")})
    return 0


def gate_check():
    """Three composites that must never reach the memory.

    A dead trigger's only symptom is nothing happening, so the declaration
    is refused when it is written rather than discovered as silence weeks
    later. None of these three leaves a grain behind: validation runs
    before the write.
    """
    db = open_db()
    grains = json.loads(db.cal('RECALL triggers LIMIT 10 FORMAT json'))["grains"]
    members = {g["fields"].get("scope"): g["hash"] for g in grains
               if g["fields"].get("kind") == "polling"}
    orders, news = members[ORDER_SCOPE], members[NEWS_SCOPE]
    plan = next(g["hash"] for g in json.loads(
        db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
        if g["fields"].get("name") == "surveillance-case")

    def refuse(label, fields, because):
        try:
            db.trigger_add(json.dumps(fields), because, NS)
        except ValueError as e:
            return str(e)
        return None

    out = {
        "one_member": refuse("one_member", {
            "kind": "composite", "workflow": plan,
            "members": {ORDER_ALIAS: orders},
            "predicate": member_fired(ORDER_ALIAS),
        }, "a gate over one signal is not a gate"),
        "no_predicate": refuse("no_predicate", {
            "kind": "composite", "workflow": plan,
            "members": {ORDER_ALIAS: orders, NEWS_ALIAS: news},
            "correlate": "/symbol", "window_ms": WINDOW_MS,
        }, "two members and nothing to say about them"),
        "unknown_alias": refuse("unknown_alias", {
            "kind": "composite", "workflow": plan,
            "members": {ORDER_ALIAS: orders, NEWS_ALIAS: news},
            "predicate": both_fired(ORDER_ALIAS, "chat_spike"),
            "correlate": "/symbol", "window_ms": WINDOW_MS,
        }, "a gate naming a member nobody declared"),
    }
    emit(out)
    return 0


def ingest():
    """One heartbeat tick: claim, poll both feeds, dedup, settle the gate.

    Members are evaluated first and the composite last, so a pair that
    completes in this pass opens its case in this pass rather than waiting
    a whole heartbeat.
    """
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))
    emit(report)
    return 0


def now_ms():
    return int(time.time() * 1000)


def await_due(argv):
    """Block until every declared trigger is due again, then return.

    This exists because `sleep <a number I guessed>` is a bet on how loaded
    the machine is. A member trigger's next firing is `interval_secs` after
    its last one, and a tick's own duration -- three or four subprocesses,
    an interpreter start each -- is a property of the box, not of the
    example. On a loaded runner the two can cross and a tick lands
    `skipped_not_due`, which then reads downstream as "the gate didn't
    fire" and blames the wrong thing.

    So: ask the evaluator. `trigger_status()` answers `due` from exactly the
    predicate `trigger_run` gates on, and the wait ends the millisecond it
    flips -- which is also the tightest possible spacing, so the pairs that
    must land INSIDE the correlation window get the best chance the machine
    can give them.
    """
    timeout = float(argv[0]) if argv else 60.0
    db = open_db()
    started = now_ms()
    deadline = started + int(timeout * 1000)
    while True:
        rows = json.loads(db.trigger_status())
        blocked = [r for r in rows if r.get("unusable")]
        if blocked:
            sys.stderr.write("unusable triggers: %s\n"
                             % [(r["trigger"][:12], r["unusable"]) for r in blocked])
            return 6
        waiting = [r["trigger"][:12] for r in rows
                   if r["enabled"] and not r["paused"] and not r["due"]]
        if not waiting:
            emit({"due": len(rows), "waited_ms": now_ms() - started})
            return 0
        if now_ms() > deadline:
            sys.stderr.write("still not due after %ss: %s\n" % (timeout, waiting))
            return 6
        time.sleep(0.05)


def await_window(argv):
    """Block until the live half-match is certainly PAST the window.

    Anchored on `last_fired_at` -- the evaluator's own record of when the
    pass that armed the partial ran -- rather than on when this script
    thinks that was. A partial's expiry is measured from the same clock, so
    waiting `window_ms + margin` past that instant makes the near-miss
    assertion structural: under load the wait gets longer, never shorter.
    """
    margin = int(argv[0]) if argv else 2000
    db = open_db()
    fired = [r["last_fired_at"] for r in json.loads(db.trigger_status())
             if r.get("last_fired_at")]
    if not fired:
        sys.stderr.write("nothing has fired yet; there is no window to wait out\n")
        return 6
    anchor = max(fired)
    target = anchor + WINDOW_MS + margin
    while now_ms() < target:
        time.sleep(0.05)
    emit({"window_ms": WINDOW_MS, "since_firing_ms": now_ms() - anchor})
    return 0


def firings():
    """What each trigger did, from the evaluator's own journal."""
    db = open_db()
    obs = json.loads(db.cal('RECALL observations WHERE namespace = "agent:triggers" '
                            'RECENT 200 FORMAT json'))
    tally = {}
    for g in obs.get("grains") or []:
        f = g.get("fields") or {}
        kind = f.get("kind")
        if not kind:
            continue
        row = tally.setdefault(kind, {"evaluations": 0, "items": 0, "runs_started": 0})
        row["evaluations"] += 1
        row["items"] += int(f.get("items") or 0)
        row["runs_started"] += int(f.get("runs_started") or 0)
    emit(tally)
    return 0


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


def cases():
    db = open_db()
    rows = []
    for run_id, ask_id, state in pending_asks(db):
        rows.append({"run_id": run_id, "ask": ask_id,
                     "case_ref": state.get("case_ref"),
                     "issuer": state.get("issuer"),
                     "signature": state.get("signature"),
                     "desk": state.get("desk"),
                     "order_lag_seconds": state.get("order_lag_seconds"),
                     "has_precedent": state.get("has_precedent"),
                     "precedent": state.get("precedent"),
                     "precedent_by": state.get("precedent_by")})
    emit(rows)
    return 0


def decide(path):
    """An analyst's disposition, read from a fixture the way a case-manager
    webhook would deliver it."""
    with open(path, encoding="utf-8") as fh:
        note = json.load(fh)
    principal = note.get("analyst", "user:unknown")
    ref = note.get("case_ref")
    verdict = note.get("disposition")
    because = (note.get("because") or "").strip()
    if verdict not in ("escalate", "benign"):
        sys.stderr.write("a disposition is escalate or benign, not %r\n" % verdict)
        return 3
    if not because:
        # The reason is not paperwork. A benign dismissal with no reason
        # teaches the desk nothing and tells an examiner nothing.
        sys.stderr.write("a surveillance disposition needs a written reason\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        if state.get("case_ref") != ref:
            continue
        result = {"disposition": verdict, "responder": principal,
                  "because": because}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        # The DRIVER writes the grains, after the run returns -- a tool
        # process must never open the memory the runtime is holding.
        sig = state.get("signature") or "unknown"
        if verdict == "benign":
            # The lesson worth keeping is about the SHAPE, not the
            # instrument: next time this pattern correlates on any
            # instrument, the case arrives with this reasoning attached.
            db.add_fact(sig, "mg:dismissed_benign", because,
                        ns=PRECEDENTS, idempotent=True)
            db.add_fact(sig, "mg:dismissed_by", principal,
                        ns=PRECEDENTS, idempotent=True)
        db.record_tool_call(
            "analyst_review",
            ("benign_dismissal:%s" if verdict == "benign" else "escalation:%s") % sig,
            is_error=(verdict == "benign"), run_id=run_id)
        emit({"run_id": run_id, "case_ref": ref, "disposition": verdict,
              "analyst": principal, "signature": sig, "outcome": outcome})
        return 0
    sys.stderr.write("no parked case for %s\n" % ref)
    return 5


def improve():
    db = open_db()
    # Tuned to this desk's volume -- a recorded act of configuration, not a
    # fork. The defaults (3 in a cluster, 40% of the tool's opportunities)
    # assume a tool called hundreds of times; a surveillance analyst judges
    # a handful of cases a week, and a quarter of them being one shape is
    # already worth somebody's attention.
    db.set_analyzer_config("loop.tool_failure/1", True,
                           json.dumps({"min_count": 2, "min_rate": 0.25}))
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


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.surv.precedents" LIMIT 20 '
                 'FORMAT TEMPLATE precedent_line'))
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
    if cmd == "gate":
        return gate()
    if cmd == "gate-check":
        return gate_check()
    if cmd == "ingest":
        return ingest()
    if cmd == "await-due":
        return await_due(sys.argv[2:])
    if cmd == "await-window":
        return await_window(sys.argv[2:])
    if cmd == "firings":
        return firings()
    if cmd == "trigger-state":
        return trigger_state()
    if cmd == "cases":
        return cases()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(sys.argv[2:])
    if cmd == "brief":
        return brief()
    if cmd == "runs":
        return runs()
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
