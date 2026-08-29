#!/usr/bin/env python3
"""incident response: the whole on-call desk, one file, embedded Areev.

The difference from the other agent examples is HOW IT WAKES UP. They poll:
a connector is asked, every tick, whether anything new arrived. A monitoring
system does not wait to be asked -- it POSTs. So this desk has no connector
at all. The host owns the listener (Areev never opens a port), authenticates
the sender, and hands the payload over:

    trigger_deliver(<webhook trigger>, payload_json, tool_cmd=...)

Everything after that hand-off is plan nodes. The delivery starts a governed
run; the same payload delivered twice starts nothing (`duplicates: 1`), which
matters because every alerting vendor retries; and a second, `manual` trigger
on the same plan lets an on-call engineer replay an incident by hand.

The desk never touches production on its own. Every remediation parks on a
client gate first, and the engineer who answers it is the audit record.

One subcommand is a subprocess seam the runtime spawns (JSON on stdin, JSON
on stdout, one process per invocation). It never opens the memory -- the
party that spawned it is holding it:

    agent.py tools        the host tools   ($AREEV_TOOL_NAME picks one)

Everything else is the driver -- and `listen` is the part you replace with a
real HTTP endpoint:

    agent.py seed         author the plan, the tool definitions, the service
                          catalog, the saved CAL queries, and BOTH triggers
    agent.py listen       the host's webhook receiver, replaying its inbox:
                          every alert fixture up to $ALERT_UPTO, delivered
    agent.py deliver FILE deliver one payload through the webhook trigger
    agent.py replay ID    deliver one past alert through the MANUAL trigger
    agent.py pages        the parked runs waiting on an on-call engineer
    agent.py decide FILE  apply an engineer's decision to its parked run
    agent.py pause  --because "..."     maintenance window: refuse deliveries
    agent.py resume --because "..."
    agent.py triggers     what is declared, and what has actually fired
    agent.py improve      the loop reads the desk's own run journals back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the acts assert on this)
    agent.py teach NS SUBJECT RELATION OBJECT

To make it real, replace `listen` with your HTTP endpoint and `tools` with
processes that call your platform. The plan, the journal, the approval gate,
the dedup key and the audit trail do not change.
"""

import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("ALERT_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
INBOX = os.path.join(FIXTURES, "alerts")
# The clock the acts advance. `listen` replays the host's inbox up to here,
# which is how week one and week two are two runs of the same command.
ALERT_UPTO = os.environ.get("ALERT_UPTO", "03")
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
ACTIONS = os.path.join(OUT, "actions.jsonl")      # what actually touched prod
INCIDENTS = os.path.join(OUT, "incidents.jsonl")  # what was closed

NS = "org.ops"                 # plan, tool definitions, triggers, journals
SRE = "org.sre"                # the desk's own rules
SERVICES = "org.sre.services"  # the catalog, and what incidents taught it
DESK = "agent:incident-desk"   # the agent -- it can never approve itself

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# Below this severity the desk records and goes back to sleep. It is a FACT
# in org.sre, not a constant here -- which is what lets the loop propose
# moving it without a redeploy.
DEFAULT_WAKE_SEVERITY = "warning"
SEVERITY_RANK = {"info": 0, "warning": 1, "critical": 2}

# The written runbook: signal -> the step a human would reach for first.
# Deliberately dumb. Memory is what makes it smarter, and the loop is what
# notices when a step has stopped being executable.
RUNBOOK = {
    "http_5xx_rate": "scale",
    "replication_lag": "rollback",
    "queue_depth": "scale",
    "cert_expiry": "none",
    "disk_usage": "none",
}


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def alert_files(upto=None):
    upto = upto or ALERT_UPTO
    return sorted(n for n in os.listdir(INBOX)
                  if n.endswith(".json") and n[:2] <= upto)


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state: the alert under "item", the trigger's
# declared context under "context", the trigger's scope under "scope", plus
# every earlier node's result merged in at the top level. Never opens the
# memory -- the runtime that spawned this process is holding it.

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


def facts_about(grains, subject, relation):
    return [g["object"] for g in grains
            if g.get("subject") == subject and g.get("relation") == relation]


def tool_main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    grains = walk_grains(state.get("context") or {}, [])
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "classify":
        # Which door did this arrive through? The trigger's scope rides into
        # the run, so a hand-replayed incident is distinguishable from a live
        # page -- without which two runs on one alert are indistinguishable.
        scope = state.get("scope") or ""
        emit({
            "alert_id": item.get("alert_id"),
            "service": item.get("service"),
            "signal": item.get("signal"),
            "severity": item.get("severity"),
            "incident_key": "%s#%s" % (item.get("service"), item.get("signal")),
            "channel": "replay" if scope.startswith("oncall:") else "webhook",
        })

    elif tool == "recall_runbook":
        # The memory leg. Everything here came in through the trigger's
        # declared context_query -- the evaluator assembled it, because on the
        # embedded backend a tool inside a run cannot open the file its own
        # run holds.
        key = state.get("incident_key")
        service = state.get("service")
        causes = facts_about(grains, key, "mg:incident_cause")
        fixes = facts_about(grains, key, "mg:known_fix")
        emit({
            "known_cause": causes[-1] if causes else "",
            "known_fix": fixes[-1] if fixes else "",
            "prior_incidents": len(causes),
            "runbook_step": RUNBOOK.get(state.get("signal"), "none"),
            "tier": (facts_about(grains, service, "tier") or ["unknown"])[0],
            "owner": (facts_about(grains, service, "owner") or ["unassigned"])[0],
            "deploy_channel": (facts_about(grains, service, "deploy_channel")
                               or ["open"])[0],
        })

    elif tool == "propose_action":
        floor = (facts_about(grains, "oncall", "wake_severity")
                 or [DEFAULT_WAKE_SEVERITY])[0]
        loud = SEVERITY_RANK.get(state.get("severity"), 0) >= \
            SEVERITY_RANK.get(floor, 1)
        known = state.get("known_fix") or ""
        step = state.get("runbook_step") or "none"
        if not loud:
            action, confidence, why = "none", "below_floor", (
                "%s is below the desk's wake floor (%s) -- recorded, nobody paged"
                % (state.get("severity"), floor))
        elif known:
            action, confidence, why = known, "known", (
                "seen %d time(s) on this service and signal; last cause: %s"
                % (state.get("prior_incidents") or 1, state.get("known_cause")))
        else:
            action, confidence, why = step, ("runbook" if step != "none" else "none"), (
                "no incident on file for %s -- falling back to the written runbook"
                % state.get("incident_key"))
        emit({"proposed_action": action, "confidence": confidence,
              "rationale": why, "target": state.get("service")})

    elif tool == "apply_remediation":
        # The only node that touches production, and it runs ONLY after a
        # named human answered the gate. Both checks below are the tool's own
        # belt-and-braces: the runtime already refuses a self-answered ask.
        who = state.get("responder") or ""
        if not who or who == DESK:
            sys.stderr.write(
                "apply_remediation: no named human on this remediation (%r)\n" % who)
            return 1
        action = state.get("action") or state.get("proposed_action")
        service = state.get("service")
        if action == "rollback" and state.get("deploy_channel") == "pinned":
            # The runbook step is unexecutable against the platform as it is
            # configured today. Fail loudly: a remediation that silently does
            # nothing is worse than one that stops and says so.
            sys.stderr.write(
                "apply_remediation: rollback refused -- %s deploy channel is "
                "pinned (release freeze); the runbook step cannot execute\n" % service)
            return 1
        append(ACTIONS, {
            "alert_id": state.get("alert_id"),
            "incident_key": state.get("incident_key"),
            "service": service,
            "action": action,
            "proposed_action": state.get("proposed_action"),
            "confidence": state.get("confidence"),
            "applied_by": who,
            "because": state.get("because", ""),
        })
        emit({"applied": action, "applied_by": who, "target": service})

    elif tool == "record_only":
        # No production action. Either nobody was paged (below the floor) or
        # a human looked and said don't.
        emit({"applied": "none",
              "recorded_by": state.get("responder", "auto"),
              "recorded_because": state.get(
                  "because", state.get("rationale", ""))})

    elif tool == "close":
        append(INCIDENTS, {
            "alert_id": state.get("alert_id"),
            "incident_key": state.get("incident_key"),
            "service": state.get("service"),
            "signal": state.get("signal"),
            "severity": state.get("severity"),
            "channel": state.get("channel"),
            "proposed_action": state.get("proposed_action"),
            "confidence": state.get("confidence"),
            "applied": state.get("applied"),
            "by": state.get("applied_by") or state.get("recorded_by") or "auto",
        })
        emit({"closed": state.get("alert_id")})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
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

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    classify = tool_def("classify", "normalize the alert and name the incident")
    recall = tool_def("recall_runbook",
                      "what this desk already knows about this service and signal")
    propose = tool_def("propose_action", "propose a remediation, or none")
    review = tool_def("oncall_review",
                      "an on-call engineer decides: apply the remediation, or "
                      "record it and touch nothing", executor_kind="client")
    apply_rem = tool_def("apply_remediation",
                         "execute the approved remediation against the platform")
    record = tool_def("record_only", "record the incident without touching production")
    close = tool_def("close", "write the incident record and close it")

    # T18: `close` is the only terminal -- a node with NO out-edges. A plan
    # whose every node has one never completes, it stalls.
    wf = db.add("workflow", json.dumps({
        "name": "incident-response",
        "nodes": ["classify", "recall_runbook", "propose_action", "oncall_review",
                  "apply_remediation", "record_only", "close"],
        "edges": [
            {"src": "classify", "dst": "recall_runbook"},
            {"src": "recall_runbook", "dst": "propose_action"},
            # The gate sits on the production-action path ONLY. An alert with
            # nothing to do in production is recorded without waking anyone.
            {"src": "propose_action", "dst": "record_only",
             "cond": 'proposed_action == "none"'},
            {"src": "propose_action", "dst": "oncall_review",
             "cond": 'proposed_action != "none"'},
            {"src": "oncall_review", "dst": "apply_remediation",
             "cond": 'decision == "apply"'},
            {"src": "oncall_review", "dst": "record_only",
             "cond": 'decision == "record_only"'},
            {"src": "apply_remediation", "dst": "close"},
            {"src": "record_only", "dst": "close"},
        ],
        "bindings": {"classify": classify, "recall_runbook": recall,
                     "propose_action": propose, "oncall_review": review,
                     "apply_remediation": apply_rem, "record_only": record,
                     "close": close},
        "retries": {"classify": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "oncall-judgment",
        "description": "how this desk reads a page",
        "instructions": "Never touch production without a named human on the "
                        "decision. A remediation that cannot execute must fail "
                        "loudly, not quietly succeed. An alert below the wake "
                        "floor is recorded, not escalated. After an incident is "
                        "resolved, write down the cause -- the next identical "
                        "page should arrive with its history attached.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The desk's own rules.
    db.add_fact("oncall", "wake_severity", DEFAULT_WAKE_SEVERITY, ns=SRE,
                idempotent=True)
    db.add_fact("oncall", "monitor", "beacon", ns=SRE, idempotent=True)

    # The service catalog. `deploy_channel` is the fact the remediation tool
    # checks before it runs a rollback -- platform state the written runbook
    # does not know about.
    with open(os.path.join(FIXTURES, "services.json")) as fh:
        catalog = json.load(fh)
    for svc in catalog["services"]:
        for key in ("tier", "owner", "deploy_channel", "runbook"):
            db.add_fact(svc["name"], key, svc[key], ns=SERVICES, idempotent=True)

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE incident_line AS '
           '"- {{subject}} {{relation}} {{object}}"')
    db.cal('DEFINE QUERY "incident_ctx"($service) '
           'DESCRIPTION "what the desk knows before it proposes anything" '
           'AS { ASSEMBLE "incident_ctx" FROM '
           'judgment: (RECALL skills LIMIT 2), '
           'desk: (RECALL facts WHERE namespace = "org.sre.*" LIMIT 200) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, what it learned" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'lessons: (RECALL facts WHERE namespace = "org.sre.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    # TWO standing rules, ONE plan. Neither needs a connector: a push source
    # is not polled, so there is nothing to poll it with.
    hook = db.trigger_add(json.dumps({
        "kind": "webhook",
        "scope": "beacon:alerts",
        "workflow": wf,
        "dedup_key": ["/alert_id"],
        "context_query": "incident_ctx($service = /service)",
    }), "beacon posts every alert here; one run per alert", NS)

    manual = db.trigger_add(json.dumps({
        "kind": "manual",
        "scope": "oncall:replay",
        "workflow": wf,
        # id + occurrence: replaying the same alert deliberately twice is a
        # different occurrence, so the operator bumps the sequence. Same
        # `id + updated_at` idiom the polling triggers use, made explicit.
        "dedup_key": ["/alert_id", "/replay_seq"],
        "context_query": "incident_ctx($service = /service)",
    }), "let an on-call engineer replay a past alert by hand", NS)

    emit({"workflow": wf, "webhook": hook, "manual": manual})
    return 0


def triggers_by_kind(db):
    return {t["kind"]: t["trigger"] for t in json.loads(db.trigger_list())}


def deliver_payload(db, trigger, payload):
    """One webhook hand-off. The host owns the listener; this is the line
    where the payload becomes Areev's problem."""
    return json.loads(db.trigger_deliver(
        trigger, json.dumps(payload),
        tool_cmd=self_cmd("tools"),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))


def totals(reports):
    out = {"delivered": len(reports), "runs_started": 0, "duplicates": 0,
           "unidentifiable": 0, "errors": []}
    for r in reports:
        for k in ("runs_started", "duplicates", "unidentifiable"):
            out[k] += r.get(k, 0)
        out["errors"].extend(r.get("errors") or [])
    return out


def listen(argv):
    """The host's webhook receiver, replaying its inbox.

    In production this is an HTTP handler: terminate TLS, check the sender's
    signature, then call `trigger_deliver` with the body. Areev never opens a
    port -- the host is far better at both jobs, and a memory engine with a
    listener is a memory engine with an attack surface.
    """
    upto = argv[1] if len(argv) > 1 and argv[0] == "--upto" else None
    db = open_db()
    hook = triggers_by_kind(db)["webhook"]
    reports = []
    for name in alert_files(upto):
        with open(os.path.join(INBOX, name)) as fh:
            reports.append(deliver_payload(db, hook, json.load(fh)))
        # One request at a time, milliseconds apart: a listener is not a
        # batch job, and each firing's journal entry is a distinct moment in
        # the audit record rather than a pile at one timestamp.
        time.sleep(0.002)
    emit(totals(reports))
    return 0


def deliver(path):
    db = open_db()
    hook = triggers_by_kind(db)["webhook"]
    with open(path) as fh:
        payload = json.load(fh)
    try:
        report = deliver_payload(db, hook, payload)
    except ValueError as e:
        # A refused delivery is the host's problem to retry -- it is never
        # swallowed. (A paused desk, a disabled rule, an unknown trigger.)
        emit({"refused": True, "error": str(e)})
        return 4
    emit(report)
    return 0


def replay(argv):
    """An on-call engineer replays a past alert by hand, through the MANUAL
    trigger. Same plan, same gate, same journal -- a different door."""
    if not argv:
        sys.stderr.write("usage: replay ALERT_ID [SEQ]\n")
        return 2
    alert_id, seq = argv[0], (argv[1] if len(argv) > 1 else "1")
    payload = None
    for name in alert_files("99"):
        with open(os.path.join(INBOX, name)) as fh:
            candidate = json.load(fh)
        if candidate.get("alert_id") == alert_id:
            payload = candidate
            break
    if payload is None:
        sys.stderr.write("no alert %s in the inbox\n" % alert_id)
        return 5
    payload = dict(payload, replay_seq=seq, replayed_by="oncall")
    db = open_db()
    manual = triggers_by_kind(db)["manual"]
    emit(deliver_payload(db, manual, payload))
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


def page_row(run_id, ask_id, state):
    return {"run_id": run_id, "ask": ask_id,
            "alert_id": state.get("alert_id"),
            "channel": state.get("channel"),
            "service": state.get("service"),
            "signal": state.get("signal"),
            "severity": state.get("severity"),
            "proposed_action": state.get("proposed_action"),
            "confidence": state.get("confidence"),
            "known_cause": state.get("known_cause", ""),
            "prior_incidents": state.get("prior_incidents", 0),
            "rationale": state.get("rationale")}


def pages():
    db = open_db()
    emit([page_row(r, a, s) for r, a, s in pending_asks(db)])
    return 0


def decide(path):
    """An engineer's decision, read from a fixture the way an incident tool's
    webhook would deliver it. It carries a verdict, a reason, and -- once the
    incident is understood -- the cause, which is the part that becomes
    memory."""
    with open(path) as fh:
        note = json.load(fh)
    principal = note.get("engineer", "user:unknown")
    verdict = note.get("decision")
    because = note.get("because", "")
    if verdict not in ("apply", "record_only") or not because:
        sys.stderr.write("an on-call decision needs a verdict and a reason\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        if state.get("alert_id") != note.get("alert_id"):
            continue
        if state.get("channel") != note.get("channel", "webhook"):
            continue
        result = {"decision": verdict, "responder": principal, "because": because}
        # The engineer may override the proposal -- which is exactly what
        # happens the first time a runbook step is wrong.
        if note.get("action"):
            result["action"] = note["action"]
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))

        # Resolution is what makes a cause worth keeping. A remediation that
        # failed taught us nothing about the incident yet, so nothing is
        # written -- the desk does not learn from an unfinished night.
        learned = []
        if outcome.get("finished") == "Completed" and note.get("cause"):
            key = state.get("incident_key")
            learned.append(db.add_fact(key, "mg:incident_cause", note["cause"],
                                       ns=SERVICES, idempotent=True))
            if note.get("fix"):
                learned.append(db.add_fact(key, "mg:known_fix", note["fix"],
                                           ns=SERVICES, idempotent=True))
        emit({"run_id": run_id, "alert_id": note.get("alert_id"),
              "decision": verdict, "responder": principal,
              "outcome": outcome, "learned": learned})
        return 0
    sys.stderr.write("no parked run for alert %s on the %s channel\n"
                     % (note.get("alert_id"), note.get("channel", "webhook")))
    return 5


def set_paused(argv, paused):
    because = None
    it = iter(argv)
    for flag in it:
        if flag == "--because":
            because = next(it, None)
    if not because:
        sys.stderr.write("pausing or resuming a standing rule needs a reason\n")
        return 2
    db = open_db()
    hook = triggers_by_kind(db)["webhook"]
    print(db.trigger_pause(hook, because) if paused
          else db.trigger_resume(hook, because))
    return 0


def trigger_state():
    """What is declared, and what this host's evaluation state says about it.

    Note both rows: two standing rules, one plan, and neither names a
    connector -- a push source is not polled, so there is nothing to poll it
    with.
    """
    db = open_db()
    status = {s["trigger"]: s for s in json.loads(db.trigger_status())}
    rows = []
    for t in json.loads(db.trigger_list()):
        st = status.get(t["trigger"], {})
        rows.append(dict(t, paused=st.get("paused"),
                         consecutive_failures=st.get("consecutive_failures"),
                         last_error=st.get("last_error")))
    emit(rows)
    return 0


def improve():
    db = open_db()
    # Tune the analyzers to this desk's volume -- a recorded act of
    # configuration, not a fork.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_failure_ratio": 0.3}))
    report = json.loads(db.loop_run(llm_cmd=os.environ.get("LOOP_LLM_CMD")))
    recs = json.loads(db.recommendations('{"status": "pending"}'))
    emit({"stored": report.get("stored", 0),
          "loop": report,
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
    print(db.cal('RECALL facts WHERE namespace = "org.sre.*" LIMIT 40 '
                 'FORMAT TEMPLATE incident_line'))
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal(
        'RECALL observations WHERE namespace = "agent:harness" RECENT 200 FORMAT json'))
    outcome, detail = {}, {}
    for g in obs.get("grains") or []:
        fields = g.get("fields") or {}
        if fields.get("observation_kind") == "run_outcome":
            outcome[fields.get("run_id")] = fields.get("object")
            detail[fields.get("run_id")] = fields.get("outcome_detail", "")
    emit([{"run_id": r, "outcome": outcome.get(r, "open"),
           "detail": detail.get(r, "")}
          for r in json.loads(db.run_list(100))])
    return 0


def teach(argv):
    if len(argv) != 4:
        sys.stderr.write("usage: teach NS SUBJECT RELATION OBJECT\n")
        return 2
    ns, subject, relation, obj = argv
    db = open_db()
    print(db.add_fact(subject, relation, obj, ns=ns, idempotent=True))
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "tools":
        return tool_main()
    if cmd == "seed":
        return seed()
    if cmd == "listen":
        return listen(sys.argv[2:])
    if cmd == "deliver":
        return deliver(sys.argv[2])
    if cmd == "replay":
        return replay(sys.argv[2:])
    if cmd == "pages":
        return pages()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "pause":
        return set_paused(sys.argv[2:], True)
    if cmd == "resume":
        return set_paused(sys.argv[2:], False)
    if cmd == "triggers":
        return trigger_state()
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(sys.argv[2:])
    if cmd == "brief":
        return brief()
    if cmd == "runs":
        return runs()
    if cmd == "teach":
        return teach(sys.argv[2:])
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
