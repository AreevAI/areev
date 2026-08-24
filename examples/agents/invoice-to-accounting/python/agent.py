#!/usr/bin/env python3
"""invoice -> accounting: the whole agent, one file, embedded Areev.

Two subcommands are subprocess seams the Areev runtime spawns (JSON on
stdin, JSON on stdout, one process per invocation). They never open the
memory -- the party that spawned them is holding it:

    agent.py tools        the host tools      ($AREEV_TOOL_NAME picks one)
    agent.py connector    the mailbox poll    (fixtures in, items+cursor out)

Everything else is the driver. It embeds Areev in-process (pip install
areev) and is what a heartbeat runs:

    agent.py seed         author the plan, tool definitions, saved CAL
                          queries, starting facts, the two mailbox triggers
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick)
    agent.py asks         the parked runs waiting on a person
    agent.py reply FILE   classify a reply email and respond to its run
    agent.py improve      the loop reads the desk's own history back
    agent.py decide R approve|apply|dismiss --because "..." --as user:X
    agent.py teach NS SUBJECT RELATION OBJECT
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the smoke asserts on this)

To make it real, replace `tools` and `connector` with processes that call
your mailbox and your accounting API (see ../connectors/ for live Outlook
and Gmail connectors). The plan, the journal, the approval gate, and the
audit trail do not change.
"""

import hashlib
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("MAIL_FIXTURES", os.path.join(EXAMPLE, "fixtures", "mail"))
MAIL_UPTO = os.environ.get("MAIL_UPTO", "03")  # the acts advance this "clock"
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
SHEET = os.path.join(OUT, "sheet.jsonl")
OUTBOX = os.path.join(OUT, "outbox.jsonl")

NS = "org.ops"           # triggers, plan, tool definitions, journals, raw mail
DESK = "agent:ap-desk"   # the agent's own principal -- it can never approve
DESK_FROM = "ap-desk@desk.example"

# One mailbox per client; the client's knowledge lives under org.<client>.
MAILBOXES = {"acme": "ap-acme@desk.example", "brightco": "ap-brightco@desk.example"}
APPROVER = {"acme": "dana@acme.example", "brightco": "priya@brightco.example"}

# Pinned so every language's seeder mints the SAME content addresses --
# created_at is part of a grain's bytes, and a grain is its bytes.
EPOCH_MS = 1756000000000

CONFIDENCE_FLOOR = 0.75
DEFAULT_THRESHOLD = 2500.0


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def marker(message_id):
    return hashlib.sha256(message_id.encode()).hexdigest()[:12]


# ── the tools seam ─────────────────────────────────────────────────────────
# stdin is the run's merged state. On the trigger path the email is under
# "item" and the trigger's declared context under "context"; a run started
# by hand passes its --input through unchanged -- resolve it once.

def walk_grains(node, out):
    """Collect {subject, relation, object} dicts from assembled JSON context."""
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

    if tool == "parse_attachments":
        # A photographed invoice has no text layer. Failing loudly is the
        # correct behaviour: a silent empty extraction posts a blank row.
        if item.get("scanned"):
            sys.stderr.write(
                "pdftotext produced 0 characters - attachment is a scanned image\n"
            )
            return 1
        emit({"texts": [{"filename": item.get("attachment", "invoice.pdf"), "chars": 4180}]})

    elif tool == "extract_rows":
        # The real one sends the PDF text to a model. This one reads the
        # fixture's own fields -- deterministic, so the assertions mean
        # something -- but it applies the same memory the real one would:
        # an alias fact recorded from a past correction canonicalizes the
        # vendor and settles the confidence question it used to raise.
        vendor = item.get("vendor", "unknown")
        confidence = float(item.get("confidence", 0.95))
        aliases = {g["subject"]: g["object"] for g in grains
                   if g.get("relation") == "mg:alias_of"}
        if vendor in aliases:
            vendor = aliases[vendor]
            confidence = max(confidence, 0.95)
        emit({
            "rows": 1,
            "vendor": vendor,
            "amount": item.get("amount", 0),
            "currency": item.get("currency", "USD"),
            "category": item.get("category", "Software"),
            "field_confidence": confidence,
            "client": item.get("client", "unknown"),
            "message_id": item.get("message_id", "?"),
            "thread": item.get("thread", "?"),
            "sender": item.get("sender", "?"),
        })

    elif tool == "validate_rows":
        # The threshold is a fact in org.<client>, delivered through the
        # trigger's declared context -- not a constant in this script. That
        # is what lets the loop propose changing it, with a written reason.
        client = state.get("client", "unknown")
        threshold = DEFAULT_THRESHOLD
        for g in grains:
            if g.get("relation") == "review_threshold_usd" and g.get("subject") == client:
                threshold = float(g["object"])
        amount = float(state.get("amount", 0))
        confidence = float(state.get("field_confidence", 1.0))
        needs_review = amount >= threshold or confidence < CONFIDENCE_FLOOR
        emit({
            "needs_review": needs_review,
            "row_key": "%s#0" % state.get("message_id", "?"),
            "review_reason": "amount at or above threshold" if amount >= threshold
            else ("field confidence below floor" if confidence < CONFIDENCE_FLOOR else "clear"),
        })

    elif tool == "send_ask":
        # Always the client's approver, never the external sender. Getting
        # that backwards emails your vendor an approval link. The marker in
        # the subject is how a reply finds its run again (a mailto: reply
        # cannot be trusted to keep In-Reply-To).
        client = state.get("client", "unknown")
        append(OUTBOX, {
            "to": APPROVER.get(client, "unknown"),
            "subject": "Approve this expense: %s %s %s [areev:ap/%s]" % (
                state.get("vendor"), state.get("amount"), state.get("currency"),
                marker(state.get("message_id", "?"))),
            "vendor": state.get("vendor"),
            "amount": state.get("amount"),
            "reason": state.get("review_reason"),
            "reply_with": "approve | reject | revise + `Field: value` lines",
        })
        emit({"ask_sent": True})

    elif tool == "apply_corrections":
        # The human replied `revise` with Field: value lines. Merge them,
        # mark the corrected fields settled, and go back around to re-ask
        # -- the plan bounds this cycle with max_cycles.
        merged = {}
        for field, value in (state.get("corrections") or {}).items():
            if field in ("vendor", "currency", "category"):
                merged[field] = str(value)
            elif field == "amount":
                merged[field] = float(value)
        merged["field_confidence"] = 1.0
        merged["revised"] = True
        emit(merged)

    elif tool == "append_sheet":
        row = {
            "row_key": state.get("row_key"),
            "client": state.get("client"),
            "vendor": state.get("vendor"),
            "amount": state.get("amount"),
            "currency": state.get("currency"),
            "category": state.get("category"),
            "approved_by": state.get("responder", "auto"),
        }
        append(SHEET, row)
        emit({"appended": 1, "row_key": row["row_key"]})

    elif tool == "reply_email":
        append(OUTBOX, {
            "to": state.get("sender", "?"),
            "subject": "Re: %s" % state.get("message_id", "?"),
            "outcome": "rejected" if state.get("decision") == "reject" else "posted",
        })
        emit({"sent": True})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# ── the connector seam ─────────────────────────────────────────────────────
# The contract from docs/triggers.md: an ABSENT cursor means "seed and fire
# nothing", so declaring a trigger never replays mailbox history. The mock's
# cursor is "how many fixture files were consumed"; MAIL_UPTO is the clock
# the smoke advances to make week two arrive.

def connector_main():
    req = json.load(sys.stdin)
    mailbox = (req.get("scope") or "").removeprefix("mailbox:")
    client = next((c for c, m in MAILBOXES.items() if m == mailbox), None)
    folder = os.path.join(FIXTURES, client or "?")
    names = sorted(n for n in os.listdir(folder)
                   if n.endswith(".json") and n[:2] <= MAIL_UPTO) if client else []
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(folder, name)) as fh:
            payload = json.load(fh)
        items.append({"id": payload["message_id"], "payload": payload})
    emit({"items": items, "cursor": str(consumed + len(items)),
          "more": consumed + len(items) < len(names)})
    return 0


# ── the driver ─────────────────────────────────────────────────────────────

def open_db(actor=DESK):
    import areev
    os.makedirs(OUT, exist_ok=True)
    return areev.Areev(DB, ns=NS, actor=actor)


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


def seed():
    db = open_db()

    def tool_def(name, description, executor_kind=None):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        if executor_kind:
            fields["executor_kind"] = executor_kind
        return db.add("tool", json.dumps(fields), ns=NS)

    parse = tool_def("parse_attachments", "pull the text layer out of each attachment")
    extract = tool_def("extract_rows", "turn invoice text into expense rows")
    validate = tool_def("validate_rows", "decide whether a person has to look")
    ask = tool_def("send_ask", "email the client's approver, with a marker")
    review = tool_def("human_review", "a person decides: approve, revise, or reject",
                      executor_kind="client")
    corrections = tool_def("apply_corrections", "merge the approver's Field: value lines")
    sheet = tool_def("append_sheet", "append the approved row to the expense sheet")
    reply = tool_def("reply_email", "tell the sender what happened")

    wf = db.add("workflow", json.dumps({
        "name": "invoice-to-accounting",
        "nodes": ["parse_attachments", "extract_rows", "validate_rows", "send_ask",
                  "human_review", "apply_corrections", "append_sheet",
                  "reply_done", "reply_rejected"],
        "edges": [
            {"src": "parse_attachments", "dst": "extract_rows"},
            {"src": "extract_rows", "dst": "validate_rows"},
            {"src": "validate_rows", "dst": "append_sheet", "cond": "needs_review == false"},
            {"src": "validate_rows", "dst": "send_ask", "cond": "needs_review == true"},
            {"src": "send_ask", "dst": "human_review"},
            {"src": "human_review", "dst": "append_sheet", "cond": 'decision == "approve"'},
            {"src": "human_review", "dst": "apply_corrections", "cond": 'decision == "revise"'},
            {"src": "human_review", "dst": "reply_rejected", "cond": 'decision == "reject"'},
            # The correction cycle: revise -> merge -> re-ask, at most 3 times.
            {"src": "apply_corrections", "dst": "send_ask", "max_cycles": 3},
            {"src": "append_sheet", "dst": "reply_done"},
        ],
        "bindings": {"parse_attachments": parse, "extract_rows": extract,
                     "validate_rows": validate, "send_ask": ask,
                     "human_review": review, "apply_corrections": corrections,
                     "append_sheet": sheet,
                     # Two nodes, one tool: both replies are the same effect.
                     "reply_done": reply, "reply_rejected": reply},
        "retries": {"extract_rows": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "invoice-triage",
        "description": "how this desk reads an invoice",
        "instructions": "Extract one row per invoice. Prefer the canonical vendor "
                        "name from the alias facts. Never guess an amount: a "
                        "low-confidence field goes to review, not to the sheet.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # Client knowledge lives under org.<client> -- exact namespaces to write,
    # a "org.*" prefix to read the whole desk in one query.
    db.add_fact("acme", "review_threshold_usd", "2500", ns="org.acme", idempotent=True)
    db.add_fact("brightco", "review_threshold_usd", "2500", ns="org.brightco", idempotent=True)
    db.add_fact("Meridian Freight", "payment_terms", "net_30", ns="org.acme.vendors", idempotent=True)
    db.add_fact("Cobalt Cloud", "payment_terms", "net_45", ns="org.brightco.vendors", idempotent=True)

    # Retrieval + presentation ship IN the file as saved queries/templates
    # (qry:/tpl: meta rows) -- they replicate with the memory and are what
    # the triggers below name as declared context.
    db.cal('DEFINE TEMPLATE vendor_line AS '
           '"- {{subject}} {{relation}} {{object}} ({{confidence}})"')
    db.cal('DEFINE QUERY "extract_ctx"($session) '
           'DESCRIPTION "what extraction should know before reading an invoice" '
           'AS { ASSEMBLE "extract_ctx" FROM '
           'instructions: (RECALL skills LIMIT 2), '
           'desk: (RECALL facts WHERE namespace = "org.*" LIMIT 120), '
           'thread: (RECALL events WHERE session_id = $session RECENT 10) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, lessons, outcomes" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'lessons: (RECALL facts WHERE namespace = "org.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    # Egress anonymization starts in audit mode on the client subtrees --
    # measure before you rewrite. NEVER on org.ops: the rewriter would
    # mangle the operational JSON (dates, 64-char hashes) that lives there.
    db.set_anon_policy("org.acme", '{"mode": "audit"}')
    db.set_anon_policy("org.brightco", '{"mode": "audit"}')

    triggers = {}
    for client, mailbox in sorted(MAILBOXES.items()):
        triggers[client] = db.trigger_add(json.dumps({
            "kind": "polling",
            "connector": "mock",
            "scope": "mailbox:%s" % mailbox,
            "interval_secs": 1,
            "workflow": wf,
            "dedup_key": ["/message_id"],
            "context_query": "extract_ctx($session = /thread)",
        }), "poll the %s AP mailbox for invoices" % client, NS)

    emit({"workflow": wf, "triggers": triggers})
    return 0


def ingest():
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))
    emit(report)
    return 0


def pending_asks(db):
    """[(run_id, ask_id, merged_state)] for every parked run."""
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
                     "marker": marker(item.get("message_id", "?")),
                     "vendor": state.get("vendor"), "amount": state.get("amount"),
                     "reason": state.get("review_reason")})
    emit(rows)
    return 0


CUTOFF = re.compile(r"^On .* wrote:$|^-+ ?Original Message|^From: ")
FIELD = re.compile(r"^(Vendor|Amount|Currency|Category):\s*(.+)$", re.I)


def classify(body):
    """Deterministic reply reading: verb first, then Field: value lines.
    Quoted history is cut, so a reply that quotes the ask does not
    re-approve itself."""
    verb, corrections = None, {}
    for raw in body.splitlines():
        line = raw.strip()
        if line.startswith(">") or CUTOFF.match(line):
            break
        if not line:
            continue
        m = FIELD.match(line)
        if m:
            corrections[m.group(1).lower()] = m.group(2).strip()
        elif verb is None:
            verb = line.split()[0].lower()
    if verb == "reject":
        return {"decision": "reject"}
    if verb == "revise" or corrections:
        return {"decision": "revise", "corrections": corrections}
    if verb == "approve":
        return {"decision": "approve"}
    return None


def reply(path):
    with open(path) as fh:
        mail = json.load(fh)
    sender = mail.get("from", "?")
    principal = DESK if sender == DESK_FROM else "user:" + sender.split("@")[0]
    ref = re.search(r"\[areev:ap/([0-9a-f]{12})\]", mail.get("subject", ""))
    verdict = classify(mail.get("body", ""))
    if not ref or verdict is None:
        sys.stderr.write("unclassified reply -- left unactioned, a person reads it\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        if marker(item.get("message_id", "?")) != ref.group(1):
            continue
        result = dict(verdict, responder=principal)
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        # A correction the approver then approved is a lesson worth keeping:
        # record the alias where the client's knowledge lives, and record
        # the correction itself as a tool outcome the loop can cluster.
        if verdict["decision"] == "approve" and state.get("revised") \
                and state.get("vendor") != item.get("vendor"):
            ns = "org.%s.vendors" % state.get("client", "unknown")
            db.add_fact(item["vendor"], "mg:alias_of", state["vendor"], ns=ns, idempotent=True)
        # Record each corrected field as a failed extract outcome. The result
        # string IS the loop's cluster key (normalized, truncated at 80
        # chars) -- keep it short and stable, never free prose.
        for field in (verdict.get("corrections") or {}):
            db.record_tool_call(
                "extract_rows", "corr:%s:%s" % (field, state.get("client", "?")),
                is_error=True, thread=item.get("thread"), run_id=run_id)
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        emit({"run_id": run_id, "decision": verdict["decision"],
              "responder": principal, "outcome": outcome})
        return 0
    sys.stderr.write("no parked run matches marker %s\n" % ref.group(1))
    return 5


def improve():
    db = open_db()
    # Tune the analyzers to this desk's volume: at ~4 invoices a week the
    # stock "half of all runs failed" bar would stay silent for a quarter.
    # Tuning is itself a recorded act of configuration, not a fork.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_failure_ratio": 0.4}))
    # Optional LLM reflection (DISCOVER->GROUND->VERIFY) on top of the
    # deterministic floor: LOOP_LLM_CMD names any --llm-cmd backend (see
    # examples/llm/), and the loop grounds every model finding in grains
    # before it may become a recommendation. Keyless runs skip it.
    report = json.loads(db.loop_run(llm_cmd=os.environ.get("LOOP_LLM_CMD")))
    recs = json.loads(db.recommendations('{"status": "pending"}'))
    emit({"loop": report,
          "pending": [{"hash": r.get("hash"), "severity": r.get("severity"),
                       "summary": r.get("summary"), "analyzer": r.get("analyzer"),
                       "target": r.get("target_ref")} for r in recs]})
    return 0


def decide(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: decide <rec> approve|apply|dismiss --because ... --as user:X\n")
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
    print(db.cal('RECALL facts WHERE namespace = "org.*" LIMIT 20 '
                 'FORMAT TEMPLATE vendor_line'))
    return 0


def runs():
    # Outcome the same way `areev run list` derives it: the run-terminal
    # Observation (`observation_kind = "run_outcome"`) the runtime writes in
    # agent:harness -- there is no separate log to join against.
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
    if cmd == "reply":
        return reply(sys.argv[2])
    if cmd == "improve":
        return improve()
    if cmd == "decide":
        return decide(sys.argv[2:])
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
