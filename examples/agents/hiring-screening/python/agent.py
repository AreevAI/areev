#!/usr/bin/env python3
"""hiring screening: the whole agent, one file, embedded Areev.

Candidate screening for one job requisition. Under the EU AI Act this is an
Annex III **high-risk** use (employment, worker management), so the design
question is not "how good is the model" -- it is *can you show a regulator,
from the record, that a person was actually in the loop*.

That is what this example exists for. The plan has NO auto-advance path:
`recruiter_review` is a client-gated node on the only route between the
criteria check and either outcome, so every single candidate parks for a
named recruiter. And the evidence of that is not a paragraph in a policy
PDF -- it is `areev run oversight-report`, MEASURED from the run journal:

    agent.py oversight            the Article 14 report for the newest run
    agent.py oversight --plan     ... for the newest run of the plan

    {"human_gates": {"client_gated_nodes": [{"node": "recruiter_review", ...}],
                     "separation_of_duties": "responder != triggering
                                              principal, refused structurally",
                     "ask_ttl_sec": 172800},
     "authorized_responders": {"principals_granted_run_respond":
                               ["user:ines", "user:mo"]},
     "budgets": {"max_usd_micros": 1500000, "max_wall_ms": 120000, ...},
     "kill_switch": {"verb": "run.cancel (deliberately the lowest-privilege
                              run verb)",
                     "measured_cancel_to_drain_ms": [4]}}

What this agent does NOT do: it does not score candidates, does not rank
them, and does not test anyone for bias. It checks an application against
the criteria the requisition published, says which ones are met, missed, or
simply not evidenced, and hands all of that to a person. Areev's
contribution is oversight and record-keeping, not fairness auditing.

Two subcommands are subprocess seams the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). They never open the memory --
the party that spawned them is holding it:

    agent.py tools        the host tools      ($AREEV_TOOL_NAME picks one)
    agent.py connector    the application queue (fixtures in, items+cursor out)

Everything else is the driver:

    agent.py seed         author the plan, the tool definitions, the
                          requisition's criteria, the reviewer GRANTS, the
                          saved CAL queries, the intake trigger
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick),
                          under this desk's four budget ceilings
    agent.py asks         the parked runs waiting on a named recruiter
    agent.py decide FILE  apply a recruiter's decision to its parked run
    agent.py stop MARKER --because "..."     the kill switch (run.cancel)
    agent.py verify [RUN] journal-consistent replay -- the record has not
                          been edited after the fact
    agent.py oversight [--plan|RUN]          the Article 14 report
    agent.py improve      the loop reads the desk's own history back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py plan         the stored plan: nodes, edges, which node parks
    agent.py gate-audit   outcomes vs. humans, counted from the journal
    agent.py precedents   what recruiters have recorded on this requisition
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the acts assert on this)

To make it real, replace `tools` and `connector` with processes that call
your ATS. The plan, the journal, the gate, the grants and the oversight
report do not change.
"""

import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("APP_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
QUEUE = os.path.join(FIXTURES, "applications")
APPS_UPTO = os.environ.get("APPS_UPTO", "05")   # the acts advance this "clock"
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
DECISIONS = os.path.join(OUT, "decisions.jsonl")
CLOSED = os.path.join(OUT, "closed.jsonl")
PARSE_LOG = os.path.join(OUT, "parse.jsonl")
PARSE_CURSOR = os.path.join(OUT, "parse.cursor")

NS = "org.talent"                # plan, tool definitions, triggers, journals
REQS = "org.talent.reqs"         # the requisition's criteria + what reviewers recorded
DESK = "agent:screening-desk"    # the agent -- it can never approve
COORDINATOR = "user:coordinator" # can stop a run; is NOT a named reviewer
QUEUE_SCOPE = "queue:applications"

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# The four budget ceilings this desk runs under. They are journaled into
# every run's manifest, which is how they reach the oversight report --
# a governance control that is reported from the record, not asserted.
#
# max_wall_ms is COMPUTE time, not calendar time: a run parked on a
# recruiter for two days accrues `elapsed`, never `wall`. A 48-hour ask TTL
# and a two-minute wall ceiling are not in tension.
MAX_TOKENS = 200_000
MAX_USD_MICROS = 1_500_000        # $1.50 per screening run
MAX_WALL_MS = 120_000             # two minutes of actual compute
ASK_TTL_SEC = 172_800             # a recruiter has 48 hours to answer

# The criteria this desk knows how to check. Every one is job-related and
# published on the requisition; the desk has no other axis, by construction.
CRITERIA_ORDER = ["min_years_backend", "required_certification",
                  "work_authorisation"]

PARSE_REFUSAL = "unreadable: the uploaded file has no text layer"


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def marker(application_id):
    return hashlib.sha256(application_id.encode()).hexdigest()[:12]


def requisition():
    with open(os.path.join(FIXTURES, "requisition.json")) as fh:
        return json.load(fh)


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


def check(state, grains):
    """Compare one parsed application against the requisition's OWN criteria.

    The criteria are not constants in this file: they are Facts in
    `org.talent.reqs`, delivered through the trigger's declared context. A
    requisition that publishes different criteria screens differently with
    no code change -- and the criteria a decision was made under stay
    queryable afterwards.

    Emits no score and no ranking, and reaches no decision: the plan has no
    edge from this node to either outcome.
    """
    req = state.get("requisition") or "?"
    stated, precedents = {}, {}
    for g in grains:
        subject, relation = g.get("subject"), g.get("relation")
        if subject == req and relation in CRITERIA_ORDER:
            stated[relation] = g.get("object")
        if relation == "mg:review_precedent" and (subject or "").startswith(req + "/"):
            prior = precedents.get(subject)
            if prior is None or (g.get("created_at") or 0) >= prior[0]:
                precedents[subject] = (g.get("created_at") or 0, g.get("object"))

    years = state.get("years_backend")
    certs = state.get("certifications")
    auth = state.get("work_authorisation")
    met, missed, not_evidenced = [], [], []

    def verdict(name, evidenced, ok):
        if not evidenced:
            not_evidenced.append(name)
        elif ok:
            met.append(name)
        else:
            missed.append(name)

    for name in CRITERIA_ORDER:
        want = stated.get(name)
        if want is None:
            continue           # the requisition does not state it; not screened on
        if name == "min_years_backend":
            verdict(name, years is not None, years is not None and int(years) >= int(want))
        elif name == "required_certification":
            verdict(name, certs is not None, bool(certs) and want in certs)
        elif name == "work_authorisation":
            verdict(name, bool(auth), auth == want)

    if missed:
        reason = "misses stated criteria: " + ", ".join(missed)
    elif not_evidenced:
        reason = "not evidenced on the application: " + ", ".join(not_evidenced)
    else:
        reason = "meets every stated criterion"

    # A recruiter's recorded reason for the SAME mismatch, if this desk has
    # seen one before. It is shown to the reviewer; it decides nothing.
    key = "%s/%s" % (req, "+".join(missed)) if missed else None
    return {
        "criteria_source": req,
        "criteria_stated": sorted(stated),
        "criteria_met": met,
        "criteria_missed": missed,
        "criteria_not_evidenced": not_evidenced,
        "review_reason": reason,
        "precedent_key": key,
        "precedent": precedents.get(key, (0, None))[1] if key else None,
    }


def tool_main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    grains = walk_grains(state.get("context") or {}, [])
    tool = os.environ.get("AREEV_TOOL_NAME", "")
    app_id = item.get("application_id", "?")

    if tool == "parse_application":
        # No text layer, no screening. A candidate is NEVER moved out of the
        # process because our parser could not read their file: the run
        # FAILS, loudly, and a person picks the application up by hand.
        if not item.get("cv_text"):
            append(PARSE_LOG, {"application_id": app_id, "is_error": True,
                               "result": PARSE_REFUSAL,
                               "source": item.get("source")})
            sys.stderr.write(
                "parse_application %s: %s (%s) -- refusing to screen an "
                "application we cannot read\n"
                % (app_id, PARSE_REFUSAL, item.get("attachment_kind")))
            return 1
        declared = item.get("declared") or {}
        append(PARSE_LOG, {"application_id": app_id, "is_error": False,
                           "result": "parsed", "source": item.get("source")})
        emit({
            "parsed": True,
            "application_id": app_id,
            "candidate": item.get("candidate"),
            "requisition": item.get("requisition"),
            "source": item.get("source"),
            "years_backend": declared.get("years_backend"),
            "certifications": declared.get("certifications"),
            "work_authorisation": declared.get("work_authorisation"),
        })

    elif tool == "check_criteria":
        emit(check(state, grains))

    elif tool == "advance":
        append(DECISIONS, {
            "application_id": app_id,
            "candidate": state.get("candidate"),
            "requisition": state.get("requisition"),
            "outcome": "advanced",
            "decided_by": state.get("responder", "auto"),
            "because": state.get("because"),
            "criteria_missed": state.get("criteria_missed"),
            "criteria_not_evidenced": state.get("criteria_not_evidenced"),
        })
        emit({"advanced": 1, "application_id": app_id})

    elif tool == "reject":
        append(DECISIONS, {
            "application_id": app_id,
            "candidate": state.get("candidate"),
            "requisition": state.get("requisition"),
            "outcome": "rejected",
            "decided_by": state.get("responder", "auto"),
            "because": state.get("because"),
            "criteria_missed": state.get("criteria_missed"),
            "criteria_not_evidenced": state.get("criteria_not_evidenced"),
        })
        emit({"rejected": 1, "application_id": app_id})

    elif tool == "close_case":
        append(CLOSED, {"application_id": app_id,
                        "outcome": "advanced" if state.get("advanced") else "rejected",
                        "decided_by": state.get("responder")})
        emit({"closed": True})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the connector seam -----------------------------------------------------
# An ABSENT cursor means "seed and fire nothing", so declaring a trigger
# never replays the queue. APPS_UPTO is the clock the acts advance.

def connector_main():
    req = json.load(sys.stdin)
    names = sorted(n for n in os.listdir(QUEUE)
                   if n.endswith(".json") and n[:2] <= APPS_UPTO)
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(QUEUE, name)) as fh:
            payload = json.load(fh)
        items.append({"id": payload["application_id"], "payload": payload})
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
    req = requisition()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    parse = tool_def("parse_application",
                     "read the application; refuse anything without a text layer")
    criteria = tool_def("check_criteria",
                        "compare the application against the criteria the "
                        "requisition published -- met, missed, or not evidenced")
    # THE GATE. `executor_kind: "client"` is what makes this node park the
    # run and hand a requires_action envelope to a person; it is also what
    # the oversight report counts as a human intervention point.
    review = tool_def("recruiter_review",
                      "a named recruiter decides: advance or reject, with a "
                      "written reason", executor_kind="client")
    advance = tool_def("advance", "move the candidate to the interview stage")
    reject = tool_def("reject", "close the application against this requisition")
    close = tool_def("close_case", "close the case and stop processing")

    # No edge runs from check_criteria to advance or reject. There is no
    # auto-advance path in this plan -- not a disabled one, an absent one.
    wf = db.add("workflow", json.dumps({
        "name": "hiring-screening",
        "nodes": ["parse_application", "check_criteria", "recruiter_review",
                  "advance", "reject", "close"],
        "edges": [
            {"src": "parse_application", "dst": "check_criteria"},
            {"src": "check_criteria", "dst": "recruiter_review"},
            {"src": "recruiter_review", "dst": "advance",
             "cond": 'decision == "advance"'},
            {"src": "recruiter_review", "dst": "reject",
             "cond": 'decision == "reject"'},
            {"src": "advance", "dst": "close"},
            {"src": "reject", "dst": "close"},
        ],
        "bindings": {"parse_application": parse, "check_criteria": criteria,
                     "recruiter_review": review, "advance": advance,
                     "reject": reject, "close": close},
        "retries": {"parse_application": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "screening-judgment",
        "description": "how this desk reads an application against a requisition",
        "instructions": "Check only what the requisition published, and check "
                        "it the same way for everybody. A criterion the "
                        "application does not mention is NOT EVIDENCED, which "
                        "is a question for the first call and never a "
                        "screen-out. An application we cannot read is a "
                        "handling failure, never a rejection. Every advance "
                        "and every reject is a named person's decision with a "
                        "written reason.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The requisition's criteria, as memory. Change these and the desk
    # screens differently with no code change -- and the criteria a decision
    # was made under stay queryable afterwards.
    for c in req["criteria"]:
        db.add_fact(req["requisition"], c["id"], c["value"], ns=REQS,
                    idempotent=True)
        db.add_fact("%s/%s" % (req["requisition"], c["id"]), "mg:stated_as",
                    c["stated"], ns=REQS, idempotent=True)
    db.add_fact(req["requisition"], "title", req["title"], ns=REQS,
                idempotent=True)

    # WHO MAY APPROVE. Grants are Facts in the file, so they replicate with
    # the memory and the oversight report reads them straight back out.
    for reviewer in req["reviewers"]:
        db.cal('GRANT run.respond ON "%s" TO "%s" WITH because('
               '"named reviewer on %s")' % (NS, reviewer, req["requisition"]))
    # Deliberately NOT granted run.respond: a coordinator can pull the
    # brake without being trusted to decide anybody's application.
    db.cal('GRANT run.cancel ON "%s" TO "%s" WITH because('
           '"the brake must never be blocked by missing privilege")'
           % (NS, COORDINATOR))

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE precedent_line AS '
           '"- {{subject}}: {{object}}"')
    db.cal('DEFINE QUERY "screen_ctx"($session) '
           'DESCRIPTION "what the criteria check is allowed to know" '
           'AS { ASSEMBLE "screen_ctx" FROM '
           'policy: (RECALL skills LIMIT 2), '
           'requisition: (RECALL facts WHERE namespace = "org.talent.*" LIMIT 200) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, gate, criteria, precedents" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'requisition: (RECALL facts WHERE namespace = "org.talent.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    trigger = db.trigger_add(json.dumps({
        "kind": "polling",
        "connector": "mock",
        "scope": QUEUE_SCOPE,
        "interval_secs": 1,
        "workflow": wf,
        "dedup_key": ["/application_id"],
        "context_query": "screen_ctx($session = /application_id)",
    }), "screen every application against %s" % req["requisition"], NS)

    emit({"workflow": wf, "trigger": trigger,
          "requisition": req["requisition"],
          "reviewers": req["reviewers"],
          "gate": "recruiter_review"})
    return 0


def drain_parse_log(db):
    """Move the parse seam's own audit lines into memory, AFTER the run.

    The tool process must never open the memory the runtime is holding, so
    it writes a line and the driver records it as a tool call once the tick
    has returned. Those grains are what `areev loop run` clusters later.
    """
    if not os.path.exists(PARSE_LOG):
        return 0
    seen = 0
    if os.path.exists(PARSE_CURSOR):
        with open(PARSE_CURSOR) as fh:
            seen = int(fh.read().strip() or 0)
    lines = open(PARSE_LOG, encoding="utf-8").read().splitlines()
    for line in lines[seen:]:
        row = json.loads(line)
        db.record_tool_call("parse_application", row["result"],
                            is_error=row["is_error"],
                            thread=row["application_id"])
    with open(PARSE_CURSOR, "w") as fh:
        fh.write(str(len(lines)))
    return len(lines) - seen


def ingest():
    """One heartbeat tick, under this desk's four declared ceilings.

    The budgets are not decoration: they are journaled into every run's
    manifest, and the oversight report reads them back from there. A
    governance control nobody can measure is a claim, not a control.
    """
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        max_tokens=MAX_TOKENS,
        max_usd_micros=MAX_USD_MICROS,
        max_wall_ms=MAX_WALL_MS,
        ask_ttl_sec=ASK_TTL_SEC,
    ))
    report["parse_calls_recorded"] = drain_parse_log(db)
    emit(report)
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


def find_ask(db, ref):
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        if marker(item.get("application_id", "?")) == ref:
            return run_id, ask_id, state
    return None, None, None


def asks():
    db = open_db()
    rows = []
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        rows.append({"run_id": run_id, "ask": ask_id,
                     "marker": marker(item.get("application_id", "?")),
                     "application_id": item.get("application_id"),
                     "candidate": state.get("candidate"),
                     "criteria_met": state.get("criteria_met"),
                     "criteria_missed": state.get("criteria_missed"),
                     "criteria_not_evidenced": state.get("criteria_not_evidenced"),
                     "review_reason": state.get("review_reason"),
                     "precedent": state.get("precedent")})
    emit(rows)
    return 0


def decide(path):
    """A recruiter's decision, read from a fixture the way an ATS webhook
    would deliver it.

    Two refusals live here and both are structural, not advisory:
    `run_respond` rejects the principal that STARTED the run (separation of
    duties), and a verdict the plan has no edge for never reaches the
    runtime at all.
    """
    with open(path) as fh:
        note = json.load(fh)
    principal = note.get("reviewer", "user:unknown")
    verdict = note.get("decision")
    because = note.get("because", "")
    if verdict not in ("advance", "reject"):
        sys.stderr.write("this plan has an edge for `advance` and `reject`; "
                         "%r is neither\n" % verdict)
        return 3
    if not because:
        sys.stderr.write("a screening decision needs a written reason\n")
        return 3

    db = open_db()
    run_id, ask_id, state = find_ask(db, note.get("marker"))
    if run_id is None:
        sys.stderr.write("no parked run matches marker %s\n" % note.get("marker"))
        return 5
    item = state.get("item", state)
    result = {"decision": verdict, "responder": principal, "because": because}
    try:
        db.run_respond(run_id, ask_id, json.dumps(result), principal)
    except ValueError as e:
        sys.stderr.write("respond refused: %s\n" % e)
        return 4

    # A recruiter's recorded reason for a criterion mismatch is the lesson
    # worth keeping: the NEXT application with the same mismatch shows the
    # reviewer what this desk decided last time -- and still parks for them.
    if verdict == "reject" and state.get("precedent_key"):
        db.add_fact(state["precedent_key"], "mg:review_precedent",
                    "%s rejected %s: %s" % (principal,
                                            item.get("application_id"), because),
                    ns=REQS, idempotent=True)

    outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
    emit({"run_id": run_id, "decision": verdict, "responder": principal,
          "application_id": item.get("application_id"), "outcome": outcome})
    return 0


def stop(argv):
    """The kill switch. `run.cancel` is deliberately the LOWEST-privilege run
    verb -- a brake must never be blocked by missing privilege -- so the
    coordinator here can stop a run without being trusted to decide one.

    The cancel writes a marker Fact; the resume drains the run at its next
    superstep boundary. Both timestamps are journaled, which is how the
    oversight report can MEASURE cancel-to-drain instead of promising it.
    """
    if not argv:
        sys.stderr.write("usage: stop MARKER --because '...'\n")
        return 2
    ref = argv[0]
    because = None
    it = iter(argv[1:])
    for flag in it:
        if flag == "--because":
            because = next(it, None)
    if not because:
        sys.stderr.write("a cancel needs a written reason\n")
        return 2
    db = open_db(actor=COORDINATOR)
    run_id, _, state = find_ask(db, ref)
    if run_id is None:
        sys.stderr.write("no parked run matches marker %s\n" % ref)
        return 5
    item = state.get("item", state)
    db.run_cancel(run_id, because)
    outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
    emit({"run_id": run_id, "canceled_by": COORDINATOR, "because": because,
          "application_id": item.get("application_id"), "outcome": outcome})
    return 0


def verify(argv):
    """Journal-consistent replay: re-derive every checkpoint from the
    journal and byte-compare against the stored chain, writing nothing.

    The compliance claim this backs is narrow and strong: the record has not
    been edited after the fact.
    """
    db = open_db()
    ids = argv or json.loads(db.run_list(100))
    rows = []
    for run_id in ids:
        report = json.loads(db.run_verify(run_id))
        rows.append({"run_id": run_id, "verified": report.get("verified"),
                     "steps": len(report.get("steps") or [])})
    emit({"runs": len(rows), "all_verified": all(r["verified"] for r in rows),
          "reports": rows})
    return 0


def oversight(argv):
    """THE ARTIFACT. EU AI Act Article 14, answered from the run journal.

    Where a person can intervene (the client-gated nodes), who is authorized
    to (the run.respond grants in the file), what expires when (the ask
    TTL), what the run was allowed to spend (the budgets), and how fast the
    kill switch actually drained -- measured, from the journaled cancel Fact
    to the terminal checkpoint's journaled close.

    Nothing here is configured for the report's benefit. Every field is read
    back out of the same record the runs wrote while doing the job.
    """
    db = open_db()
    if argv and argv[0] == "--plan":
        plans = json.loads(db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
        plan = next(g["hash"] for g in plans
                    if g["fields"].get("name") == "hiring-screening")
        report = json.loads(db.run_oversight_report(plan=plan))
    else:
        report = json.loads(db.run_oversight_report(run_id=argv[0] if argv else None))
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


def improve():
    db = open_db()
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
    # A BLANK reason is not caught here on purpose: the engine's own
    # mandatory-BECAUSE gate is the one that has to refuse it, and an act
    # script asserting a driver-side check would be asserting nothing.
    if because is None:
        sys.stderr.write("usage: govern <rec> approve|apply|dismiss "
                         "--because '<why>' --as user:X\n")
        return 2
    db = open_db(actor=actor or "user:anonymous")
    # Resolve across EVERY status, not just pending: a second approval has
    # to come back as the lifecycle violation it is, not as "not found".
    rec = next((r["hash"] for r in json.loads(
                    db.recommendations('{"status": "all"}'))
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


def gate_audit():
    """Count the outcomes, count the humans, and insist they match.

    Read from the RUN JOURNAL, not from this desk's own ledger files: for
    every run, the completed `advance`/`reject` effects and the completed
    `recruiter_review` client effect that produced them, whose `author_did`
    IS the recruiter who answered the ask.

    The invariant: every candidate who reached an outcome went through a
    named person, and that person was never the principal that started the
    run. A regression that quietly drops the human -- an added edge, a
    changed executor_kind, a stray auto-answer -- shows up here as
    `decisions > human_reviews`, whatever caused it.

    The EU AI Act does not treat a human in the loop as a way OUT of the
    high-risk tier (the Commission's Art. 6(5) guidelines of 2026-05-19 are
    explicit that human involvement does not change the classification), so
    this is not a compliance shortcut. It is the thing Article 14 asks you
    to be able to show, and this is where it stops being a promise.
    """
    db = open_db()
    rows = []
    for run_id in json.loads(db.run_list(200)):
        trace = json.loads(db.run_trace(run_id, 400, False, None))["trace"]
        principal = json.loads(db.run_inspect(run_id)).get("principal")
        outcomes, reviewers, app = [], [], None
        for g in trace:
            f = g.get("fields") or {}
            if g.get("grain_type") != "tool":
                continue
            item = ((f.get("input") or {}).get("item") or {})
            app = app or item.get("application_id")
            if f.get("status") != "completed":
                continue
            if f.get("node") in ("advance", "reject"):
                outcomes.append(f["node"])
            elif f.get("node") == "recruiter_review" \
                    and f.get("executor_kind") == "client":
                reviewers.append(f.get("author_did"))
        rows.append({"run_id": run_id, "application_id": app,
                     "started_by": principal, "outcomes": outcomes,
                     "reviewed_by": reviewers})
    emit({
        "runs": rows,
        "decisions": sum(len(r["outcomes"]) for r in rows),
        "human_reviews": sum(len(r["reviewed_by"]) for r in rows),
        "reviewers": sorted({p for r in rows for p in r["reviewed_by"]}),
        # Either of these being non-empty is the regression this exists for.
        "decisions_with_no_human": [r["run_id"] for r in rows
                                    if r["outcomes"] and not r["reviewed_by"]],
        "self_reviewed": [r["run_id"] for r in rows
                          if r["started_by"] in r["reviewed_by"]],
    })
    return 0


def plan():
    """The plan as it is STORED -- nodes, edges, and which node parks.

    The claim "every candidate goes through a person" is not a policy
    sentence here, it is the shape of this graph: nothing reaches `advance`
    or `reject` except out of `recruiter_review`. The act script asserts
    that from the grain, not from the seeder.
    """
    db = open_db()
    plans = json.loads(db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
    wf = next(g for g in plans if g["fields"].get("name") == "hiring-screening")
    tools = json.loads(db.cal('RECALL tools WHERE kind = "definition" '
                              'LIMIT 50 FORMAT json'))["grains"]
    by_hash = {g["hash"]: g["fields"] for g in tools}
    gated = sorted(node for node, h in (wf["fields"].get("bindings") or {}).items()
                   if by_hash.get(h, {}).get("executor_kind") == "client")
    emit({"hash": wf["hash"], "nodes": wf["fields"].get("nodes"),
          "edges": wf["fields"].get("edges"), "client_gated": gated})
    return 0


def precedents():
    db = open_db()
    grains = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.talent.reqs" LIMIT 200 FORMAT json'
    ))["grains"]
    emit([{"subject": g["fields"].get("subject"), "object": g["fields"].get("object")}
          for g in grains
          if g["fields"].get("relation") == "mg:review_precedent"])
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('SHOW GRANTS'))
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
    if cmd == "ingest":
        return ingest()
    if cmd == "asks":
        return asks()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "stop":
        return stop(sys.argv[2:])
    if cmd == "verify":
        return verify(sys.argv[2:])
    if cmd == "oversight":
        return oversight(sys.argv[2:])
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(sys.argv[2:])
    if cmd == "plan":
        return plan()
    if cmd == "gate-audit":
        return gate_audit()
    if cmd == "precedents":
        return precedents()
    if cmd == "brief":
        return brief()
    if cmd == "runs":
        return runs()
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
