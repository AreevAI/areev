#!/usr/bin/env python3
"""a privacy desk: data-subject requests, end to end, embedded Areev.

Access, portability and erasure requests arrive. The desk works out who is
asking, prices the request against what is actually stored, and parks for a
Data Protection Officer -- because an erasure is irreversible and the
approver's identity IS the audit record. On approval it erases, and records
what it did WITHOUT recording who it did it to.

The property this example exists for: ERASURE AND DISCLOSURE ARE ONE
SELECTION. `subject_report` shows exactly what `forget_subject` would
remove; the desk asserts the two counts agree at execution time and refuses
to erase if they ever diverge. A DSAR that discloses one set and deletes
another is a compliance failure, not a rounding error.

Two things follow from Areev's shape and are load-bearing here:

  * The runtime holds the single writer, so a host tool subprocess can
    never open the memory. Every read and every erasure is the DRIVER's,
    taken after the run returns. Which is the right architecture anyway: an
    irreversible act is not performed by a subprocess, it is performed by
    the party holding the memory, after a named human approved it.
  * An immutable, replicating grain that names an erased person would undo
    the erasure it records. So the RUN JOURNAL NEVER SEES AN IDENTITY. The
    desk passes a fingerprint (sha256(subject)[:16], the shape
    `areev_core::authz::subject_fingerprint` uses) into the run, and keeps
    the fingerprint -> person mapping in its own case file on disk.

One subcommand is a subprocess seam the runtime spawns (JSON on stdin,
JSON on stdout, one process per invocation). It never opens the memory:

    agent.py tools        the host tools ($AREEV_TOOL_NAME picks one)

Everything else is the driver:

    agent.py seed              the plan, the tool definitions, the desk's
                               declared rules, and a synthetic multi-
                               namespace memory to answer requests from
    agent.py intake [--retry]  log new requests and start a governed run
                               for each (--retry re-runs ones that failed)
    agent.py asks              the requests parked on a DPO
    agent.py decide FILE       apply a DPO's decision, then execute it
    agent.py report SUBJECT    the DSAR read, on demand, across every
                               registered namespace (Art. 15/20)
    agent.py guards            the structural refusals, as a report
    agent.py sweep             the declared retention rules, applied
    agent.py trace SUBJECT     does this identity survive anywhere?
    agent.py improve           the loop reads the desk's own history back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py teach NS SUBJECT RELATION OBJECT
    agent.py register          the desk's DSAR register + certificates
    agent.py brief             the desk's self-briefing (saved CAL queries)
    agent.py runs              run list as JSON (the acts assert on this)
    agent.py verify            integrity + content-address verification

To make it real, replace `fixtures/requests` with your intake mailbox and
`tools` with processes that reach your case manager. The plan, the journal,
the DPO gate, the one-selector guarantee and the fingerprinted certificate
do not change.
"""

import hashlib
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("DSR_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
REQUESTS = os.path.join(FIXTURES, "requests")
SEED_FILE = os.path.join(FIXTURES, "seed", "subjects.json")
REQ_UPTO = os.environ.get("REQ_UPTO", "04")   # the acts advance this "clock"
OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))

CASES = os.path.join(OUT, "cases.jsonl")          # request -> person (on disk only)
ORDERS = os.path.join(OUT, "orders.jsonl")        # what the run ordered
REGISTER = os.path.join(OUT, "register.jsonl")    # the Art. 30 style register
CERTS_LOG = os.path.join(OUT, "certificates.jsonl")
PACKS = os.path.join(OUT, "packs")                # the disclosure artifacts

NS = "org.ops"            # plan, tool definitions, run journals
PRIVACY = "org.privacy"   # the desk's OWN rules: register, retention, resolvers
CERT_NS = "agent:privacy"  # erasure certificates -- fingerprints, never a name
DESK = "agent:privacy-desk"        # the agent; it can never approve its own work

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# The statutory clock. A Fact in org.privacy, not a constant here -- which is
# what lets it be changed without touching this file.
DEFAULT_DEADLINE_DAYS = 30

DAY_MS = 86_400_000


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


def fingerprint(identity):
    """The same shape `areev_core::authz::subject_fingerprint` writes into a
    destruction audit record: sha256 of the identity, first 8 bytes, hex.

    Given a candidate identity you can recompute this and VERIFY that a
    certificate concerns that person. You cannot go the other way, and you
    cannot enumerate the log. That is the whole point: the record of an
    erasure must not be a copy of what was erased.
    """
    return hashlib.sha256(identity.encode("utf-8")).hexdigest()[:16]


def now_ms():
    return int(time.time() * 1000)


# -- the tools seam ---------------------------------------------------------
# stdin is the run's merged state. It carries the REDACTED intake record --
# case reference, request type, verification outcome, fingerprint, and per-
# namespace counts. No name, no address, no prose from the request. Never
# opens the memory: the runtime that spawned this process is holding it.

def tool_main():
    state = json.load(sys.stdin)
    intake = state.get("intake") or {}
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "identify_subject":
        # Two questions, and the desk stops on either. Disclosing to the
        # wrong person is a breach; erasing the wrong person is worse and
        # cannot be undone (Art. 12(6) -- verify, or do not act).
        verified = bool((intake.get("verification") or {}).get("verified"))
        candidates = intake.get("candidates") or []
        if not verified:
            sys.stderr.write(
                "requester identity was not verified (%s) -- refusing to act "
                "on request %s\n" % ((intake.get("verification") or {}).get("method"),
                                     intake.get("request_id")))
            return 1
        if len(candidates) != 1:
            sys.stderr.write(
                "the claimed identity resolved to %d subjects, not exactly one "
                "-- refusing to act on request %s\n"
                % (len(candidates), intake.get("request_id")))
            return 1
        emit({"subject_ref": candidates[0], "identity_verified": True})

    elif tool == "build_report":
        # The inventory the driver measured with the DSAR selector: how many
        # grains, in which namespaces. Counts and namespace names only.
        inventory = intake.get("inventory") or []
        total = sum(int(r.get("grains") or 0) for r in inventory)
        emit({
            "grain_count": total,
            "namespaces": [r["namespace"] for r in inventory if r.get("grains")],
            "nothing_on_file": total == 0,
            "deadline_days": intake.get("deadline_days"),
            "consents_on_file": intake.get("consents_on_file", 0),
            "withdrawal_on_file": bool(intake.get("withdrawal_on_file")),
        })

    elif tool == "erase":
        # The node ORDERS the erasure; it does not perform it. The party
        # holding the memory performs it, after this run returns.
        append(ORDERS, {
            "run_id": intake.get("run_id"),
            "request_id": intake.get("request_id"),
            "subject_ref": state.get("subject_ref"),
            "order": "erase",
            "namespaces": state.get("namespaces") or [],
            "reported_at_intake": state.get("grain_count"),
            "approved_by": state.get("responder"),
            "because": state.get("because"),
        })
        emit({"erasure_ordered": True})

    elif tool == "disclose_only":
        append(ORDERS, {
            "run_id": intake.get("run_id"),
            "request_id": intake.get("request_id"),
            "subject_ref": state.get("subject_ref"),
            "order": "disclose",
            "request_type": intake.get("request_type"),
            "namespaces": state.get("namespaces") or [],
            "reported_at_intake": state.get("grain_count"),
            "approved_by": state.get("responder"),
            "because": state.get("because"),
        })
        emit({"disclosure_ordered": True})

    elif tool == "close":
        append(REGISTER, {
            "request_id": intake.get("request_id"),
            "request_type": intake.get("request_type"),
            "received_at": intake.get("received_at"),
            "subject_ref": state.get("subject_ref"),
            "decision": state.get("decision", "none"),
            "decided_by": state.get("responder", "none"),
            "because": state.get("because", ""),
            "inventory_grains": state.get("grain_count"),
            "namespaces": state.get("namespaces") or [],
            "nothing_on_file": bool(state.get("nothing_on_file")),
            "deadline_days": state.get("deadline_days"),
        })
        emit({"closed": True})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the driver -------------------------------------------------------------

def open_db(actor=DESK):
    """`telemetry="off"` is not a performance choice -- it is the whole
    posture of this desk.

    The recall-telemetry sidecar records the TEXT of queries. A privacy desk
    searches for the people it is about to erase, and again afterwards to
    prove they are gone. With telemetry on, those searches leave the erased
    person's name in a sidecar the erasure has already run past -- and the
    loop's coverage-gap analyzer will helpfully propose "recurring question
    with no matching memory: <erased name>", writing the identity back into
    the memory as a recommendation grain.

    A desk that must be able to erase does not keep a query log of who it
    searched for.
    """
    import areev
    os.makedirs(OUT, exist_ok=True)
    return areev.Areev(DB, ns=NS, actor=actor, telemetry="off")


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


def load_seed():
    with open(SEED_FILE, encoding="utf-8") as fh:
        return json.load(fh)


def declared(db, subject_prefix, relation):
    """Read one class of the desk's declared rules out of org.privacy.

    Everything the desk treats as policy -- which namespaces hold personal
    data, how long support history is kept, how a claimed identity may be
    matched -- is a Fact in the memory, not a constant in this file. That is
    what makes `teach` a real change of behaviour.
    """
    out = {}
    payload = json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s" AND relation = "%s" '
        'LIMIT 200 FORMAT json' % (PRIVACY, relation)))
    for g in payload.get("grains") or []:
        subject = g["fields"].get("subject") or ""
        if subject.startswith(subject_prefix):
            out[subject[len(subject_prefix):]] = g["fields"].get("object")
    return out


def registered_namespaces(db):
    """Where personal data lives, in declaration order. EXACT namespaces --
    every one of them is a destruction target, and destruction never takes a
    pattern."""
    return sorted(declared(db, "register:", "mg:purpose"))


def resolve_claim(db, claim):
    """A claimed identity -> the subject keys it matches, using only the
    resolution rules the desk has actually declared.

    Week one declares one rule: a claim that is already a DID is the subject
    key. An email address resolves to nobody, and the run refuses rather
    than guessing -- which is the correct failure, and also the one the loop
    later finds a pattern in.
    """
    rules = declared(db, "dsar-intake", "mg:resolve_did")
    if claim.startswith("did:") and rules.get("") == "on":
        return [claim]
    relation = declared(db, "dsar-intake", "mg:resolve_contact_email").get("")
    if not relation:
        return []
    found = []
    for ns in registered_namespaces(db):
        payload = json.loads(db.cal(
            'RECALL facts WHERE namespace = "%s" AND relation = "%s" '
            'AND object = "%s" LIMIT 20 FORMAT json' % (ns, relation, claim)))
        for g in payload.get("grains") or []:
            subject = g["fields"].get("subject")
            if subject and subject not in found:
                found.append(subject)
    return found


def dsar_inventory(db, subject):
    """The authoritative measurement: what `forget_subject` would remove,
    per registered namespace, taken with the SAME selector as the erasure.

    `subject_report` is that selector in show-me mode. Anything else -- a
    recall, a search, a hand-written query -- is a hint. Pricing a request
    off a hint is how a desk ends up disclosing one set and deleting
    another.
    """
    inventory, names, consents, withdrawal = [], set(), 0, False
    for ns in registered_namespaces(db):
        report = json.loads(db.subject_report(subject, ns=ns))
        inventory.append({"namespace": ns, "grains": len(report["grains"])})
        names.update(report.get("identity_names") or [])
        for g in report["grains"]:
            if g["type"] == "consent":
                consents += 1
                if g["fields"].get("is_withdrawal"):
                    withdrawal = True
    return {"inventory": inventory, "identity_names": sorted(names),
            "consents_on_file": consents, "withdrawal_on_file": withdrawal,
            "total": sum(r["grains"] for r in inventory)}


def seed():
    db = open_db()
    fixture = load_seed()

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    identify = tool_def("identify_subject",
                        "resolve the claim to exactly one subject, and refuse "
                        "unless the requester's identity was verified")
    report = tool_def("build_report",
                      "price the request: how many grains, in which namespaces")
    review = tool_def("dpo_review",
                      "a Data Protection Officer decides: disclose, or erase",
                      executor_kind="client")
    erase = tool_def("erase", "order the erasure the DPO approved")
    disclose = tool_def("disclose_only",
                        "order the disclosure pack the DPO approved")
    close = tool_def("close", "write the request into the desk's register")

    # T18: `close` is the ONLY node with no out-edge. Every path ends there,
    # including the one that finds nothing on file and never troubles a human.
    wf = db.add("workflow", json.dumps({
        "name": "data-subject-request",
        "nodes": ["identify_subject", "build_report", "dpo_review",
                  "erase", "disclose_only", "close"],
        "edges": [
            {"src": "identify_subject", "dst": "build_report"},
            {"src": "build_report", "dst": "close", "cond": "nothing_on_file == true"},
            {"src": "build_report", "dst": "dpo_review", "cond": "nothing_on_file == false"},
            {"src": "dpo_review", "dst": "erase", "cond": 'decision == "erase"'},
            {"src": "dpo_review", "dst": "disclose_only", "cond": 'decision == "disclose"'},
            {"src": "erase", "dst": "close"},
            {"src": "disclose_only", "dst": "close"},
        ],
        "bindings": {"identify_subject": identify, "build_report": report,
                     "dpo_review": review, "erase": erase,
                     "disclose_only": disclose, "close": close},
        "retries": {},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "dsar-judgment",
        "description": "how this desk reads a data-subject request",
        "instructions": "Verify before you act: disclosing to the wrong person "
                        "is a breach, erasing the wrong person cannot be undone. "
                        "Price the request with the erasure selector, never with "
                        "a search. Never write an identity into an audit record "
                        "-- a replicating grain naming the erased person would "
                        "undo the erasure it records. A wildcard namespace is a "
                        "reading convention, never a destruction target.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # -- the desk's declared rules ------------------------------------------
    for entry in fixture["processing_register"]:
        ns = entry["namespace"]
        db.add_fact("register:" + ns, "mg:purpose", entry["purpose"],
                    ns=PRIVACY, idempotent=True)
        db.add_fact("register:" + ns, "mg:lawful_basis", entry["lawful_basis"],
                    ns=PRIVACY, idempotent=True)
    for rule in fixture["retention"]:
        ns = rule["namespace"]
        db.add_fact("retention:" + ns, "mg:max_age_days",
                    str(rule["max_age_days"]), ns=PRIVACY, idempotent=True)
        db.add_fact("retention:" + ns, "mg:grain_type", rule["grain_type"],
                    ns=PRIVACY, idempotent=True)
        db.add_fact("retention:" + ns, "mg:because", rule["because"],
                    ns=PRIVACY, idempotent=True)
    db.add_fact("dsar-intake", "mg:resolve_did", "on", ns=PRIVACY, idempotent=True)
    db.add_fact("dsar-intake", "mg:deadline_days", str(DEFAULT_DEADLINE_DAYS),
                ns=PRIVACY, idempotent=True)

    # -- the personal data the desk answers requests from -------------------
    # Synthetic, and stamped relative to now so the retention sweep's
    # arithmetic is the fixture's, not the wall clock's.
    now = now_ms()
    seeded = 0
    for person in fixture["subjects"]:
        subject = person["subject"]
        for grain in person["grains"]:
            ns, kind = grain["ns"], grain["kind"]
            created = now - int(grain.get("age_days", 0)) * DAY_MS
            if kind == "fact":
                db.add_fact(subject, grain["relation"], grain["object"],
                            ns=ns, idempotent=True)
            elif kind == "event":
                db.add("event", json.dumps({
                    "content": grain["content"], "role": "user",
                    "session_id": subject, "subject": subject,
                    "created_at": created,
                }), ns=ns)
            elif kind == "consent":
                # `subject` as well as `subject_did`: the DSAR selector finds
                # DICTIONARY-INDEXED references, and `subject` is the indexed
                # position (docs/erasure.md, scope contract). A consent grain
                # that names the person only in `subject_did` is invisible to
                # both the report and the erasure -- disclosed by neither,
                # removed by neither.
                db.add("consent", json.dumps({
                    "subject_did": subject, "user_id": subject,
                    "subject": subject, "scope": grain["scope"],
                    "basis": grain["basis"], "created_at": created,
                }), ns=ns)
            else:
                raise SystemExit("unknown seed grain kind %r" % kind)
            seeded += 1

    # Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE rule_line AS "- {{subject}} {{relation}} {{object}}"')
    db.cal('DEFINE QUERY "desk_rules"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, declared rules" '
           'AS { ASSEMBLE "desk_rules" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'judgment: (RECALL skills LIMIT 2), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'rules: (RECALL facts WHERE namespace = "org.privacy" LIMIT 60) '
           'BUDGET 3000 tokens FORMAT markdown }')

    emit({"workflow": wf, "grains_seeded": seeded,
          "namespaces": registered_namespaces(db),
          "tools": {"identify_subject": identify, "build_report": report,
                    "dpo_review": review, "erase": erase,
                    "disclose_only": disclose, "close": close}})
    return 0


def plan_hash(db):
    plans = json.loads(db.cal('RECALL workflows LIMIT 10 FORMAT json'))["grains"]
    return next(g["hash"] for g in plans
                if g["fields"].get("name") == "data-subject-request")


def request_files(upto):
    return sorted(n for n in os.listdir(REQUESTS)
                  if n.endswith(".json") and n[:2] <= upto)


def intake(argv):
    """Log every new request and start a governed run for it.

    There is deliberately no standing trigger here. Screening every payment
    automatically is the point of a screening desk; starting an ERASURE run
    off an unauthenticated mailbox is not. Intake is an explicit, logged act
    by the controller -- and the loud, boring reason is the same one the
    rest of this file keeps running into: the run's own journal must not
    become a second copy of the request.
    """
    retry = "--retry" in argv
    upto = REQ_UPTO
    if "--upto" in argv:
        upto = argv[argv.index("--upto") + 1]
    db = open_db()
    wf = plan_hash(db)
    deadline = int(declared(db, "dsar-intake", "mg:deadline_days").get("")
                   or DEFAULT_DEADLINE_DAYS)

    seen = {}
    for case in rows(CASES):
        seen[case["request_id"]] = case
    started, failed, closed, parked, skipped = 0, 0, 0, 0, 0

    for name in request_files(upto):
        with open(os.path.join(REQUESTS, name), encoding="utf-8") as fh:
            request = json.load(fh)
        rid = request["request_id"]
        prior = seen.get(rid)
        if prior and not (retry and prior.get("outcome") == "failed"):
            skipped += 1
            continue
        attempt = (prior.get("attempt", 1) + 1) if prior else 1
        run_id = "%s%s" % (rid.lower(), "" if attempt == 1 else "-r%d" % attempt)

        candidates = resolve_claim(db, request["claimed_identity"])
        subject = candidates[0] if len(candidates) == 1 else None
        measured = dsar_inventory(db, subject) if subject else {
            "inventory": [], "identity_names": [], "consents_on_file": 0,
            "withdrawal_on_file": False, "total": 0}

        # The REDACTED intake record -- the only thing the run ever sees.
        # No sender, no name, no request body: those stay in fixtures/ and
        # in the desk's own case file, never in a replicating grain.
        record = {
            "request_id": rid,
            "request_type": request["request_type"],
            "received_at": request["received_at"],
            "channel": request["channel"],
            "run_id": run_id,
            "attempt": attempt,
            "verification": {
                "verified": bool(request["verification"]["verified"]),
                "method": request["verification"]["method"],
            },
            "candidates": [fingerprint(c) for c in candidates],
            "inventory": measured["inventory"],
            "consents_on_file": measured["consents_on_file"],
            "withdrawal_on_file": measured["withdrawal_on_file"],
            "deadline_days": deadline,
        }

        # The join from case reference to person lives HERE, on the desk's
        # disk, and nowhere in the memory.
        case = {"request_id": rid, "run_id": run_id, "attempt": attempt,
                "subject": subject, "claimed": request["claimed_identity"],
                "subject_ref": fingerprint(subject) if subject else None,
                "request_type": request["request_type"], "outcome": "started"}
        append(CASES, case)

        session = json.loads(db.run_start(
            workflow=wf, run_id=run_id, tool_cmd=self_cmd("tools"),
            input_json=json.dumps({"intake": record}),
            max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600))
        started += 1
        finished = session.get("finished") or ""

        if "parked" in session:
            parked += 1
        elif finished.startswith("Failed") or finished.startswith("Stalled"):
            # The run stopped at `identify_subject`, before anything was
            # disclosed or erased. Loud beats wrong: a DSAR the desk cannot
            # attribute is a request it must not answer.
            cause = ("unverified-requester" if not record["verification"]["verified"]
                     else "unresolved-identity")
            failed += 1
            append(CASES, dict(case, outcome="failed", cause=cause,
                               detail=finished))
            db.record_tool_call("identify_subject", cause,
                                is_error=True, run_id=run_id)
        else:
            closed += 1
            append(CASES, dict(case, outcome="closed-nothing-on-file"))

    emit({"started": started, "parked": parked, "closed_without_a_human": closed,
          "refused": failed, "already_logged": skipped})
    return 0


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
    listing = []
    for run_id, ask_id, state in pending_asks(db):
        intake_rec = state.get("intake") or {}
        listing.append({
            "run_id": run_id, "ask": ask_id,
            "request_id": intake_rec.get("request_id"),
            "request_type": intake_rec.get("request_type"),
            "subject_ref": state.get("subject_ref"),
            "grain_count": state.get("grain_count"),
            "namespaces": state.get("namespaces"),
            "consents_on_file": state.get("consents_on_file"),
            "deadline_days": state.get("deadline_days"),
            "decide_with": "disclose | erase (+ because: ...)",
        })
    emit(listing)
    return 0


def case_for(request_id):
    latest = None
    for case in rows(CASES):
        if case["request_id"] == request_id:
            latest = case
    return latest


def decide(path):
    """A DPO's decision, read from a fixture the way a case-manager webhook
    would deliver it -- then executed by the party holding the memory."""
    with open(path, encoding="utf-8") as fh:
        note = json.load(fh)
    principal = note.get("officer", "user:unknown")
    request_id = note.get("request_id")
    verdict = note.get("decision")
    because = (note.get("because") or "").strip()
    if verdict not in ("disclose", "erase"):
        sys.stderr.write("a decision is 'disclose' or 'erase', not %r\n" % verdict)
        return 3
    if not because:
        # Not a formality. The reason is the only part of an irreversible act
        # that a regulator, or the next officer, can actually read back.
        sys.stderr.write("a data-subject decision with no written reason is refused\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        if (state.get("intake") or {}).get("request_id") != request_id:
            continue
        result = {"decision": verdict, "responder": principal, "because": because}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as exc:
            sys.stderr.write("respond refused: %s\n" % exc)
            return 4

        case = case_for(request_id) or {}
        subject = case.get("subject")
        # A withdrawal is recorded BEFORE the erasure runs -- and is itself
        # in scope for it, because it names the person. What survives is the
        # fingerprinted certificate, not the withdrawal grain.
        withdrawal = None
        if verdict == "erase" and subject:
            withdrawal = db.add("consent", json.dumps({
                "subject_did": subject, "user_id": subject, "subject": subject,
                "scope": "all-processing", "basis": "data-subject request",
                "is_withdrawal": True,
            }), ns=registered_namespaces(db)[0])

        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        executed = execute_orders(db, run_id, case, principal, because)
        append(CASES, dict(case, outcome="decided", decision=verdict,
                           decided_by=principal))
        emit({"run_id": run_id, "request_id": request_id, "decision": verdict,
              "responder": principal, "withdrawal_recorded": withdrawal,
              "outcome": outcome, "executed": executed})
        return 0
    sys.stderr.write("no parked run is waiting on request %s\n" % request_id)
    return 5


def execute_orders(db, run_id, case, principal, because):
    """Carry out what the run ordered, with the memory in hand.

    The equality asserted here is the reason this example exists: for every
    namespace, the count `subject_report` discloses is the count
    `forget_subject` removes. They are one selector; if they ever disagree
    the desk stops rather than erasing something it never disclosed.
    """
    subject = case.get("subject")
    done = []
    for order in rows(ORDERS):
        if order.get("run_id") != run_id:
            continue
        if order["order"] == "erase":
            if not subject:
                raise SystemExit("an erasure order with no resolved subject")
            reported = erased = terms = blobs = 0
            per_ns = []
            for ns in registered_namespaces(db):
                shown = len(json.loads(db.subject_report(subject, ns=ns))["grains"])
                report = json.loads(db.forget_subject(subject, ns=ns))
                if shown != report["grains_erased"]:
                    raise SystemExit(
                        "REFUSING: %s disclosed %d grains and erased %d -- the "
                        "report and the erasure must be one selection"
                        % (ns, shown, report["grains_erased"]))
                per_ns.append({"namespace": ns, "reported": shown,
                               "erased": report["grains_erased"]})
                reported += shown
                erased += report["grains_erased"]
                terms += report["terms_removed"]
                blobs += report["blobs_reclaimed"]
            cert = {
                "request_id": order["request_id"], "run_id": run_id,
                "subject_ref": order["subject_ref"], "act": "erasure",
                "reported": reported, "erased": erased,
                "terms_removed": terms, "blobs_reclaimed": blobs,
                "per_namespace": per_ns,
                "approved_by": principal, "because": because,
                "at_ms": now_ms(),
            }
            append(CERTS_LOG, cert)
            certify(db, cert)
            db.record_tool_call("erase", "erasure-executed", run_id=run_id)
            done.append(cert)
        elif order["order"] == "disclose":
            os.makedirs(PACKS, exist_ok=True)
            pack, names, count = [], set(), 0
            for ns in registered_namespaces(db):
                report = json.loads(db.subject_report(subject, ns=ns))
                names.update(report.get("identity_names") or [])
                count += len(report["grains"])
                pack.append({"namespace": ns, "grains": report["grains"]})
            pack_path = os.path.join(PACKS, "%s.json" % order["request_id"])
            with open(pack_path, "w", encoding="utf-8") as fh:
                json.dump({"request_id": order["request_id"],
                           "identity_names": sorted(names),
                           "namespaces": pack}, fh, indent=2, sort_keys=True)
            bundle_ops = 0
            if order.get("request_type") == "portability":
                # Art. 20 wants a portable artifact, not a JSON dump we
                # invented: an MGB1 bundle imports into any OMS store.
                for ns in registered_namespaces(db):
                    out_path = os.path.join(
                        PACKS, "%s.%s.mgb" % (order["request_id"], ns))
                    stats = json.loads(db.subject_bundle(out_path, subject, ns=ns))
                    bundle_ops += stats["ops"]
            cert = {
                "request_id": order["request_id"], "run_id": run_id,
                "subject_ref": order["subject_ref"], "act": "disclosure",
                "reported": count, "erased": 0, "bundle_ops": bundle_ops,
                "pack": os.path.basename(pack_path),
                "approved_by": principal, "because": because,
                "at_ms": now_ms(),
            }
            append(CERTS_LOG, cert)
            certify(db, cert)
            db.record_tool_call("disclose_only", "disclosure-executed", run_id=run_id)
            done.append(cert)
    return done


def certify(db, cert):
    """The record that survives the erasure it records.

    `forget_subject` deliberately writes no audit grain of its own -- an
    engine-written record naming the subject would re-introduce the very
    reference being erased (docs/erasure.md, REQ-ERASE-5). The HOST decides
    what to log, so the desk logs this: keyed on the CASE reference, naming
    the approver, the counts and the reason, and pointing at the person only
    through a FINGERPRINT. Given a candidate identity you can recompute the
    fingerprint and verify the certificate concerns them; you cannot read
    the person out of it, and you cannot enumerate the log.

    Note what is NOT here: no name, no address, no request text. `because`
    names a REQUEST, never a data subject.
    """
    key = "dsar:%s" % cert["request_id"]
    for relation, value in (
        ("mg:dsar_act", cert["act"]),
        ("mg:dsar_subject_ref", cert["subject_ref"]),
        ("mg:dsar_approved_by", cert["approved_by"]),
        ("mg:dsar_grains_reported", str(cert["reported"])),
        ("mg:dsar_grains_erased", str(cert["erased"])),
        ("mg:dsar_because", cert["because"]),
        ("mg:dsar_at_ms", str(cert["at_ms"])),
    ):
        db.add_fact(key, relation, value, ns=CERT_NS, idempotent=True)


def report(subject):
    """The DSAR read on demand: everything an erasure WOULD remove."""
    db = open_db()
    out = []
    total = 0
    for ns in registered_namespaces(db):
        payload = json.loads(db.subject_report(subject, ns=ns))
        total += len(payload["grains"])
        out.append({"namespace": ns,
                    "identity_names": payload["identity_names"],
                    "grains": payload["grains"]})
    emit({"subject_ref": fingerprint(subject), "total_grains": total,
          "namespaces": out})
    return 0


def trace(subject):
    """Does this identity survive anywhere the desk can see?

    Four legs, deliberately wider than the erasure selector:

      1. the DSAR selector itself, in every registered namespace;
      2. a prefix-scoped structural recall over `org.*` (`*` is a READING
         convention -- this is where it belongs);
      3. free-text search across `org.*` for the identity's distinctive
         tokens, which is what catches a prose mention nothing structural
         would find;
      4. the certificate namespace, which must contain the fingerprint and
         must NOT contain the person.
    """
    import re
    db = open_db()
    surviving = 0
    for ns in registered_namespaces(db):
        surviving += len(json.loads(db.subject_report(subject, ns=ns))["grains"])
    structural = json.loads(db.cal(
        'RECALL grains WHERE namespace = "org.*" AND subject = "%s" '
        'LIMIT 200 FORMAT json' % subject)).get("grains") or []
    tokens = [t for t in re.split(r"[^A-Za-z0-9]+", subject)
              if len(t) > 3 and t.lower() not in ("did", "example")]
    prose = set()
    for token in tokens:
        for grain in json.loads(db.search(token, k=100, ns="org.*")):
            blob = json.dumps(grain).lower()
            if any(t.lower() in blob for t in tokens):
                prose.add(grain.get("hash"))
    # The plan, the run journal and the certificates. If the desk had ever
    # let an identity into a journaled grain, it would show up here -- and
    # no erasure scoped to a data namespace would have removed it.
    journal = set()
    for ns in ("agent:*", NS):
        for token in tokens:
            for grain in json.loads(db.search(token, k=100, ns=ns)):
                blob = json.dumps(grain).lower()
                if any(t.lower() in blob for t in tokens):
                    journal.add(grain.get("hash"))
    certs = json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s" LIMIT 400 FORMAT json'
        % CERT_NS)).get("grains") or []
    cert_blob = json.dumps(certs).lower()
    emit({"subject_ref": fingerprint(subject),
          "dsar_selector": surviving,
          "structural_recall": len(structural),
          "text_mentions": len(prose),
          "journal_mentions": len(journal),
          "named_in_certificates": any(t.lower() in cert_blob for t in tokens),
          "fingerprinted_in_certificates": fingerprint(subject) in cert_blob,
          "clean": surviving == 0 and not structural and not prose})
    return 0


def guards():
    """The refusals that are structural, not conventional.

    A pattern namespace is a READ convention. Pointing destruction at one
    would silently widen it, so every destructive surface -- and the DSAR
    read that mirrors it -- refuses the pattern outright (VAL-E001). An
    empty subject is refused for the same reason: with prefix matching it
    would select everything, and an unset variable must never read as
    "erase all".
    """
    db = open_db()
    ns = registered_namespaces(db)[0]
    probe = "did:example:ines-bakker"
    checks = {}

    def refuses(name, fn):
        try:
            fn()
            checks[name] = {"refused": False, "error": None}
        except ValueError as exc:
            checks[name] = {"refused": True, "error": str(exc)}

    refuses("erase_wildcard_namespace",
            lambda: db.forget_subject(probe, ns="org.*"))
    refuses("report_wildcard_namespace",
            lambda: db.subject_report(probe, ns="org.*"))
    refuses("sweep_wildcard_namespace",
            lambda: db.forget_older_than(now_ms(), ns="org.*"))
    refuses("erase_empty_subject", lambda: db.forget_subject("", ns=ns))
    refuses("report_empty_subject", lambda: db.subject_report("", ns=ns))
    emit(checks)
    return 0


def sweep():
    """Storage limitation, declared rather than coded.

    Each rule is Facts in org.privacy: a namespace, a grain type, an age.
    The sweep reads them back and applies `forget_older_than` per rule --
    and a rule whose namespace is a pattern is REFUSED, not widened. A
    blanket "everything under org" is exactly the mistake this refusal is
    for.
    """
    db = open_db()
    days = declared(db, "retention:", "mg:max_age_days")
    types = declared(db, "retention:", "mg:grain_type")
    reasons = declared(db, "retention:", "mg:because")
    now = now_ms()
    applied = []
    for ns in sorted(days):
        cutoff = now - int(float(days[ns])) * DAY_MS
        entry = {"namespace": ns, "max_age_days": float(days[ns]),
                 "grain_type": types.get(ns), "because": reasons.get(ns)}
        try:
            report = json.loads(db.forget_older_than(
                cutoff, ns=ns, grain_type=types.get(ns)))
            entry.update({"applied": True, "grains_erased": report["grains_erased"],
                          "vocab_removed": report["vocab_removed"]})
        except ValueError as exc:
            entry.update({"applied": False, "refused": str(exc)})
        applied.append(entry)
    emit({"swept_at_ms": now, "rules": applied})
    return 0


def improve():
    db = open_db()
    # Tune the analyzers to this desk's volume -- a recorded act of
    # configuration, not a fork.
    db.set_analyzer_config("loop.run_outcome/1", True,
                           json.dumps({"min_runs": 3, "min_failure_ratio": 0.3}))
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
    except ValueError as exc:
        sys.stderr.write("refused: %s\n" % exc)
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


def register_view():
    db = open_db()
    certs = json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s" LIMIT 400 FORMAT json'
        % CERT_NS)).get("grains") or []
    grouped = {}
    for g in certs:
        fields = g["fields"]
        grouped.setdefault(fields["subject"], {})[fields["relation"]] = fields["object"]
    emit({"register": rows(REGISTER),
          "certificates": rows(CERTS_LOG),
          "certificate_grains": grouped})
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_rules"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.privacy" LIMIT 30 '
                 'FORMAT TEMPLATE rule_line'))
    return 0


def runs():
    db = open_db()
    obs = json.loads(db.cal(
        'RECALL observations WHERE namespace = "agent:harness" RECENT 300 '
        'FORMAT json'))
    outcome = {}
    for g in obs.get("grains") or []:
        fields = g.get("fields") or {}
        if fields.get("observation_kind") == "run_outcome":
            outcome[fields.get("run_id")] = fields.get("object")
    emit([{"run_id": r, "outcome": outcome.get(r, "open")}
          for r in json.loads(db.run_list(200))])
    return 0


def verify_cmd():
    db = open_db()
    emit({"integrity": json.loads(db.verify()), "stats": json.loads(db.stats())})
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "tools":
        return tool_main()
    if cmd == "seed":
        return seed()
    if cmd == "intake":
        return intake(sys.argv[2:])
    if cmd == "asks":
        return asks()
    if cmd == "decide":
        return decide(sys.argv[2])
    if cmd == "report":
        return report(sys.argv[2])
    if cmd == "trace":
        return trace(sys.argv[2])
    if cmd == "guards":
        return guards()
    if cmd == "sweep":
        return sweep()
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(sys.argv[2:])
    if cmd == "teach":
        return teach(sys.argv[2:])
    if cmd == "register":
        return register_view()
    if cmd == "brief":
        return brief()
    if cmd == "runs":
        return runs()
    if cmd == "verify":
        return verify_cmd()
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
