#!/usr/bin/env python3
"""rcm-optimization: the whole agent, one file, embedded Areev.

A payer remittance lands carrying many denied claims. The plan does NOT
enumerate them -- it cannot, because the count is a property of the file,
not of the plan. So `split_denials` returns a `$send` list and the runtime
spawns ONE `classify_denial` task per denial, joins the batch before the
downstream edges fire, and folds the per-task results through DECLARED
REDUCERS (`append` for the rows, `sum` for the money and the counters).

That is the property this example exists for: DYNAMIC WIDTH THE PLAN DID
NOT ENUMERATE, merged order-independently and replayable byte-for-byte.

Two subcommands are subprocess seams the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). They never open the memory --
the party that spawned them is holding it:

    agent.py tools        the host tools        ($AREEV_TOOL_NAME picks one)
    agent.py connector    the remittance feed   (fixtures in, items+cursor out)

Everything else is the driver:

    agent.py seed         author the plan (with its reducer table), the tool
                          definitions, the desk's policy, the saved CAL
                          queries, and the remittance trigger
    agent.py plan         the plan hash and the reducer table AS STORED
    agent.py reducer-check   write a plan whose reducer is an OBJECT, prove
                          it stores cleanly, and prove it refuses at RUN
                          START (RUN-E019) -- reducers are validated late
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick)
    agent.py trigger-state        what has actually fired, and what failed
    agent.py asks         the parked runs waiting on a billing lead
    agent.py decide FILE  apply a lead's decision to its parked run
    agent.py verify       journal-consistent replay of every run
    agent.py improve [--grant-auto-apply]
                          the loop reads the desk's own history back
    agent.py govern R approve|apply|dismiss|rollback --because "..." --as user:X
    agent.py mappings     the governed denial-code mappings, from memory
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py queries      the saved CAL queries as stored, with body sizes
    agent.py recommendation H   one recommendation's live lifecycle state
    agent.py runs         run list as JSON (the acts assert on this)

To make it real, replace `tools` and `connector` with processes that read
your 835 remittance feed and write your billing system's worklist. The
plan, the fan-out, the reducer table, the approval gate and the audit
trail do not change.
"""

import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("RCM_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
REMITS = os.path.join(FIXTURES, "remits")
# The acts advance this "clock": the feed serves remittance files whose
# 2-digit prefix is <= it. Week one is 01-02, week two is 03-04.
REMIT_UPTO = os.environ.get("REMIT_UPTO", "02")

OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
CLASSIFIED = os.path.join(OUT, "classified.jsonl")   # one row per denial
CLUSTERS = os.path.join(OUT, "clusters.jsonl")       # one row per remittance
WORKLIST = os.path.join(OUT, "worklist.jsonl")       # what gets resubmitted
REPORTS = os.path.join(OUT, "reports.jsonl")         # one row per remittance
CURSOR = os.path.join(OUT, "telemetry.cursor")

NS = "org.rcm"                  # plan, tool definitions, triggers, journals
POLICY = "org.rcm.policy"       # the desk's own thresholds
MAP = "org.rcm.denials"         # the mappings a billing lead APPROVED
DESK = "agent:rcm-desk"         # the agent -- it can never approve
FEED_SCOPE = "feed:remittance"

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# How many denials must share one cause before this desk will spend a
# billing lead's attention on a fix. It is a FACT in org.rcm.policy, not a
# constant here -- which is what lets the loop propose moving it.
DEFAULT_CLUSTER_FLOOR = 3


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def rows(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def marker(remit_id):
    return hashlib.sha256(remit_id.encode()).hexdigest()[:12]


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state -- EXCEPT for a `$send` task, which gets
# exactly the input its spawn decision named and nothing else. That is the
# whole contract of fan-out: the spawner decides what each task may see.
# Never opens the memory.

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


def signature_for(slug, code_text, root_cause):
    """The telemetry signature the loop clusters on.

    Deliberately DIGIT-FREE: the loop normalizes digit runs to `#`, so a
    signature keyed on `DN-517` would collapse into the same cluster as
    `DN-622` and the desk would learn nothing. Keyed on the payer and the
    denial's own words, every cause clusters separately.
    """
    if root_cause and root_cause != "unmapped":
        return "mapped %s %s" % (slug, root_cause)
    return ("unmapped %s %s" % (slug, code_text))[:78]


def tool_main():
    state = json.load(sys.stdin)
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "split_denials":
        # The ONE node that sees the whole remittance. It reads the desk's
        # governed knowledge out of the trigger's declared context and hands
        # each task exactly what that task needs -- then returns `$send`,
        # which is the plan growing a width it never declared.
        item = state.get("item") or {}
        floor = DEFAULT_CLUSTER_FLOOR
        known = {}
        for g in walk_grains(state.get("context") or {}, []):
            rel = g.get("relation")
            if rel == "min_cluster_size" and g.get("subject") == "denial_clusters":
                floor = int(float(g["object"]))
            elif rel == "root_cause":
                known[g["subject"]] = g["object"]
        crosswalk = (item.get("crosswalk") or {}).get("codes") or {}
        payers = (item.get("crosswalk") or {}).get("payers") or {}
        payer_id = item.get("payer_id")
        slug = (payers.get(payer_id) or {}).get("slug", "unknown")
        tasks = []
        for denial in item.get("denials") or []:
            code = denial.get("denial_code")
            entry = crosswalk.get(code) or {}
            tasks.append({"node": "classify_denial", "input": {
                "denial": denial,
                "remit_id": item.get("remit_id"),
                "payer_id": payer_id,
                "payer_slug": slug,
                "code_text": entry.get("text", code),
                # The crosswalk file only SUGGESTS. Nothing acts on it until
                # a lead has approved it into memory.
                "proposed_root_cause": entry.get("proposed_root_cause"),
                # The memory DECIDES. Absent = this desk has never signed off
                # on what this code means for this payer.
                "known_root_cause": known.get("%s/%s" % (payer_id, code)),
            }})
        emit({
            "remit_id": item.get("remit_id"),
            "payer_id": payer_id,
            "payer_slug": slug,
            "payer_name": item.get("payer_name"),
            "denial_count": len(tasks),
            "cluster_floor": floor,
            "$send": tasks,
        })

    elif tool == "classify_denial":
        # Runs once per denial, under its own task path, with its own retry
        # budget. Its result is folded through the DECLARED reducers:
        # `classified` appends, `denied_cents`/`auto_classified`/`unmapped`
        # sum -- which is what makes the merge order-independent.
        denial = state.get("denial") or {}
        known = state.get("known_root_cause")
        slug = state.get("payer_slug", "unknown")
        code_text = state.get("code_text", "")
        row = {
            "claim_id": denial.get("claim_id"),
            "denial_code": denial.get("denial_code"),
            "code_text": code_text,
            "remit_id": state.get("remit_id"),
            "payer_id": state.get("payer_id"),
            "payer_slug": slug,
            "billed_cents": int(denial.get("billed_cents") or 0),
            "cpt": denial.get("cpt"),
            "root_cause": known or "unmapped",
            "auto": bool(known),
            "proposed_root_cause": state.get("proposed_root_cause"),
            "signature": signature_for(slug, code_text, known),
        }
        append(CLASSIFIED, row)
        emit({
            "classified": [row],
            "denied_cents": row["billed_cents"],
            "auto_classified": 1 if known else 0,
            "unmapped": 0 if known else 1,
        })

    elif tool == "cluster":
        # The join is already done by the time this runs: `classified` is the
        # whole batch, in spawn order, and the sums are closed.
        batch = state.get("classified") or []
        floor = int(state.get("cluster_floor") or DEFAULT_CLUSTER_FLOOR)
        groups = {}
        for r in batch:
            key = (r.get("payer_id"), r.get("denial_code"))
            g = groups.setdefault(key, {
                "payer_id": r.get("payer_id"),
                "payer_slug": r.get("payer_slug"),
                "denial_code": r.get("denial_code"),
                "code_text": r.get("code_text"),
                "root_cause": r.get("root_cause"),
                "proposed_root_cause": r.get("proposed_root_cause"),
                "count": 0, "denied_cents": 0, "claims": [],
            })
            g["count"] += 1
            g["denied_cents"] += r.get("billed_cents") or 0
            g["claims"].append({"claim_id": r.get("claim_id"),
                                "billed_cents": r.get("billed_cents")})
        clusters = sorted(groups.values(),
                          key=lambda c: (-c["denied_cents"], c["denial_code"]))
        # A fix is proposable when the desk has NO governed mapping for the
        # cause and the cluster is big enough to be a pattern rather than a
        # week of noise. Both halves matter: the floor is what stops this
        # desk from spending a lead's attention on a coincidence.
        openers = [c for c in clusters
                   if c["root_cause"] == "unmapped" and c["count"] >= floor]
        top = openers[0] if openers else None
        proposal = None
        if top:
            proposal = {
                "payer_id": top["payer_id"],
                "payer_slug": top["payer_slug"],
                "denial_code": top["denial_code"],
                "code_text": top["code_text"],
                "root_cause": top["proposed_root_cause"] or "unclassified",
                "count": top["count"],
                "denied_cents": top["denied_cents"],
                "claims": top["claims"],
                "fix": "map %s/%s to %s, add the matching pre-submission edit, "
                       "and requeue the %d claims for resubmission"
                       % (top["payer_id"], top["denial_code"],
                          top["proposed_root_cause"] or "unclassified",
                          top["count"]),
            }
        out = {
            "classified_count": len(batch),
            "claim_order": [r.get("claim_id") for r in batch],
            "clusters": clusters,
            "actionable": bool(top),
            "proposal": proposal,
        }
        append(CLUSTERS, dict(out,
                              remit_id=state.get("remit_id"),
                              payer_id=state.get("payer_id"),
                              denial_count=state.get("denial_count"),
                              denied_cents=state.get("denied_cents"),
                              auto_classified=state.get("auto_classified"),
                              unmapped=state.get("unmapped"),
                              cluster_floor=floor))
        emit(out)

    elif tool == "apply_fix":
        proposal = state.get("proposal") or {}
        queued = 0
        for claim in proposal.get("claims") or []:
            append(WORKLIST, {
                "claim_id": claim.get("claim_id"),
                "billed_cents": claim.get("billed_cents"),
                "remit_id": state.get("remit_id"),
                "payer_id": proposal.get("payer_id"),
                "denial_code": proposal.get("denial_code"),
                "root_cause": proposal.get("root_cause"),
                "action": proposal.get("fix"),
                "approved_by": state.get("responder"),
                "because": state.get("because"),
            })
            queued += 1
        emit({"resubmissions_queued": queued, "fix_applied": True})

    elif tool == "file_report":
        proposal = state.get("proposal") or {}
        append(REPORTS, {
            "remit_id": state.get("remit_id"),
            "payer_id": state.get("payer_id"),
            "denial_count": state.get("denial_count"),
            "classified_count": state.get("classified_count"),
            "denied_cents": state.get("denied_cents"),
            "auto_classified": state.get("auto_classified", 0),
            "unmapped": state.get("unmapped", 0),
            "proposed": proposal.get("denial_code"),
            "decision": state.get("decision", "no_proposal"),
            "decided_by": state.get("responder"),
            "because": state.get("because"),
            "resubmissions_queued": state.get("resubmissions_queued", 0),
        })
        emit({"report_filed": True})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the connector seam -----------------------------------------------------
# An ABSENT cursor means "seed and fire nothing", so declaring a trigger
# never replays the feed. REMIT_UPTO is the clock the acts advance.

def connector_main():
    req = json.load(sys.stdin)
    with open(os.path.join(FIXTURES, "codebook.json")) as fh:
        crosswalk = json.load(fh)
    names = sorted(n for n in os.listdir(REMITS)
                   if n.endswith(".json") and n[:2] <= REMIT_UPTO)
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(REMITS, name)) as fh:
            payload = json.load(fh)
        # The crosswalk is a file: it SUGGESTS a root cause and rides with
        # the item. The APPROVED mappings come from memory, through the
        # trigger's declared context query. Only the second one decides.
        payload["crosswalk"] = crosswalk
        items.append({"id": payload["remit_id"], "payload": payload})
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


PLAN_NAME = "rcm-denial-optimization"

# The reducer table, declared on the plan. STRING values -- `lww`, `append`,
# `sum`, `max`, `min`. Anything else (an object, a typo) writes cleanly and
# refuses at RUN START with RUN-E019; `agent.py reducer-check` proves it.
REDUCERS = {
    "classified": "append",      # one row per denial, in spawn order
    "denied_cents": "sum",       # the money, order-independent
    "auto_classified": "sum",    # how many the memory already knew
    "unmapped": "sum",           # how many it did not
}


def plan_fields(bindings):
    return {
        "name": PLAN_NAME,
        "nodes": ["split_denials", "classify_denial", "cluster",
                  "lead_review", "apply_fix", "file_report"],
        "edges": [
            # The fan-out edge. `classify_denial`'s STATIC activation is
            # preempted by the spawn decision -- it runs once per task, not
            # once for the batch -- and `cluster` waits for the join.
            {"src": "split_denials", "dst": "classify_denial"},
            {"src": "classify_denial", "dst": "cluster"},
            {"src": "cluster", "dst": "lead_review", "cond": "actionable == true"},
            {"src": "cluster", "dst": "file_report", "cond": "actionable == false"},
            {"src": "lead_review", "dst": "apply_fix",
             "cond": 'decision == "approve"'},
            {"src": "lead_review", "dst": "file_report",
             "cond": 'decision == "reject"'},
            {"src": "apply_fix", "dst": "file_report"},
        ],
        "bindings": bindings,
        # Per-task retry budget: a task that fails retries under its OWN
        # task path, not the whole batch.
        "retries": {"classify_denial": 1},
        "reducers": REDUCERS,
        "created_at": EPOCH_MS,
    }


def seed():
    db = open_db()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    bindings = {
        "split_denials": tool_def(
            "split_denials", "read a remittance and spawn one screening task "
                             "per denied claim"),
        "classify_denial": tool_def(
            "classify_denial", "assign one denial to a root cause the desk has "
                               "a governed mapping for"),
        "cluster": tool_def(
            "cluster", "group the batch by payer and denial code and propose "
                       "at most one fix"),
        # THE HUMAN GATE. A `client` executor parks the run; the principal
        # that started it structurally cannot answer it.
        "lead_review": tool_def(
            "lead_review", "a billing lead approves or rejects the proposed "
                           "fix, with a written reason",
            executor_kind="client"),
        "apply_fix": tool_def(
            "apply_fix", "queue the denied claims for resubmission under the "
                         "approved fix"),
        "file_report": tool_def(
            "file_report", "file the remittance's outcome, decided or not"),
    }

    wf = db.add("workflow", json.dumps(plan_fields(bindings)), ns=NS)

    db.add("skill", json.dumps({
        "name": "denial-judgment",
        "description": "how this desk reads a wall of denials",
        "instructions": "A cluster is a pattern only when it repeats; three "
                        "claims in one week is noise. Never resubmit under a "
                        "root cause a lead has not approved -- the crosswalk "
                        "file suggests, the memory decides. A rejected "
                        "proposal is not a mapping: it is a reason to look "
                        "again with more evidence.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # The desk's own rules. Facts, not constants -- so the loop can propose
    # moving them and a person can approve the move.
    db.add_fact("denial_clusters", "min_cluster_size",
                str(DEFAULT_CLUSTER_FLOOR), ns=POLICY, idempotent=True)
    db.add_fact("denial_clusters", "crosswalk_version", "2026-07-01",
                ns=POLICY, idempotent=True)

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE mapping_line AS '
           '"- {{subject}} {{relation}} {{object}} ({{confidence}})"')
    db.cal('DEFINE QUERY "rcm_ctx"($session) '
           'DESCRIPTION "what the desk must know before it opens a remittance" '
           'AS { ASSEMBLE "rcm_ctx" FROM '
           'judgment: (RECALL skills LIMIT 2), '
           'desk: (RECALL facts WHERE namespace = "org.rcm.*" LIMIT 200) '
           'BUDGET 4000 tokens FORMAT json }')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, mappings" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'lessons: (RECALL facts WHERE namespace = "org.rcm.*" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    trigger = db.trigger_add(json.dumps({
        "kind": "polling",
        "connector": "mock",
        "scope": FEED_SCOPE,
        "interval_secs": 1,
        "workflow": wf,
        "dedup_key": ["/remit_id"],
        "context_query": "rcm_ctx($session = /remit_id)",
    }), "work every payer remittance the day it lands", NS)

    emit({"workflow": wf, "trigger": trigger, "reducers": REDUCERS})
    return 0


def stored_plan(db, name=PLAN_NAME):
    plans = json.loads(db.cal('RECALL workflows LIMIT 20 FORMAT json'))["grains"]
    return next(g for g in plans if g["fields"].get("name") == name)


def plan():
    """The plan hash and the reducer table AS STORED -- not as authored.

    The act script asserts the values are STRINGS here, because that is the
    one thing the write path will not check for you.
    """
    db = open_db()
    grain = stored_plan(db)
    emit({"workflow": grain["hash"],
          "reducers": grain["fields"].get("reducers"),
          "nodes": grain["fields"].get("nodes"),
          "retries": grain["fields"].get("retries")})
    return 0


def reducer_check():
    """Reducers are validated LATE. Prove both halves of that.

    `reducers` is an untyped passthrough on the Workflow grain: an object
    where a string belongs stores cleanly, mints a content address, and
    replicates -- and then refuses at every later RUN START with RUN-E019.
    A plan you can save is not a plan you can run.
    """
    db = open_db()
    fields = dict(stored_plan(db)["fields"])
    for drop in ("namespace", "type", "confidence"):
        fields.pop(drop, None)
    fields["name"] = "rcm-reducer-probe"
    # The mistake: the shape a reasonable person writes.
    fields["reducers"] = dict(REDUCERS, classified={"kind": "append"})
    probe = db.add("workflow", json.dumps(fields), ns=NS)
    try:
        db.run_start(workflow=probe, run_id="reducer-probe",
                     tool_cmd=self_cmd("tools"), input_json="{}")
    except ValueError as e:
        emit({"written": probe, "refused": True, "error": str(e)})
        return 0
    emit({"written": probe, "refused": False})
    return 1


def record_new_classifications(db):
    """Telemetry the DRIVER writes, after the run returns.

    A tool subprocess must never open the memory the runtime is holding, so
    every classification lands in a file first and becomes a Tool grain
    here. One grain per denial: a mapped one is a success, an unmapped one
    is an error -- which is exactly what the loop clusters on.

    Recorded under `denial_root_cause`, NOT under the node name
    `classify_denial`, and that is deliberate. The run journal already
    writes an execution Tool grain per node dispatch, under the node's own
    tool name; the loop's rate gate divides a cluster by that tool's
    opportunities, so telemetry sharing a name with a node is divided by
    the journal's own volume and quietly stops firing. A distinct name
    keeps the desk's signal its own denominator.
    """
    seen = 0
    if os.path.exists(CURSOR):
        with open(CURSOR, encoding="utf-8") as fh:
            seen = int((fh.read() or "0").strip() or 0)
    batch = rows(CLASSIFIED)
    for row in batch[seen:]:
        db.record_tool_call(
            "denial_root_cause", row["signature"],
            is_error=(row["root_cause"] == "unmapped"),
            call_id="%s:%s" % (row["remit_id"], row["claim_id"]))
    with open(CURSOR, "w", encoding="utf-8") as fh:
        fh.write(str(len(batch)))
    return len(batch) - seen


def ingest():
    """One heartbeat tick: claim, poll, dedup, start."""
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))
    report["classifications_recorded"] = record_new_classifications(db)
    emit(report)
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


def asks():
    db = open_db()
    out = []
    for run_id, ask_id, state in pending_asks(db):
        proposal = state.get("proposal") or {}
        out.append({
            "run_id": run_id, "ask": ask_id,
            "marker": marker(state.get("remit_id") or "?"),
            "remit_id": state.get("remit_id"),
            "payer_id": state.get("payer_id"),
            "denial_count": state.get("denial_count"),
            "classified_count": state.get("classified_count"),
            "denied_cents": state.get("denied_cents"),
            "auto_classified": state.get("auto_classified"),
            "unmapped": state.get("unmapped"),
            "claim_order": state.get("claim_order"),
            "proposed_code": proposal.get("denial_code"),
            "proposed_root_cause": proposal.get("root_cause"),
            "proposed_claims": len(proposal.get("claims") or []),
            "proposed_cents": proposal.get("denied_cents"),
        })
    emit(out)
    return 0


def decide(path):
    """A billing lead's decision, read from a fixture the way a worklist
    webhook or an email reply would deliver it."""
    with open(path) as fh:
        note = json.load(fh)
    principal = note.get("lead", "user:unknown")
    ref = note.get("marker")
    verdict = note.get("decision")
    because = note.get("because", "")
    if verdict not in ("approve", "reject") or not because:
        sys.stderr.write("a fix decision needs a verdict and a written reason\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        if marker(state.get("remit_id") or "?") != ref:
            continue
        result = {"decision": verdict, "responder": principal, "because": because}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        proposal = state.get("proposal") or {}
        if verdict == "approve":
            # THE LESSON. An approved mapping is a fact, and it is what the
            # next remittance is classified against -- not the crosswalk
            # file, which only ever suggested it.
            db.add_fact("%s/%s" % (proposal.get("payer_id"),
                                   proposal.get("denial_code")),
                        "root_cause", proposal.get("root_cause"),
                        ns=MAP, idempotent=True)
            db.record_tool_call(
                "denial_fix",
                "fix %s %s" % (proposal.get("payer_slug"),
                               proposal.get("root_cause")),
                is_error=False, run_id=run_id)
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        emit({"run_id": run_id, "decision": verdict, "responder": principal,
              "mapped": proposal.get("denial_code") if verdict == "approve" else None,
              "outcome": outcome})
        return 0
    sys.stderr.write("no parked run matches marker %s\n" % ref)
    return 5


def verify():
    """Journal-consistent replay of every run: re-derive each checkpoint and
    byte-compare it. This is what makes the fan-out's merge a claim rather
    than a hope -- the reducers fold identically on replay or it fails."""
    db = open_db()
    out = []
    for run_id in json.loads(db.run_list(100)):
        report = json.loads(db.run_verify(run_id))
        out.append({"run_id": run_id, "verified": report.get("verified"),
                    "supersteps": report.get("supersteps")})
    emit(out)
    return 0


def mappings():
    db = open_db()
    grains = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.rcm.denials" LIMIT 100 '
        'FORMAT json'))["grains"]
    emit(sorted(({"subject": g["fields"].get("subject"),
                  "relation": g["fields"].get("relation"),
                  "object": g["fields"].get("object")} for g in grains),
                key=lambda r: r["subject"] or ""))
    return 0


def improve(argv):
    """One analysis pass over the desk's own record.

    `--grant-auto-apply` writes a host policy file that GRANTS this
    analyzer family auto-apply on memory targets, up to high severity --
    and the engine still applies nothing, because `loop.tool_failure`'s own
    manifest is `auto_apply: Never`: its finding is derived from free text
    the engine did not author. The host can only ever restrict; it cannot
    grant past an engine ceiling. That is the gate worth seeing fail.
    """
    db = open_db()
    # Tune the analyzer to this desk's volume -- a recorded act of
    # configuration, not a fork. Five is "it survived a second remittance",
    # which is exactly the bar a lead rejected the first proposal for.
    db.set_analyzer_config("loop.tool_failure/1", True,
                           json.dumps({"min_count": 5, "min_rate": 0.4}))
    policy = None
    if "--grant-auto-apply" in argv:
        policy = os.path.join(OUT, "loop-policy.json")
        with open(policy, "w", encoding="utf-8") as fh:
            json.dump({"auto_apply_enabled": True,
                       "auto_apply": [{"analyzer": "loop.tool_failure",
                                       "targets": ["memory"],
                                       "max_severity": "high"}]}, fh)
    if "--grant-llm-auto-apply" in argv:
        # The widest grant a host could misconfigure for a MODEL-authored
        # change: name the llm family and the query class outright. The
        # engine still applies nothing, for two independent reasons --
        # `origin = llm` is categorically auto-apply-ineligible, and
        # `grants_auto_apply` admits only the `memory` class, so the query
        # leg of this grant is inert. A grain edit changes one remembered
        # value; a definition rewrite changes what every future briefing
        # contains.
        policy = os.path.join(OUT, "loop-policy-llm.json")
        with open(policy, "w", encoding="utf-8") as fh:
            json.dump({"auto_apply_enabled": True,
                       "auto_apply": [{"analyzer": "loop.llm",
                                       "targets": ["query", "memory"],
                                       "max_severity": "high"}]}, fh)
    report = json.loads(db.loop_run(llm_cmd=os.environ.get("LOOP_LLM_CMD"),
                                    policy=policy))
    recs = json.loads(db.recommendations('{"status": "pending"}'))
    emit({"loop": report,
          "pending": [{"hash": r.get("hash"), "severity": r.get("severity"),
                       "summary": r.get("summary"), "analyzer": r.get("analyzer"),
                       "target": r.get("target_ref")} for r in recs]})
    return 0


def govern(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: govern <rec> approve|apply|dismiss|rollback "
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
    # `--because ""` is deliberately let through: the ENGINE refuses it with
    # LOP-E011, and a driver that swallowed it would hide the real gate.
    if because is None:
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
        elif action == "rollback":
            # For a definition rewrite this is the load-bearing one: a
            # DEFINE writes a registry row, not a grain, so the ordinary
            # "retract what the apply created" would undo nothing while
            # reporting success. The engine refuses to APPLY a definition
            # change whose inverse it could not record, which is what makes
            # this call able to put the old body back.
            out = db.rollback_recommendation(rec, because)
        else:
            sys.stderr.write("unknown action %r\n" % action)
            return 2
    except ValueError as e:
        sys.stderr.write("refused: %s\n" % e)
        return 4
    print(out)
    return 0


def recommendation(argv):
    """One recommendation's live lifecycle state, by hash prefix.

    Status is index-layer state, not part of the immutable body, so it has to
    be read back rather than inferred from the propose-time report.
    """
    if not argv:
        sys.stderr.write("usage: recommendation <hash-prefix>\n")
        return 2
    db = open_db()
    rows = json.loads(db.recommendations(None))
    hit = next((r for r in rows if r["hash"].startswith(argv[0])), None)
    if hit is None:
        sys.stderr.write("no recommendation matching %r\n" % argv[0])
        return 4
    emit(hit)
    return 0


def queries():
    """The saved CAL queries as stored IN the file, with their body sizes.

    The size is the honest, cheap way to see a definition rewrite land and be
    taken back: the body is host metadata (a `qry:` row), not a grain, so it
    has no content address to compare and `DESCRIBE QUERIES` is the only
    read that reports it.
    """
    db = open_db()
    emit(json.loads(db.cal("DESCRIBE QUERIES"))["info"]["queries"])
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.rcm.*" LIMIT 20 '
                 'FORMAT TEMPLATE mapping_line'))
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal(
        'RECALL observations WHERE namespace = "agent:harness" RECENT 200 '
        'FORMAT json'))
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
    if cmd == "plan":
        return plan()
    if cmd == "reducer-check":
        return reducer_check()
    if cmd == "ingest":
        return ingest()
    if cmd == "trigger-state":
        return trigger_state()
    if cmd == "asks":
        return asks()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "verify":
        return verify()
    if cmd == "mappings":
        return mappings()
    if cmd == "improve":
        return improve(sys.argv[2:])
    if cmd == "queries":
        return queries()
    if cmd == "recommendation":
        return recommendation(sys.argv[2:])
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
