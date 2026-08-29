#!/usr/bin/env python3
"""due diligence: the whole agent, one file, embedded Areev.

The property this example exists for: A BUDGET IS A CONTROL, NOT AN ERROR.

Research is open-ended and expensive, so every diligence request runs under
a ceiling. When the ceiling is reached the run does not crash and does not
silently truncate -- it finishes `BudgetExhausted`, which is a TERMINAL
STATE THAT IS RESUMABLE. The journal survives with the spend on it, an
analyst reads what the money bought and what is still unread, and then
decides: fork it under a raised ceiling, or ship the partial report. That
decision is the human gate, and the desk cannot take it.

Everything the desk did is replayable afterwards. `verify` re-derives every
checkpoint and byte-compares it against the stored chain; `shadow` replays
whole runs with ZERO effect dispatches, so the ledger does not move while
you audit it.

Two subcommands are subprocess seams the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). They never open the memory --
the party that spawned them is holding it:

    agent.py tools --run RUN   the research legs and the effects
                          ($AREEV_TOOL_NAME picks one; the host names the
                          run it is serving, because a FORK inherits the
                          base run's context and would otherwise file its
                          findings under the base run's id)

Everything else is the driver:

    agent.py seed              author the plan, the tool definitions, the
                               desk's own rules, the saved CAL queries
    agent.py file FIXTURE      file a diligence request and run it
                               [--ceiling-ms N] [--as user:X] [--run-id ID]
    agent.py inspect RUN       what the ceiling bought: spend, legs read,
                               legs still unread, fork lineage
    agent.py resume RUN        continue a run from its last checkpoint
    agent.py fork RUN NEW      a new run descending from RUN's last
                               checkpoint, under a raised ceiling
    agent.py asks              runs parked on a partner's signature
    agent.py sign FIXTURE      apply a partner's review to its parked run
    agent.py verify RUN...     journal-consistent replay (writes nothing)
    agent.py shadow RUN...     replay with zero effect dispatches
    agent.py oversight RUN     the EU AI Act Article 14 report
    agent.py book --upto NN    run the rest of the quarter's request book
    agent.py brief             the desk briefing itself (saved CAL queries)
    agent.py improve           the loop reads the desk's own journals back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py adopt R --policy jurisdiction=leg --as user:X
                               a desk-policy change that must cite an
                               APPROVED finding
    agent.py runs              run list + outcomes as JSON

To make it real, replace `tools` with processes that call your registry
provider, your docket search and your media vendor. The plan, the ceiling,
the journal, the partner gate and the replay verbs do not change.
"""

import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("DD_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
REQUESTS = os.path.join(FIXTURES, "requests")
RECORDS = os.path.join(FIXTURES, "records")
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
FINDINGS = os.path.join(OUT, "findings.jsonl")   # what the research pulled
REPORTS = os.path.join(OUT, "reports.jsonl")     # what left the building

NS = "org.ops"                    # plan, tool definitions, run journals
DESK = "org.diligence"            # the desk's own standing rules
LEARNED = "org.diligence.learned"  # what partners taught it, with reasons
# Reads use the "org.diligence.*" prefix (see the saved queries in seed());
# writes and policy take the exact namespace.
AGENT = "agent:diligence-desk"

# Pinned so the seeder mints a stable plan hash. A grain is its bytes.
EPOCH_MS = 1756000000000

# The checklist, in the order the desk's procedure manual writes it.
LEG_ORDER = "adverse_media,corporate_filings,financials,litigation"

# What one research leg costs in wall time. A real registry pull, docket
# search or media sweep takes seconds; this is the simulated stand-in that
# makes the wall-clock ceiling mean something on a laptop. The acts set it
# to 0 for the bulk runs, where the ceiling is not the point.
LEG_MS = int(os.environ.get("DD_LEG_MS", "1500"))

# The desk's standing ceiling for one request. Sized so it buys part of the
# checklist and not all of it -- which is the whole demonstration.
DEFAULT_CEILING_MS = int(os.environ.get("DD_CEILING_MS", "2900"))


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def read_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def request_path(name):
    if os.path.isabs(name) or os.path.exists(name):
        return name
    return os.path.join(REQUESTS, name)


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state. `request` and `desk` were put there by
# the driver at run start; everything else was merged from earlier nodes
# through the plan's reducers. Never opens the memory.

def leg_queue_for(request, desk):
    """The order this desk will work the checklist in, for THIS target.

    The default order is the procedure manual's. Memory only ever demotes:
    a leg a partner marked low-yield for the sector goes to the back, it is
    never dropped. Nobody signed off on not looking.
    """
    order = [x for x in (desk.get("leg_order") or LEG_ORDER).split(",") if x]
    demoted = [leg for leg in order
               if leg in (desk.get("low_yield") or {}).get(request.get("sector"), [])]
    return [leg for leg in order if leg not in demoted] + demoted


def tool_main(argv):
    state = json.load(sys.stdin)
    request = state.get("request") or {}
    desk = state.get("desk") or {}
    tool = os.environ.get("AREEV_TOOL_NAME", "")
    run_id = flags(argv).get("run") or "?"

    if tool == "intake":
        queue = leg_queue_for(request, desk)
        emit({
            "request_id": request.get("request_id"),
            "target": request.get("target"),
            "leg_queue": ",".join(queue),
            "leg_plan_source": ("memory: the sector's low-yield leg was demoted"
                                if queue != [x for x in LEG_ORDER.split(",")]
                                else "the procedure manual's default order"),
        })

    elif tool == "next_leg":
        queue = [x for x in (state.get("leg_queue") or "").split(",") if x]
        done = state.get("legs_done") or []
        remaining = [leg for leg in queue if leg not in done]
        if remaining:
            emit({"next_leg": remaining[0], "more": "yes"})
        else:
            emit({"next_leg": "", "more": "no"})

    elif tool == "research":
        leg = state.get("next_leg")
        book = read_json(os.path.join(RECORDS, "%s.json" % request.get("target_id")))
        source = book["legs"].get(leg) or {"source": "unknown", "records": []}
        # The cost of going and looking. This is what the ceiling is buying.
        if LEG_MS:
            time.sleep(LEG_MS / 1000.0)
        if source.get("unavailable"):
            tolerated = (desk.get("gap_tolerated") or {}).get(
                request.get("jurisdiction"), [])
            if leg not in tolerated:
                # Loud, not silent. An unread leg the desk has no rule for
                # is not a gap you can write into a report -- it is a
                # research failure, and it stops the run.
                sys.stderr.write("%s: %s\n" % (leg, source["unavailable"]))
                return 7
            append(FINDINGS, {
                "run_id": run_id,
                "request_id": request.get("request_id"),
                "target": request.get("target"),
                "leg": leg,
                "source": source.get("source"),
                "ref": None,
                "summary": "GAP: %s" % source["unavailable"],
                "material": False,
                "severity": "none",
                "gap": True,
            })
            emit({"legs_done": [leg],
                  "gaps": ["%s: %s" % (leg, source["unavailable"])],
                  "material_count": 0})
            return 0
        material = 0
        for rec in source.get("records") or []:
            if rec.get("material"):
                material += 1
            append(FINDINGS, {
                "run_id": run_id,
                "request_id": request.get("request_id"),
                "target": request.get("target"),
                "leg": leg,
                "source": source.get("source"),
                "ref": rec.get("ref"),
                "summary": rec.get("summary"),
                "material": bool(rec.get("material")),
                "severity": rec.get("severity"),
            })
        emit({"legs_done": [leg],
              "findings": ["%s/%s" % (leg, r.get("ref"))
                           for r in (source.get("records") or [])
                           if r.get("material")],
              "material_count": material})

    elif tool == "assemble":
        emit({"draft_ready": "yes",
              "material_total": state.get("material_count") or 0,
              "legs_read": len(state.get("legs_done") or []),
              "gap_count": len(state.get("gaps") or [])})

    elif tool == "issue_report":
        append(REPORTS, {
            "run_id": run_id,
            "request_id": request.get("request_id"),
            "target": request.get("target"),
            "outcome": "issued",
            "legs_read": state.get("legs_done") or [],
            "material_findings": state.get("findings") or [],
            "gaps": state.get("gaps") or [],
            "issued_by": state.get("responder"),
            "because": state.get("because"),
        })
        emit({"issued": "yes"})

    elif tool == "shelve":
        append(REPORTS, {
            "run_id": run_id,
            "request_id": request.get("request_id"),
            "target": request.get("target"),
            "outcome": "shelved",
            "legs_read": state.get("legs_done") or [],
            "shelved_by": state.get("responder"),
            "because": state.get("because"),
        })
        emit({"shelved": "yes"})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the driver -------------------------------------------------------------

def open_db(actor=AGENT, attempts=40):
    """One handle, and never two.

    The embedded backend is single-writer: one process holds the file. A
    driver subcommand opens per invocation and releases it on return; the
    `tools` seam never opens the memory at all, because the runtime that
    spawned it is holding it.

    A previous holder that has only just exited can still be tearing its
    handle down, so a lock is WAITED ON briefly rather than treated as
    fatal. Without that, whether two back-to-back subcommands work depends
    on how loaded the machine is -- which is a flake, not a contract.
    """
    import areev
    os.makedirs(OUT, exist_ok=True)
    for attempt in range(attempts):
        try:
            return areev.Areev(DB, ns=NS, actor=actor)
        except ValueError as e:
            locked = "STO-E001" in str(e) or "STO-E002" in str(e)
            if not locked or attempt == attempts - 1:
                raise
            time.sleep(0.25)


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


def tools_for(run_id):
    """The `--tool-cmd` seam, told which run it is serving."""
    return self_cmd("tools --run %s" % run_id)


def flags(argv):
    out, it = {}, iter(argv)
    for token in it:
        if token.startswith("--"):
            out[token[2:]] = next(it, "")
    return out


def plan_hash(db):
    plans = json.loads(db.cal('RECALL workflows LIMIT 20 FORMAT json'))["grains"]
    return next(g["hash"] for g in plans
                if g["fields"].get("name") == "due-diligence")


def desk_notes(db):
    """Everything memory has to say about how to work a file.

    Retrieval lives IN the memory as a saved CAL query, so it replicates
    with the file instead of living in this script. The driver -- which
    holds the single writer -- reads it and hands the result to the run as
    input; the tool processes never open the memory.
    """
    grains = json.loads(db.cal('RUN "desk_notes"()')).get("grains") or []
    notes = {"leg_order": LEG_ORDER, "low_yield": {}, "gap_tolerated": {},
             "why": []}
    for g in grains:
        f = g.get("fields") or {}
        subject, relation, obj = f.get("subject"), f.get("relation"), f.get("object")
        if relation == "leg_order":
            notes["leg_order"] = obj
        elif relation == "mg:low_yield_leg":
            notes["low_yield"].setdefault(subject, []).append(obj)
            notes["why"].append("%s: run %s last" % (subject, obj))
        elif relation == "mg:leg_gap_tolerated":
            notes["gap_tolerated"].setdefault(subject, []).append(obj)
            notes["why"].append("%s: an unpublished %s is a gap, not a failure"
                                % (subject, obj))
    return notes


def seed():
    db = open_db()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    bindings = {
        "intake": tool_def("intake", "read the request and order the checklist"),
        "next_leg": tool_def("next_leg", "pick the next unread leg, or stop"),
        "research": tool_def("research", "work one leg of the checklist"),
        "assemble": tool_def("assemble", "draft the diligence report"),
        # The gate. executor_kind=client is what makes the run PARK instead
        # of deciding for itself; every client ask is an approval boundary,
        # so separation of duties is structural.
        "partner_review": tool_def(
            "partner_review", "a partner signs the report out, or shelves it",
            executor_kind="client"),
        "issue_report": tool_def("issue_report", "issue the report to the requester"),
        "shelve": tool_def("shelve", "shelve the file without issuing"),
    }

    wf = db.add("workflow", json.dumps({
        "name": "due-diligence",
        "nodes": ["intake", "next_leg", "research", "assemble",
                  "partner_review", "issue_report", "shelve"],
        # next_leg -> research -> next_leg is the checklist loop; the back
        # edge is bounded, and issue_report/shelve are the two dead ends
        # that let the run finish at all.
        "edges": [
            {"src": "intake", "dst": "next_leg"},
            {"src": "next_leg", "dst": "research", "cond": 'more == "yes"'},
            {"src": "next_leg", "dst": "assemble", "cond": 'more == "no"'},
            {"src": "research", "dst": "next_leg", "max_cycles": 8},
            {"src": "assemble", "dst": "partner_review"},
            {"src": "partner_review", "dst": "issue_report",
             "cond": 'decision == "issue"'},
            {"src": "partner_review", "dst": "shelve",
             "cond": 'decision == "shelve"'},
        ],
        "bindings": bindings,
        # Findings accumulate across the loop rather than overwriting.
        "reducers": {"legs_done": "append", "findings": "append",
                     "gaps": "append", "material_count": "sum"},
        "retries": {"research": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "diligence-judgment",
        "description": "how this desk spends a research budget",
        "instructions": "Research is unbounded; the budget is not. Read the "
                        "legs most likely to change the conclusion first. A "
                        "leg that returns nothing material three files running "
                        "is a leg to run last, never a leg to drop -- nobody "
                        "signed off on not looking. An unread leg is a gap "
                        "that must be written into the report, and a gap "
                        "nobody has a rule for stops the file.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The desk's own standing rules. Facts, not constants in this script --
    # which is what lets a partner change them without a deploy.
    db.add_fact("diligence", "leg_order", LEG_ORDER, ns=DESK, idempotent=True)
    db.add_fact("diligence", "ceiling_ms", str(DEFAULT_CEILING_MS),
                ns=DESK, idempotent=True)

    # Retrieval and presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE note_line AS '
           '"- {{subject}} {{relation}} {{object}}"')
    db.cal('DEFINE QUERY "desk_notes"() '
           'DESCRIPTION "what memory has to say about how to work a file" '
           'AS { RECALL facts WHERE namespace = "org.diligence.*" '
           'LIMIT 200 FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, checklist, lessons" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'checklist: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'judgment: (RECALL skills LIMIT 2), '
           'lessons: (RECALL facts WHERE namespace = "org.diligence.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    emit({"workflow": wf, "legs": LEG_ORDER,
          "ceiling_ms": DEFAULT_CEILING_MS, "leg_ms": LEG_MS})
    return 0


def summarize(db, run_id, session):
    """What an operator wants back from a run: the outcome string, what the
    checklist got through, and what it did not.

    `finished` is a Rust Debug rendering of the outcome enum
    (`BudgetExhausted { axis: WallMs }`, `Failed { node: "research", .. }`).
    It is a human-readable label, not a stable token -- match a substring
    of it, never the whole string.
    """
    report = json.loads(db.run_inspect(run_id))
    session = json.loads(session)
    row = {"run_id": run_id,
           "finished": session.get("finished"),
           "parked": bool(session.get("parked")),
           "phase": report.get("phase"),
           "supersteps": report.get("superstep"),
           "spent_wall_ms": (report.get("spent") or {}).get("wall_ms"),
           "ceiling_ms": (report.get("budgets") or {}).get("max_wall_ms"),
           "asks": len(report.get("pending_asks") or {})}
    row.update(legs_of(db, run_id))
    return row


def lineage(db, run_id):
    """A run and every run it descends from.

    A fork re-executes only what the base run had not reached, so "what has
    this file read" is a question about the whole chain, not one run.
    """
    chain, seen = [], set()
    while run_id and run_id not in seen:
        seen.add(run_id)
        chain.append(run_id)
        try:
            report = json.loads(db.run_inspect(run_id))
        except ValueError:
            break
        run_id = (report.get("fork_of") or {}).get("base_run")
    return chain


def run_context(db, run_id):
    """The run's own merged state, read off its LAST CHECKPOINT.

    This -- not the effects ledger -- is the authoritative record of what a
    run did. A checkpoint is a State grain whose `context` field carries the
    serialized scheduler state, and a FORK's seed checkpoint carries the
    base run's context verbatim. So "what has this file read" needs no
    lineage walk and no cross-run bookkeeping in a side file: ask the
    journal, which is the thing `run_verify` and `run_shadow` replay.
    """
    after, newest = 0, None
    while True:
        page = json.loads(db.run_grains(run_id, after, 512))
        entries = page.get("entries") or []
        if not entries:
            break
        for entry in entries:
            grain = entry.get("grain") or {}
            if grain.get("grain_type") == "state":
                newest = grain
        after = page.get("next_after_seq")
        if not after:
            break
    if newest is None:
        return {}
    context = (newest.get("fields") or {}).get("context") or {}
    if isinstance(context, str):
        context = json.loads(context)
    scheduler = context.get("scheduler") if isinstance(context, dict) else None
    return (scheduler or {}).get("context") or {}


def legs_of(db, run_id):
    """The checklist state for one run, off that run's own journal."""
    context = run_context(db, run_id)
    done = list(context.get("legs_done") or [])
    queue = [x for x in (context.get("leg_queue") or LEG_ORDER).split(",") if x]
    return {"legs_read": done,
            "legs_unread": [leg for leg in queue if leg not in done],
            "material_findings": int(context.get("material_count") or 0),
            "gaps": [g.split(":")[0] for g in (context.get("gaps") or [])]}


def file_request(argv):
    """File a diligence request and run it under a ceiling."""
    if not argv:
        sys.stderr.write("usage: file FIXTURE [--ceiling-ms N] [--as user:X]\n")
        return 2
    request = read_json(request_path(argv[0]))
    opt = flags(argv[1:])
    principal = opt.get("as") or request.get("requested_by") or AGENT
    ceiling = opt.get("ceiling-ms")
    ceiling = None if ceiling in ("", "none") else int(ceiling or DEFAULT_CEILING_MS)
    run_id = opt.get("run-id") or request["request_id"].lower()

    db = open_db(actor=principal)
    session = db.run_start(
        workflow=plan_hash(db), run_id=run_id, tool_cmd=tools_for(run_id),
        input_json=json.dumps({"request": request, "desk": desk_notes(db)}),
        max_wall_ms=ceiling, ask_ttl_sec=86400)
    emit(summarize(db, run_id, session))
    return 0


def inspect_run(run_id):
    db = open_db()
    report = json.loads(db.run_inspect(run_id))
    row = {"run_id": run_id,
           "phase": report.get("phase"),
           "ceiling_ms": (report.get("budgets") or {}).get("max_wall_ms"),
           "spent": report.get("spent"),
           "checkpoints": report.get("checkpoints"),
           "journal_entries": report.get("journal_entries"),
           "fork_of": report.get("fork_of"),
           "pending_asks": len(report.get("pending_asks") or {}),
           "lineage": lineage(db, run_id)}
    row.update(legs_of(db, run_id))
    emit(row)
    return 0


def fork_run(argv):
    """A new run descending from RUN's last checkpoint.

    This is how a BudgetExhausted run continues. The exhausted run is
    never touched: its journal is the record of what the first ceiling
    bought. The fork inherits every pinned resolution verbatim -- budgets
    and TTL are the only knobs it gets, which is exactly the point, and
    the FORKER becomes the fork's triggering principal, so whoever raises
    the ceiling is the person who may not sign the report off.
    """
    if len(argv) < 2:
        sys.stderr.write("usage: fork RUN NEW [--as user:X]\n")
        return 2
    base, new = argv[0], argv[1]
    opt = flags(argv[2:])
    db = open_db(actor=opt.get("as") or AGENT)
    try:
        seed_hash = db.run_fork(base, new)
    except ValueError as e:
        sys.stderr.write("fork refused: %s\n" % e)
        return 4
    emit({"forked": new, "from": base, "seed_checkpoint": seed_hash,
          "ceiling_ms": (json.loads(db.run_inspect(new)).get("budgets")
                         or {}).get("max_wall_ms")})
    return 0


def resume_run(argv):
    if not argv:
        sys.stderr.write("usage: resume RUN [--as user:X]\n")
        return 2
    run_id = argv[0]
    opt = flags(argv[1:])
    db = open_db(actor=opt.get("as") or AGENT)
    session = db.run_resume(run_id, tool_cmd=tools_for(run_id))
    emit(summarize(db, run_id, session))
    return 0


def pending_asks(db):
    out = []
    for run_id in json.loads(db.run_list(200)):
        report = json.loads(db.run_inspect(run_id))
        if report.get("phase") != "open":
            continue
        for ask_id, entry in (report.get("pending_asks") or {}).items():
            out.append((run_id, ask_id, (entry.get("ask") or {}).get("input") or {}))
    return out


def asks():
    db = open_db()
    rows = []
    for run_id, ask_id, state in pending_asks(db):
        request = state.get("request") or {}
        rows.append({"run_id": run_id, "ask": ask_id,
                     "request_id": request.get("request_id"),
                     "target": request.get("target"),
                     "legs_read": state.get("legs_done") or [],
                     "gaps": state.get("gaps") or [],
                     "material_total": state.get("material_count") or 0,
                     "waiting_on": "a partner's signature"})
    emit(rows)
    return 0


def sign(path):
    """A partner's review, read from a fixture the way a case-management
    webhook would deliver it.

    Two refusals live here, and neither is a convention:
      * no written reason -> refused by this desk, before the runtime;
      * the principal that started the run -> refused by the RUNTIME,
        because every client ask is an approval boundary.
    """
    note = read_json(path)
    partner = note.get("partner", "user:unknown")
    decision = note.get("decision")
    because = (note.get("because") or "").strip()
    if decision not in ("issue", "shelve"):
        sys.stderr.write("a review is 'issue' or 'shelve'\n")
        return 3
    if not because:
        sys.stderr.write("a diligence report is not signed out without a "
                         "written reason\n")
        return 3

    db = open_db(actor=partner)
    for run_id, ask_id, state in pending_asks(db):
        request = state.get("request") or {}
        if request.get("request_id") != note.get("request_id"):
            continue
        result = {"decision": decision, "responder": partner, "because": because}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), partner)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        session = db.run_resume(run_id, tool_cmd=tools_for(run_id))
        # What the partner learned on this file, kept with their name and
        # their reason on it -- written only once the decision has actually
        # taken effect. This is the grain act two reads back.
        if note.get("note_low_yield_leg"):
            db.add_fact(request.get("sector"), "mg:low_yield_leg",
                        note["note_low_yield_leg"], ns=LEARNED, idempotent=True)
            db.add("observation", json.dumps({
                "observer_id": partner,
                "observer_type": "human",
                "subject": "%s/%s" % (request.get("sector"),
                                      note["note_low_yield_leg"]),
                "content": note.get("note_because") or because,
                "observation_kind": "leg_yield_note",
            }), ns=LEARNED)
        row = summarize(db, run_id, session)
        row.update({"decision": decision, "signed_by": partner})
        emit(row)
        return 0
    sys.stderr.write("no parked run is waiting on %s\n" % note.get("request_id"))
    return 5


def verify(run_ids):
    """Journal-consistent replay: every checkpoint re-derived and
    byte-compared against the stored chain. Writes nothing."""
    db = open_db()
    out = []
    for run_id in run_ids:
        report = json.loads(db.run_verify(run_id))
        out.append({"run_id": run_id, "verified": report.get("verified"),
                    "steps": len(report.get("steps") or [])})
    emit(out)
    return 0


def shadow(run_ids):
    """Replay whole runs from their journals with ZERO effect dispatches --
    the report says so in its own body, and the ledgers prove it."""
    db = open_db()
    report = json.loads(db.run_shadow(list(run_ids)))
    emit({"all_consistent": report.get("all_consistent"),
          "effect_dispatches": report.get("effect_dispatches"),
          "runs": [{"run_id": r.get("run_id"), "consistent": r.get("consistent"),
                    "effects_replayed": r.get("effects_replayed")}
                   for r in report.get("runs") or []]})
    return 0


def oversight(run_id):
    db = open_db()
    emit(json.loads(db.run_oversight_report(run_id=run_id)))
    return 0


def book(argv):
    """Work the rest of the quarter's request book.

    No ceiling: these are the routine files, and what they are here to
    produce is a run history the loop can read.
    """
    opt = flags(argv)
    upto = opt.get("upto", "99")
    ceiling = opt.get("ceiling-ms")
    # The 2-digit fixture prefix is the clock the acts advance.
    names = [n for n in sorted(os.listdir(REQUESTS))
             if n.endswith(".json") and n[:2] <= upto]
    # ONE handle for the whole batch. One memory is one writer: opening a
    # second handle per request fails at open (STO-E002), by design.
    db = open_db(actor=opt.get("as") or AGENT)
    plan = plan_hash(db)
    started = set(json.loads(db.run_list(400)))
    rows = []
    for name in names:
        request = read_json(os.path.join(REQUESTS, name))
        run_id = request["request_id"].lower()
        if run_id in started:
            continue
        try:
            session = db.run_start(
                workflow=plan, run_id=run_id, tool_cmd=tools_for(run_id),
                input_json=json.dumps({"request": request,
                                       "desk": desk_notes(db)}),
                max_wall_ms=None if ceiling in (None, "", "none") else int(ceiling),
                ask_ttl_sec=86400)
        except ValueError as e:
            rows.append({"run_id": run_id, "request_id": request["request_id"],
                         "error": str(e)})
            continue
        row = summarize(db, run_id, session)
        row["request_id"] = request["request_id"]
        rows.append(row)
    emit(rows)
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal('RECALL observations WHERE namespace = "agent:harness" '
                            'RECENT 400 FORMAT json'))
    outcome, detail = {}, {}
    for g in obs.get("grains") or []:
        f = g.get("fields") or {}
        if f.get("observation_kind") == "run_outcome":
            outcome[f.get("run_id")] = f.get("object")
            detail[f.get("run_id")] = f.get("outcome_detail")
    emit([{"run_id": r, "outcome": outcome.get(r, "open"),
           "detail": detail.get(r)}
          for r in json.loads(db.run_list(200))])
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.diligence.*" LIMIT 20 '
                 'FORMAT TEMPLATE note_line'))
    return 0


def improve():
    db = open_db()
    # Tuned to this desk's volume -- a recorded act of configuration, not a
    # fork of the analyzer.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_runs": 3, "min_failure_ratio": 0.3}))
    report = json.loads(db.loop_run(llm_cmd=os.environ.get("LOOP_LLM_CMD")))
    recs = json.loads(db.recommendations('{"status": "pending"}'))
    emit({"loop": report,
          "pending": [{"hash": r.get("hash"), "severity": r.get("severity"),
                       "summary": r.get("summary"), "analyzer": r.get("analyzer"),
                       "target": r.get("target_ref")} for r in recs]})
    return 0


ALL_RECS = '{"status": "all"}'


def resolve_rec(db, prefix):
    return next((r["hash"] for r in json.loads(db.recommendations(ALL_RECS))
                 if r["hash"].startswith(prefix)), prefix)


def govern(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: govern <rec> approve|apply|dismiss "
                         "--because ... --as user:X\n")
        return 2
    prefix, action = argv[0], argv[1]
    opt = flags(argv[2:])
    if not (opt.get("because") or "").strip():
        sys.stderr.write("a decision with no written reason is refused\n")
        return 2
    db = open_db(actor=opt.get("as") or "user:anonymous")
    rec = resolve_rec(db, prefix)
    try:
        if action == "approve":
            out = db.approve_recommendation(rec, opt["because"])
        elif action == "apply":
            out = db.apply_recommendation(rec, opt["because"])
        elif action == "dismiss":
            out = db.dismiss_recommendation(rec, opt["because"])
        else:
            sys.stderr.write("unknown action %r\n" % action)
            return 2
    except ValueError as e:
        sys.stderr.write("refused: %s\n" % e)
        return 4
    print(out)
    return 0


def adopt(argv):
    """Turn an APPROVED finding into a standing desk rule.

    The loop's findings are advisory: the engine may not execute its own
    advice, and this desk will not either. `adopt` refuses to write a rule
    that does not cite a recommendation a named person has approved -- so
    the rule and the reason for it are one record, not two.
    """
    if not argv:
        sys.stderr.write("usage: adopt <rec> --policy JURISDICTION=LEG "
                         "--as user:X\n")
        return 2
    opt = flags(argv[1:])
    policy = opt.get("policy") or ""
    if "=" not in policy:
        sys.stderr.write("--policy takes JURISDICTION=LEG\n")
        return 2
    jurisdiction, leg = policy.split("=", 1)
    actor = opt.get("as") or "user:anonymous"
    db = open_db(actor=actor)
    rec_hash = resolve_rec(db, argv[0])
    rec = next((r for r in json.loads(db.recommendations(ALL_RECS))
                if r["hash"] == rec_hash), None)
    if rec is None:
        sys.stderr.write("no such finding: %s\n" % argv[0])
        return 4
    if rec.get("status") != "approved":
        sys.stderr.write("a desk rule must cite an APPROVED finding; %s is %s\n"
                         % (rec_hash[:12], rec.get("status")))
        return 4
    fact = db.add_fact(jurisdiction, "mg:leg_gap_tolerated", leg,
                       ns=LEARNED, idempotent=True)
    db.add("observation", json.dumps({
        "observer_id": actor,
        "observer_type": "human",
        "subject": "%s/%s" % (jurisdiction, leg),
        "content": "adopted from finding %s: %s" % (rec_hash[:12],
                                                    rec.get("summary")),
        "observation_kind": "desk_rule_adopted",
    }), ns=LEARNED)
    emit({"adopted": rec_hash, "by": actor, "fact": fact,
          "rule": "%s: an unpublished %s is a gap, not a failure"
                  % (jurisdiction, leg)})
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    rest = sys.argv[2:]
    if cmd == "tools":
        return tool_main(rest)
    if cmd == "seed":
        return seed()
    if cmd == "file":
        return file_request(rest)
    if cmd == "inspect":
        return inspect_run(rest[0])
    if cmd == "fork":
        return fork_run(rest)
    if cmd == "resume":
        return resume_run(rest)
    if cmd == "asks":
        return asks()
    if cmd == "sign":
        return sign(rest[0])
    if cmd == "verify":
        return verify(rest)
    if cmd == "shadow":
        return shadow(rest)
    if cmd == "oversight":
        return oversight(rest[0])
    if cmd == "book":
        return book(rest)
    if cmd == "runs":
        return runs()
    if cmd == "brief":
        return brief()
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(rest)
    if cmd == "adopt":
        return adopt(rest)
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
