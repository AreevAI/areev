#!/usr/bin/env python3
"""insurance documents: a policy servicing desk, one file, embedded Areev.

The difference from every other agent example in this repo is WHEN THE
MEMORY IS READ. Not "what does the file say now" -- two different questions,
on two different clocks:

    world time      what cover was actually IN FORCE at time T
                    (valid_from / valid_to on the grain)
    knowledge time  what this desk KNEW at time T
                    (system_valid_from, which is the grain's created_at,
                     walked back down the supersession chain)

A claim is assessed on the world axis: the loss happened on a date, and the
cover that responds is the cover in force on that date -- not the cover the
file holds today. A coverage dispute, or a regulator asking "what did you
tell the insured in June?", is answered on the knowledge axis. An endorsement
that is BACKDATED -- effective earlier than it was recorded -- makes the two
axes disagree, and that disagreement is the whole reason this example exists.

    agent.py as-of POLICY RELATION      both axes, side by side

Two subcommands are subprocess seams the runtime spawns (JSON on stdin, JSON
on stdout, one process per invocation). They never open the memory -- the
party that spawned them is holding it:

    agent.py tools        the host tools ($AREEV_TOOL_NAME picks one)

Everything else is the driver:

    agent.py seed         author the plan, the tool definitions, the schedule
                          as issued, the entity graph, the saved CAL queries
    agent.py intake       process the inbound document queue (DOC_UPTO is the
                          clock the acts advance); one run per document
    agent.py as-of P R    the two-axis as-of table for (policy, relation)
    agent.py exposure P   the `related` accumulation walk, per direction
    agent.py asks         runs parked on an underwriter
    agent.py determine F  apply an underwriter's determination to its run
    agent.py trace CLAIM  what the determination was actually made against
    agent.py desk-rule SOURCE ACTION --because ... --as user:X
    agent.py improve      the loop reads the desk's own run history back
    agent.py govern R approve|apply|dismiss --because ... --as user:X
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py runs         run list as JSON (the acts assert on this)

To make it real, replace `tools` with processes that call your policy admin
system and your claims platform, and feed `intake` from a document connector.
The plan, the journal, the approval gate, the two clocks and the audit trail
do not change.
"""

import datetime as dt
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("DOC_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
INBOUND = os.path.join(FIXTURES, "inbound")
DOC_UPTO = os.environ.get("DOC_UPTO", "05")   # the acts advance this "clock"
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
DETERMINATIONS = os.path.join(OUT, "determinations.jsonl")
REFERRALS = os.path.join(OUT, "referrals.jsonl")
CHANGES = os.path.join(OUT, "changes.jsonl")

NS = "org.uw"                    # plan, tool definitions, document state
POLICY_NS = "org.uw.policies"    # the coverage picture + the entity graph
WORDING_NS = "org.uw.wordings"   # clause readings an underwriter settled
DESK_NS = "org.uw.desk"          # the desk's own rules
DESK = "agent:servicing-desk"    # the agent -- it can never sign a determination

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# The desk's "today". Pinned so every as-of read in the acts is deterministic;
# in production this is `now`.
TODAY = "2026-08-01"

# `related` walks only these two relations. Both are in the OMS entity-valued
# vocabulary the store declares by default, which is what makes the reverse
# ("in") direction work at all -- see README, "what actually worked".
GRAPH_RELATIONS = "mg:owned_by,part_of"


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def ms(datestr):
    """A calendar date on either clock, as epoch milliseconds (UTC)."""
    d = dt.datetime.strptime(datestr, "%Y-%m-%d")
    return int(d.replace(tzinfo=dt.timezone.utc).timestamp() * 1000)


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state. Never opens the memory: the driver is
# holding it, and every memory-shaped answer these tools need was resolved
# BEFORE the run started and pinned into its input -- which is also why the
# journal can prove what each determination was made against.

def tool_main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "extract":
        kind = item.get("doc_kind")
        rules = state.get("desk_rules") or {}
        common = {"document_id": item.get("document_id"),
                  "policy_id": item.get("policy_id"),
                  "source": item.get("source"),
                  "doc_kind": kind,
                  "received_at": item.get("received_at")}
        if kind == "claim_notice":
            if not item.get("date_of_loss"):
                sys.stderr.write("a claim notice with no date of loss cannot be "
                                 "assessed against any date\n")
                return 1
            common.update({"route": "claim", "date_of_loss": item["date_of_loss"],
                           "claim_id": item.get("document_id")})
            emit(common)
            return 0
        if kind in ("endorsement", "correction", "cancellation"):
            if not item.get("effective_from"):
                # A coverage document with no effective date has no place on
                # either clock. Refusing is the whole point -- guessing "today"
                # would silently rewrite when cover attached.
                if rules.get("on_missing_effective_date") == "refer_back":
                    common.update({"route": "referral",
                                   "referral_reason": "no effective date on the document"})
                    emit(common)
                    return 0
                sys.stderr.write("%s carries no effective date: it cannot be placed "
                                 "on the world clock\n" % item.get("document_id"))
                return 1
            common.update({"route": "change",
                           "effective_from": item["effective_from"],
                           "restates": bool(item.get("restates"))})
            emit(common)
            return 0
        sys.stderr.write("unknown document kind: %r\n" % kind)
        return 1

    if tool == "assess_cover":
        as_of = state.get("as_of") or {}
        world = as_of.get("world") or {}
        if not world.get("object"):
            # No cover in force on the date of loss. A determination against
            # nothing is not a determination.
            sys.stderr.write("no cover in force on %s for %s\n"
                             % (state.get("date_of_loss"), state.get("policy_id")))
            return 1
        settled = state.get("settled_reading") or {}
        amount = float(item.get("amount") or 0)
        limit = float(world.get("object") or 0)
        emit({
            "limit_in_force": world.get("object"),
            "cover_grain": world.get("hash"),
            "limit_now": (as_of.get("head") or {}).get("object"),
            "known_at_notice": (as_of.get("knowledge") or {}).get("object"),
            "deductible_in_force": (as_of.get("deductible") or {}).get("object"),
            "claim_amount": item.get("amount"),
            "uninsured_excess": "%d" % max(0.0, amount - limit),
            "wording_clause": item.get("clause"),
            "settled_by": settled.get("settled_by"),
        })
        return 0

    if tool == "accumulation":
        ex = state.get("exposure") or {}
        agg = float(ex.get("aggregate_exposure") or 0)
        cap = float(ex.get("accumulation_limit") or 0)
        emit({
            "insured": ex.get("insured"),
            "aggregate_exposure": ex.get("aggregate_exposure"),
            "accumulation_limit": ex.get("accumulation_limit"),
            "accumulation_flag": bool(cap and agg >= cap),
            "own_policies": ex.get("own_policies"),
            "group_policies": ex.get("group_policies"),
            # A settled clause reading carries the underwriter's earlier
            # signature forward. It is NOT a licence to skip a flagged
            # accumulation -- that still goes to a person.
            "settled": bool(state.get("settled_reading")) and not (cap and agg >= cap),
        })
        return 0

    if tool == "issue_determination":
        row = {
            "claim_id": state.get("claim_id"),
            "policy_id": state.get("policy_id"),
            "date_of_loss": state.get("date_of_loss"),
            "decision": state.get("decision", "cover_confirmed"),
            "limit_in_force": state.get("limit_in_force"),
            "limit_now": state.get("limit_now"),
            "cover_grain": state.get("cover_grain"),
            "deductible_in_force": state.get("deductible_in_force"),
            "claim_amount": state.get("claim_amount"),
            "uninsured_excess": state.get("uninsured_excess"),
            "accumulation_flag": state.get("accumulation_flag"),
            "aggregate_exposure": state.get("aggregate_exposure"),
            "because": state.get("because"),
        }
        if state.get("responder"):
            row["determined_by"] = state["responder"]
            row["authority"] = "underwriter"
        else:
            row["determined_by"] = state.get("settled_by")
            row["authority"] = "settled-wording %s" % state.get("wording_clause")
            row["because"] = ("applied the reading %s settled on %s"
                              % (state.get("settled_by"), state.get("wording_clause")))
        append(DETERMINATIONS, row)
        emit({"determination_issued": True, "claim_id": row["claim_id"]})
        return 0

    if tool == "refer_back":
        append(REFERRALS, {"document_id": state.get("document_id"),
                           "policy_id": state.get("policy_id"),
                           "source": state.get("source"),
                           "reason": state.get("referral_reason")})
        emit({"referred_back": True})
        return 0

    if tool == "record_change":
        append(CHANGES, {"document_id": state.get("document_id"),
                         "policy_id": state.get("policy_id"),
                         "doc_kind": state.get("doc_kind"),
                         "effective_from": state.get("effective_from"),
                         "received_at": state.get("received_at"),
                         "restates": state.get("restates")})
        emit({"change_recorded": True})
        return 0

    sys.stderr.write("unknown tool: %r\n" % tool)
    return 1


# -- the driver -------------------------------------------------------------

def open_db(actor=DESK):
    import areev
    os.makedirs(OUT, exist_ok=True)
    return areev.Areev(DB, ns=NS, actor=actor)


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


def schedule():
    with open(os.path.join(FIXTURES, "policies.json"), encoding="utf-8") as fh:
        return json.load(fh)


def seed():
    db = open_db()
    sched = schedule()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    extract = tool_def("extract", "read the document and place it on the world clock")
    assess = tool_def("assess_cover", "the cover in force on the date of loss")
    accum = tool_def("accumulation", "aggregate exposure across the insured's policies")
    review = tool_def("underwriter_review", "an underwriter determines cover, in writing",
                      executor_kind="client")
    issue = tool_def("issue_determination", "issue the coverage determination")
    refer = tool_def("refer_back", "return the document to its sender, unactioned")
    record = tool_def("record_change", "book the coverage change against the policy")

    wf = db.add("workflow", json.dumps({
        "name": "policy-servicing",
        "nodes": ["extract", "assess_cover", "accumulation", "underwriter_review",
                  "issue_determination", "refer_back", "record_change"],
        # `route` is emitted by `extract` and is mutually exclusive by
        # construction, so no edge depends on evaluation order.
        "edges": [
            {"src": "extract", "dst": "refer_back", "cond": 'route == "referral"'},
            {"src": "extract", "dst": "assess_cover", "cond": 'route == "claim"'},
            {"src": "extract", "dst": "record_change", "cond": 'route == "change"'},
            {"src": "assess_cover", "dst": "accumulation"},
            {"src": "accumulation", "dst": "issue_determination", "cond": "settled == true"},
            {"src": "accumulation", "dst": "underwriter_review", "cond": "settled != true"},
            {"src": "underwriter_review", "dst": "issue_determination"},
        ],
        "bindings": {"extract": extract, "assess_cover": assess,
                     "accumulation": accum, "underwriter_review": review,
                     "issue_determination": issue, "refer_back": refer,
                     "record_change": record},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "coverage-judgment",
        "description": "how this desk reads a policy across two clocks",
        "instructions": "A loss is assessed against the cover in force on the "
                        "date of loss, never against the cover the file holds "
                        "today. An endorsement cannot reach a loss that "
                        "predates its own effective date. A document with no "
                        "effective date belongs on neither clock and is "
                        "refused, not guessed. What we knew on a date is a "
                        "separate question from what was true on it, and the "
                        "insured is entitled to an answer on both.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # -- the schedule as issued, on both clocks -----------------------------
    # valid_from = when cover attached (world). created_at = when the desk
    # keyed it (knowledge; the store copies created_at into system_valid_from).
    for row in sched["as_issued"]:
        for relation, obj in (("mg:coverage_limit", row["coverage_limit"]),
                              ("mg:deductible", row["deductible"])):
            db.add("fact", json.dumps({
                "subject": row["policy"], "relation": relation, "object": obj,
                "valid_from": ms(row["inception"]),
                "created_at": ms(row["booked_on"]),
            }), ns=POLICY_NS)
        # The entity graph. `mg:owned_by` and `part_of` are entity-valued in
        # the store's default vocabulary, so they get reverse-index rows and
        # `related(direction="in")` can walk them. `mg:covers_peril` is not --
        # deliberately, so the example can show the difference honestly.
        db.add_fact(row["policy"], "mg:owned_by", row["insured"],
                    ns=POLICY_NS, idempotent=True)
        for peril in row["perils"]:
            db.add_fact(row["policy"], "mg:covers_peril", peril,
                        ns=POLICY_NS, idempotent=True)
    for g in sched["groups"]:
        db.add_fact(g["insured"], "part_of", g["group"], ns=POLICY_NS, idempotent=True)

    db.add_fact("accumulation", "mg:limit", sched["accumulation_limit"],
                ns=DESK_NS, idempotent=True)

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE cover_line AS '
           '"- {{subject}} {{relation}} {{object}}"')
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, coverage, rulings" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'judgment: (RECALL skills LIMIT 2), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'cover: (RECALL facts WHERE namespace = "org.uw.policies" LIMIT 40), '
           'rulings: (RECALL facts WHERE namespace = "org.uw.wordings" LIMIT 20) '
           'BUDGET 3000 tokens FORMAT markdown }')

    emit({"workflow": wf, "policies": len(sched["as_issued"]),
          "accumulation_limit": sched["accumulation_limit"]})
    return 0


# -- the two clocks ---------------------------------------------------------

def _grain(raw):
    if not raw:
        return {}
    doc = json.loads(raw)
    if isinstance(doc, dict) and doc.get("found") is False:
        return {}
    g = doc.get("grain", doc) if isinstance(doc, dict) else {}
    fields = g.get("fields") or {}
    return {"object": fields.get("object"), "hash": g.get("hash"),
            "valid_from": fields.get("valid_from"),
            "valid_to": fields.get("valid_to"),
            "created_at": fields.get("created_at")}


def cover_at(db, policy, relation, at_ms, axis):
    return _grain(db.entity_at(policy, relation, at_ms, axis=axis, ns=POLICY_NS))


def head_of(db, policy, relation):
    return _grain(db.latest(policy, relation, ns=POLICY_NS))


def as_of(argv):
    """Both clocks, side by side, at the dates the acts care about."""
    policy = argv[0]
    relation = argv[1] if len(argv) > 1 else "mg:coverage_limit"
    dates = argv[2:] or [TODAY]
    db = open_db()
    rows = []
    for date in dates:
        at = ms(date)
        rows.append({
            "at": date,
            "world": cover_at(db, policy, relation, at, "world"),
            "knowledge": cover_at(db, policy, relation, at, "knowledge"),
        })
    emit({"policy": policy, "relation": relation, "rows": rows,
          "head": head_of(db, policy, relation),
          "history": json.loads(db.history(policy, relation, ns=POLICY_NS))})
    return 0


def exposure_of(db, policy, at_ms):
    """The accumulation walk. `related` is a bounded k-hop walk over the
    entity graph; the aggregate itself is a world-axis read per policy, so
    a cancelled policy drops out of the aggregate on its cancellation date
    without anyone deleting anything."""
    insured = head_of(db, policy, "mg:owned_by").get("object")
    own, group = [], []
    if insured:
        own = [t for t in json.loads(db.related(
            insured, "mg:owned_by", direction="in", depth=1, limit=64,
            ns=POLICY_NS))["reached"] if t.startswith("POL-")]
    reach = json.loads(db.related(policy, GRAPH_RELATIONS, direction="both",
                                  depth=4, limit=64, ns=POLICY_NS))["reached"]
    group = [t for t in reach if t.startswith("POL-") and t not in own and t != policy]
    if policy not in own:
        own.append(policy)
    total = 0
    in_force = []
    for p in sorted(own):
        limit = cover_at(db, p, "mg:coverage_limit", at_ms, "world").get("object")
        if limit:
            total += int(limit)
            in_force.append({"policy": p, "limit_in_force": limit})
    cap = json.loads(db.latest("accumulation", "mg:limit", ns=DESK_NS) or "null")
    return {
        "insured": insured,
        "own_policies": in_force,
        "group_policies": sorted(group),
        "aggregate_exposure": str(total),
        "accumulation_limit": (cap or {}).get("fields", {}).get("object"),
        "walk": {
            # Reported so the README's honesty about directions is checkable.
            "out_from_policy": json.loads(db.related(
                policy, GRAPH_RELATIONS, direction="out", depth=4, limit=64,
                ns=POLICY_NS))["reached"],
            "in_from_policy": json.loads(db.related(
                policy, GRAPH_RELATIONS, direction="in", depth=4, limit=64,
                ns=POLICY_NS))["reached"],
            "both_from_policy": reach,
            "in_on_non_entity_relation": json.loads(db.related(
                "flood", "mg:covers_peril", direction="in", depth=1, limit=64,
                ns=POLICY_NS))["reached"],
            "out_on_non_entity_relation": json.loads(db.related(
                policy, "mg:covers_peril", direction="out", depth=1, limit=64,
                ns=POLICY_NS))["reached"],
        },
    }


def exposure(argv):
    db = open_db()
    at = ms(argv[1]) if len(argv) > 1 else ms(TODAY)
    emit(exposure_of(db, argv[0], at))
    return 0


# -- intake -----------------------------------------------------------------

def queue():
    return sorted(n for n in os.listdir(INBOUND)
                  if n.endswith(".json") and n[:2] <= DOC_UPTO)


def settled_reading(db, clause):
    if not clause:
        return None
    reading = _grain(db.latest(clause, "mg:wording_reading", ns=WORDING_NS))
    if not reading.get("object"):
        return None
    by = _grain(db.latest(clause, "mg:settled_by", ns=WORDING_NS))
    return {"clause": clause, "reading": reading["object"],
            "settled_by": by.get("object")}


def desk_rules(db, source):
    rule = _grain(db.latest("source:%s" % source, "mg:on_missing_effective_date",
                            ns=DESK_NS))
    return {"on_missing_effective_date": rule.get("object") or "fail"}


def run_input(db, doc):
    payload = {"item": doc, "desk_rules": desk_rules(db, doc.get("source", ""))}
    if doc.get("doc_kind") != "claim_notice":
        return payload
    at = ms(doc["date_of_loss"])
    policy = doc["policy_id"]
    payload["as_of"] = {
        # THE read this example exists for. `world` is what was in force when
        # the loss happened; `head` is what the file says today; `knowledge`
        # is what the desk knew when the notice arrived.
        "world": cover_at(db, policy, "mg:coverage_limit", at, "world"),
        "knowledge": cover_at(db, policy, "mg:coverage_limit",
                              ms(doc["received_at"]), "knowledge"),
        "head": head_of(db, policy, "mg:coverage_limit"),
        "deductible": cover_at(db, policy, "mg:deductible", at, "world"),
    }
    payload["exposure"] = exposure_of(db, policy, at)
    payload["settled_reading"] = settled_reading(db, doc.get("clause"))
    return payload


def apply_change(db, doc):
    """Book an accepted coverage document onto the two clocks.

    Three shapes, and the difference between them is the example:

    * a **variation** (endorsement, cancellation) changes what is TRUE from
      its effective date. The open window is superseded by a closed
      restatement of itself, and a new grain opens at the effective date.
      The old value stays live and world-readable, so a loss before that date
      still finds it.
    * a **restatement** (`"restates": true`) says the desk had it wrong all
      along. That is a change in KNOWLEDGE, not in the world: the grain is
      superseded with the corrected value over the SAME world window, and the
      knowledge axis keeps the old belief reachable at the dates the desk
      held it.
    * a **cancellation** closes the window and opens nothing.
    """
    policy = doc["policy_id"]
    eff = ms(doc["effective_from"])
    recv = ms(doc["received_at"])
    kind = doc["doc_kind"]
    relations = ([doc["change"]["relation"]] if "change" in doc
                 else ["mg:coverage_limit", "mg:deductible"])
    written = []
    for relation in relations:
        head = head_of(db, policy, relation)
        if not head.get("hash"):
            continue
        if doc.get("restates"):
            written.append(db.supersede(head["hash"], "fact", json.dumps({
                "subject": policy, "relation": relation,
                "object": doc["change"]["object"],
                "valid_from": head.get("valid_from"),
                "valid_to": head.get("valid_to"),
                "created_at": recv,   # a NEW belief, held from today
            }), ns=POLICY_NS))
            continue
        if head.get("valid_to") is None or head["valid_to"] > eff:
            # Close the open window. The restatement keeps the ORIGINAL
            # created_at: the desk is not changing its mind about the old
            # value, only saying where it stopped applying.
            written.append(db.supersede(head["hash"], "fact", json.dumps({
                "subject": policy, "relation": relation,
                "object": head["object"],
                "valid_from": head.get("valid_from"),
                "valid_to": eff,
                "created_at": head.get("created_at"),
            }), ns=POLICY_NS))
        if kind != "cancellation":
            written.append(db.add("fact", json.dumps({
                "subject": policy, "relation": relation,
                "object": doc["change"]["object"],
                "valid_from": eff,
                "created_at": recv,   # BACKDATED when recv > eff
            }), ns=POLICY_NS))
    return written


def intake():
    db = open_db()
    report = {"started": 0, "completed": 0, "parked": 0, "failed": 0,
              "referred": 0, "documents": []}
    for name in queue():
        with open(os.path.join(INBOUND, name), encoding="utf-8") as fh:
            doc = json.load(fh)
        doc_id = doc["document_id"]
        # Dedup lives in the memory, not in a file beside it.
        if _grain(db.latest("doc:%s" % doc_id, "mg:intake_state", ns=NS)).get("object"):
            continue
        run_id = "doc-%s" % doc_id
        state = "failed"
        try:
            session = json.loads(db.run_start(
                workflow=plan_hash(db), run_id=run_id,
                input_json=json.dumps(run_input(db, doc)),
                tool_cmd=self_cmd("tools"),
                max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600))
            report["started"] += 1
            # A host tool that exits non-zero does not raise here: the run
            # reaches a terminal state and the session says which one. Only a
            # run that never started (a bad plan, a refused executor) raises.
            finished = session.get("finished") or ""
            if session.get("parked"):
                state = "parked"
                report["parked"] += 1
            elif not finished.startswith("Completed"):
                state = "failed"
                report["failed"] += 1
                sys.stderr.write("%s: %s\n" % (doc_id, finished))
            elif doc.get("doc_kind") == "claim_notice":
                state = "completed"
                report["completed"] += 1
            elif doc.get("effective_from"):
                state = "completed"
                report["completed"] += 1
                apply_change(db, doc)
            else:
                # Completed without an effective date means it took the
                # refer-back route, which only exists once a person has
                # signed the standing rule that creates it.
                state = "referred"
                report["referred"] += 1
        except ValueError as e:
            report["failed"] += 1
            sys.stderr.write("%s: %s\n" % (doc_id, e))
        db.add_fact("doc:%s" % doc_id, "mg:intake_state", state, ns=NS, idempotent=True)
        report["documents"].append({"document_id": doc_id, "state": state})
    emit(report)
    return 0


def plan_hash(db):
    plans = json.loads(db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
    return next(g["hash"] for g in plans
                if g["fields"].get("name") == "policy-servicing")


# -- the underwriter --------------------------------------------------------

def pending_asks(db):
    out = []
    for run_id in json.loads(db.run_list(200)):
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
        rows.append({"run_id": run_id, "ask": ask_id,
                     "claim_id": state.get("claim_id"),
                     "policy_id": state.get("policy_id"),
                     "date_of_loss": state.get("date_of_loss"),
                     "limit_in_force": state.get("limit_in_force"),
                     "limit_now": state.get("limit_now"),
                     "known_at_notice": state.get("known_at_notice"),
                     "claim_amount": state.get("claim_amount"),
                     "uninsured_excess": state.get("uninsured_excess"),
                     "accumulation_flag": state.get("accumulation_flag"),
                     "aggregate_exposure": state.get("aggregate_exposure")})
    emit(rows)
    return 0


def determine(path):
    """An underwriter's coverage determination, read from a fixture the way a
    claims platform would deliver it."""
    with open(path, encoding="utf-8") as fh:
        note = json.load(fh)
    principal = note.get("underwriter", "user:unknown")
    verdict = note.get("decision")
    because = (note.get("because") or "").strip()
    if verdict not in ("cover_confirmed", "cover_declined"):
        sys.stderr.write("a determination is confirmed or declined\n")
        return 3
    if not because:
        # A coverage determination is a document the insured may see. An
        # unreasoned one is not a determination, it is an outcome.
        sys.stderr.write("a coverage determination with no written reason "
                         "is refused\n")
        return 3

    db = open_db(actor=principal)
    for run_id, ask_id, state in pending_asks(db):
        if state.get("claim_id") != note.get("claim_id"):
            continue
        result = {"decision": verdict, "responder": principal, "because": because}
        if note.get("limit_applied"):
            result["limit_applied"] = note["limit_applied"]
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        # The reading an underwriter settled is the lesson worth keeping: it
        # carries their signature forward to the next claim on that clause.
        settles = note.get("settles_wording")
        if settles:
            db.add_fact(settles["clause"], "mg:wording_reading", settles["reading"],
                        ns=WORDING_NS, idempotent=True)
            db.add_fact(settles["clause"], "mg:settled_by", principal,
                        ns=WORDING_NS, idempotent=True)
            db.add_fact(settles["clause"], "mg:settled_on", note.get("claim_id", "?"),
                        ns=WORDING_NS, idempotent=True)
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        emit({"run_id": run_id, "decision": verdict, "determined_by": principal,
              "settled_wording": bool(settles), "outcome": outcome})
        return 0
    sys.stderr.write("no parked run for claim %s\n" % note.get("claim_id"))
    return 5


def trace(claim_id):
    """What the determination was actually made against -- read back out of
    the run journal, not out of today's file."""
    db = open_db()
    run_id = "doc-%s" % claim_id
    journal = json.loads(db.run_trace(run_id))

    def find(node, key):
        if isinstance(node, dict):
            if key in node and isinstance(node[key], dict):
                return node[key]
            for v in node.values():
                hit = find(v, key)
                if hit:
                    return hit
        elif isinstance(node, list):
            for v in node:
                hit = find(v, key)
                if hit:
                    return hit
        return None

    emit({"claim_id": claim_id, "run_id": run_id,
          "as_of_pinned_into_the_run": find(journal, "as_of") or {},
          "exposure_pinned_into_the_run": find(journal, "exposure") or {},
          "journal_entries": len(journal.get("trace") or [])})
    return 0


# -- desk rules, the loop, and the people who sign things -------------------

def flagged(argv, flag):
    it = iter(argv)
    for a in it:
        if a == flag:
            return next(it, None)
    return None


def desk_rule(argv):
    """A standing intake rule, signed. Refuses without a written reason."""
    if len(argv) < 2:
        sys.stderr.write("usage: desk-rule SOURCE ACTION --because ... --as user:X "
                         "[--after REC]\n")
        return 2
    source, action = argv[0], argv[1]
    because = flagged(argv[2:], "--because")
    actor = flagged(argv[2:], "--as")
    after = flagged(argv[2:], "--after")
    if not because:
        sys.stderr.write("a standing rule with no written reason is refused\n")
        return 2
    db = open_db(actor=actor or "user:anonymous")
    subject = "source:%s" % source
    db.add_fact(subject, "mg:on_missing_effective_date", action, ns=DESK_NS)
    db.add_fact(subject, "mg:rule_reason", because, ns=DESK_NS)
    db.add_fact(subject, "mg:rule_signed_by", actor or "user:anonymous", ns=DESK_NS)
    if after:
        db.add_fact(subject, "mg:rule_from_finding", after, ns=DESK_NS)
    emit({"source": source, "action": action, "signed_by": actor, "because": because})
    return 0


def improve():
    db = open_db()
    # Tune the analyzers to this desk -- recorded acts of configuration, not
    # a fork. `cold_grains` is switched OFF here for a reason that is specific
    # to a bi-temporal memory: this desk reads its coverage picture through
    # `entity_at`, which is an as-of read rather than a recall, so every
    # coverage grain looks "never recalled" and the analyzer proposes retiring
    # the file's entire history. `staleness` stays ON deliberately -- see
    # improve.sh, where a person has to tell it no.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_runs": 3, "min_failure_ratio": 0.3}))
    db.set_analyzer_config("loop.cold_grains/1", False, None)
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
    because = flagged(argv[2:], "--because")
    actor = flagged(argv[2:], "--as")
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
    print(db.cal('RECALL facts WHERE namespace = "org.uw.wordings" LIMIT 20 '
                 'FORMAT TEMPLATE cover_line'))
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal('RECALL observations WHERE namespace = "agent:harness" '
                            'RECENT 400 FORMAT json'))
    outcome = {}
    for g in obs.get("grains") or []:
        fields = g.get("fields") or {}
        if fields.get("observation_kind") == "run_outcome":
            outcome[fields.get("run_id")] = fields.get("object")
    emit([{"run_id": r, "outcome": outcome.get(r, "open")}
          for r in json.loads(db.run_list(200))])
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "tools":
        return tool_main()
    if cmd == "seed":
        return seed()
    if cmd == "intake":
        return intake()
    if cmd == "as-of":
        return as_of(sys.argv[2:])
    if cmd == "exposure":
        return exposure(sys.argv[2:])
    if cmd == "asks":
        return asks()
    if cmd == "determine":
        return determine(sys.argv[2])
    if cmd == "trace":
        return trace(sys.argv[2])
    if cmd == "desk-rule":
        return desk_rule(sys.argv[2:])
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
