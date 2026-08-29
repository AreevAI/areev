#!/usr/bin/env python3
"""clinical referrals: the whole agent, one file, embedded Areev.

A specialist chest-pain clinic's referral desk. Letters arrive from GP
practices, the desk triages them, and a clinician signs every acceptance or
redirection. The interesting part is what happens when the desk needs help
from outside the clinic.

THE PROPERTY THIS EXAMPLE EXISTS FOR: PSEUDONYMIZATION ON EGRESS.
The memory holds the real values -- name, date of birth, MRN, phone, email,
all identified, all queryable by the clinician. What LEAVES the memory is
typed placeholders: `[PERSON_1]`, `[DATE_1]`, `[MRN_1]`, `[PHONE_1]`,
`[EMAIL_1]`. One declaration does it:

    db.set_anon_policy("org.clinic.referrals",
                       '{"mode":"egress","scope":"session"}')

after which every model-facing read of that namespace -- `recall`, CAL, the
context a trigger assembles for a run -- comes back pseudonymized. Nothing in
this file rewrites text on the way out; the store does it at the read exit.
The mapping stays in this process (`anon_mappings()`), so the clinician's
letter is rehydrated in-process and the identifiers never travel.

`invoice-to-accounting` only *measures* (`mode: "audit"`). This one rewrites.
The five valid modes are `off` / `egress` / `ingress` / `both` / `audit` --
there is no "rewrite" mode, whatever some prose calls it.

Four subcommands are subprocess seams, spawned by the runtime or by the
store. None of them opens the memory -- the party that spawned them is
holding it:

    agent.py tools        the host tools        ($AREEV_TOOL_NAME picks one)
    agent.py connector    the referral inbox    (fixtures in, items+cursor out)
    agent.py service      THE OUTSIDE -- a stand-in for a clinical coding and
                          triage-suggestion API. Everything it sees, it saw
                          because the desk sent it. `out/egress.jsonl` is the
                          wire log, and the act scripts audit it.
    agent.py ner          the Tier-1 detector seam (`set_anonymizer_command`)

Everything else is the driver:

    agent.py seed         plan, tool definitions, protocol, saved queries,
                          THE ANONYMIZATION POLICY, the inbox trigger
    agent.py intake       file today's letters into the clinical namespace,
                          identified -- this is the system of record
    agent.py ingest       one trigger-evaluation pass (a heartbeat tick)
    agent.py asks         the parked runs waiting on a clinician
    agent.py review FILE  apply a clinician's decision to its parked run
    agent.py outbound REF what would leave the memory for this referral
    agent.py letter REF   the clinician's identified acknowledgement
                          (rehydrate_text puts the real values back)
    agent.py reveal REF   admin-gated reverse lookup + its Tier-2 audit row
    agent.py policies     the declared policies, and the host floor
    agent.py policy-drill what a policy on an OPERATIONAL namespace does
    agent.py floor-check  the host cap is a cap, and is never persisted
    agent.py harden       demand a Tier-1 detector -- reads fail closed until
                          the host installs one
    agent.py brief        the desk's self-briefing (saved CAL queries)
    agent.py improve      the loop reads the desk's own history back
    agent.py govern R approve|apply|dismiss --because "..." --as user:X
    agent.py runs         run list as JSON (the acts assert on this)
    agent.py trigger-state        what has fired, and what failed

To make it real, replace `connector` with your document intake and `service`
with the coding API you actually call. The policy, the placeholders, the
gate, the mapping custody and the audit trail do not change.

PSEUDONYMIZATION IS NOT ANONYMISATION. See the README.
"""

import hashlib
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.dirname(HERE)
FIXTURES = os.environ.get("REF_FIXTURES", os.path.join(EXAMPLE, "fixtures"))
INBOX = os.path.join(FIXTURES, "referrals")
# The acts advance this "clock": the inbox only serves fixtures whose 2-digit
# prefix is <= it, so week one and week two are one committed directory.
REF_UPTO = os.environ.get("REF_UPTO", "03")
# Whether THIS HOST has a Tier-1 NER detector installed. A host capability,
# exactly like an embedder -- it is not in the file, and the file's policy
# cannot conjure it. Set it and `open_db` installs `agent.py ner`.
HAS_NER = bool(os.environ.get("CLINIC_NER"))

OUT = os.environ.get("AGENT_OUT", os.path.join(HERE, "out"))
DB = os.environ.get("AGENT_DB", os.path.join(OUT, "agent.db"))
EGRESS = os.path.join(OUT, "egress.jsonl")   # the wire log: what left the clinic
LEDGER = os.path.join(OUT, "clinic.jsonl")   # accepted + redirected referrals
LETTERS = os.path.join(OUT, "letters")

NS = "org.ops"                     # plan, tool definitions, trigger, journals
CLIN = "org.clinic.referrals"      # PATIENTS. identified at rest, pseudonymous on egress
PROT = "org.clinic.protocol"       # how the desk triages. read back as INPUT -- never rewritten
DESK = "agent:referral-desk"       # the agent -- it can never sign a triage
INBOX_SCOPE = "inbox:referrals"

# Pinned so the seeder mints stable content addresses. A grain is its bytes.
EPOCH_MS = 1756000000000

# The one policy this example turns on. `scope: "session"` keeps a token
# stable for the life of a handle, so the mapping a read hands back still
# resolves the tokens an earlier read in the same process produced.
CLINICAL_POLICY = {"mode": "egress", "scope": "session"}

TOKEN = re.compile(r"\[[A-Z][A-Z_]*_[0-9]+\]")


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def marker(referral_id):
    return hashlib.sha256(referral_id.encode()).hexdigest()[:12]


def slug(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")[:48]


def inbox_files():
    return sorted(n for n in os.listdir(INBOX)
                  if n.endswith(".json") and n[:2] <= REF_UPTO)


def self_cmd(sub):
    return "%s %s %s" % (sys.executable, os.path.abspath(__file__), sub)


# -- reading the run's context ---------------------------------------------
# The runtime hands each tool the merged run state on stdin. `context` is
# whatever the trigger's declared CAL query returned -- and by the time it
# gets here it has ALREADY been through the egress gate, because the query
# read `org.clinic.referrals` and that namespace has a policy. No tool in
# this file anonymizes anything; the store did it at the read exit.

def walk(node, out):
    if isinstance(node, dict):
        if "relation" in node and "subject" in node:
            out.append(("fact", node))
        elif "content" in node and "role" in node:
            out.append(("event", node))
        for v in node.values():
            walk(v, out)
    elif isinstance(node, list):
        for v in node:
            walk(v, out)
    return out


def facts(grains, subject=None, relation=None):
    return [g for kind, g in grains if kind == "fact"
            and (subject is None or g.get("subject") == subject)
            and (relation is None or g.get("relation") == relation)]


# -- the tools seam ---------------------------------------------------------

def tool_main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    grains = walk(state.get("context") or {}, [])
    tool = os.environ.get("AREEV_TOOL_NAME", "")
    rid = item.get("referral_id", "?")

    if tool == "extract":
        # The letter as MEMORY RETURNS IT -- placeholders where the
        # identifiers were. The desk does not need to read a date of birth to
        # know one is on file: it checks that `[DATE_1]` is THERE.
        events = [g for kind, g in grains if kind == "event"]
        letter = events[0]["content"] if events else ""
        if not letter:
            sys.stderr.write("no referral letter on file for %s\n" % rid)
            return 1
        required = [f["object"] for f in facts(grains, "intake", "mg:required_identifier")]
        missing = [r for r in required if "[%s_" % r.upper() not in letter]
        if missing:
            # Loud, not silent. A referral triaged without the identifiers
            # that pin it to a patient is the expensive failure; a referral
            # that stops at the desk is the cheap one.
            sys.stderr.write("%s is missing required identifier(s): %s\n"
                             % (rid, ", ".join(missing)))
            return 1
        terms = [f["object"] for f in facts(grains, "triage", "mg:complaint_term")]
        # Whichever term the letter raises FIRST is the presenting complaint --
        # a fixed rule, so recall order can never change a triage decision.
        body = letter.lower()
        found = sorted((t for t in terms if t in body), key=body.index)
        emit({
            "referral_id": rid,
            "complaint": found[0] if found else "unclassified",
            "narrative": letter,
            "identifiers_present": sorted(
                set(m.split("_")[0].lstrip("[").lower() for m in TOKEN.findall(letter))),
            "intake_complete": True,
        })

    elif tool == "code_lookup":
        # THE EGRESS. Everything in `request` came out of the memory through
        # the policy, so everything identifying in it is a placeholder. The
        # wire log records the exchange verbatim -- the act scripts read it
        # and assert no identifier is in there.
        request = {"referral_id": rid,
                   "complaint": state.get("complaint"),
                   "narrative": state.get("narrative")}
        proc = subprocess.run(self_cmd("service").split(),
                              input=json.dumps(request, sort_keys=True),
                              capture_output=True, text=True)
        if proc.returncode != 0:
            sys.stderr.write("coding service failed: %s\n" % proc.stderr.strip())
            return 1
        response = json.loads(proc.stdout)
        append(EGRESS, {"referral_id": rid, "sent": request, "received": response})
        emit(response)

    elif tool == "triage":
        # The outside suggests; the clinic's own protocol decides. A rule a
        # clinician wrote here OVERRIDES the service -- which is the whole
        # point of holding the protocol in memory rather than in the vendor.
        complaint = state.get("complaint") or "unclassified"
        rule = facts(grains, complaint, "mg:triage_urgency")
        if rule:
            urgency, source = rule[0]["object"], "clinic_rule"
            why = "the clinic's own rule for %r, written by a clinician" % complaint
        else:
            urgency = state.get("suggested_urgency") or "routine"
            source = "external_service"
            why = "no clinic rule on file; taking the coding service's suggestion"
        in_scope = [f["object"] for f in facts(grains, "triage", "mg:in_scope")]
        route = "accept" if complaint in in_scope else "redirect"
        elsewhere = facts(grains, "triage", "mg:out_of_scope_route")
        emit({"urgency": urgency, "urgency_source": source,
              "proposed_route": route, "why": why,
              "redirect_to": elsewhere[0]["object"] if elsewhere else "unallocated"})

    elif tool in ("accept", "redirect"):
        row = {"referral_id": rid, "source": item.get("source"),
               "complaint": state.get("complaint"),
               "snomed_code": state.get("snomed_code"),
               "urgency": state.get("urgency"),
               "urgency_source": state.get("urgency_source"),
               "corrected": bool(state.get("corrected")),
               "route": "accepted" if tool == "accept" else "redirected",
               "decided_by": state.get("responder", "auto")}
        if tool == "redirect":
            row["redirect_to"] = state.get("redirect_to")
        append(LEDGER, row)
        emit({tool: 1, "referral_id": rid})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1
    return 0


# -- the connector seam -----------------------------------------------------
# The inbox lists what has ARRIVED. It carries no clinical content and no
# identifier -- a referral id, who sent it, when. Everything else the run
# needs comes out of the memory, through the policy.
# An ABSENT cursor means "seed and fire nothing", so declaring the trigger
# never replays the inbox.

def connector_main():
    req = json.load(sys.stdin)
    names = inbox_files()
    if req.get("cursor") is None:
        emit({"items": [], "cursor": "0", "more": False})
        return 0
    consumed = int(req["cursor"])
    items = []
    for name in names[consumed:consumed + int(req.get("max_items", 100))]:
        with open(os.path.join(INBOX, name)) as fh:
            ref = json.load(fh)
        items.append({"id": ref["referral_id"],
                      "payload": {"referral_id": ref["referral_id"],
                                  "source": ref["source"],
                                  "received_at": ref["received_at"]}})
    emit({"items": items, "cursor": str(consumed + len(items)),
          "more": consumed + len(items) < len(names)})
    return 0


# -- the outside ------------------------------------------------------------

def service_main():
    """A stand-in for a clinical coding + triage-suggestion API.

    This process is OUTSIDE the clinic's trust boundary. It never opens the
    memory, never reads the fixtures, and knows only what arrived on stdin.
    It echoes the narrative's length back so the wire log shows plainly that
    it received prose -- prose with placeholders where the patient was.
    """
    req = json.load(sys.stdin)
    table = {
        "chest tightness on exertion": ("29857009", "Chest pain", "routine", 0.62),
        "palpitations": ("80313002", "Palpitations", "routine", 0.71),
        "murmur": ("88610006", "Cardiac murmur", "routine", 0.55),
    }
    code, display, urgency, conf = table.get(
        req.get("complaint"), ("261665006", "Unspecified", "routine", 0.10))
    emit({"service": "claritycode-mock", "snomed_code": code, "display": display,
          "suggested_urgency": urgency, "confidence": conf,
          "narrative_chars": len(req.get("narrative") or "")})
    return 0


# -- the Tier-1 detector seam ----------------------------------------------

CUE = re.compile(
    r"\b(?:daughter|son|wife|husband|partner|mother|father|carer|"
    r"next of kin|sister|brother|neighbour)\b[, ]+(?:is |named |called )?", re.I)
CAPS = re.compile(r"[A-Z][a-z]+(?:[ -][A-Z][a-z]+)+")


def ner_main():
    """`set_anonymizer_command` speaks JSON on stdio, one spawn per call:

        {"areev_anonymize":1,"op":"probe"}  -> {"id":..., "kind":"ner"}
        {"areev_anonymize":1,"op":"detect","text":"..."}
                                            -> {"detections":[{start,end,
                                                category,confidence}]}

    Offsets are UTF-8 byte positions over NFC text. A real deployment puts a
    model behind this. The rule here is deliberately crude and deterministic
    so the keyless floor stays keyless: a capitalized name right after a
    relationship word is a person. It catches exactly the thing the Tier-0
    floor cannot -- a third party the memory has never interned as a subject.
    """
    req = json.load(sys.stdin)
    if req.get("op") == "probe":
        emit({"id": "clinic-relatives/0.1", "kind": "ner"})
        return 0
    text = req.get("text") or ""
    found = []
    for cue in CUE.finditer(text):
        m = CAPS.match(text, cue.end())
        if m:
            found.append({"start": len(text[:m.start()].encode()),
                          "end": len(text[:m.end()].encode()),
                          "category": "person", "confidence": 0.8})
    emit({"detections": found})
    return 0


# -- the driver -------------------------------------------------------------

def open_db(actor=DESK):
    import areev
    os.makedirs(OUT, exist_ok=True)
    db = areev.Areev(DB, ns=NS, actor=actor)
    # A host capability, never a file truth: the policy can DEMAND a Tier-1
    # detector, but only the host can supply one. Without it a policy that
    # asks for `ner` fails the read closed (VAL-E001) rather than serving
    # the identifiers raw.
    if HAS_NER:
        db.set_anonymizer_command(self_cmd("ner"))
    return db


def read_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def seed():
    db = open_db()

    # 1. THE POLICY, first, so nothing is ever read out of the clinical
    #    namespace without it. It is a file truth: it replicates in bundles,
    #    and any host that opens this file inherits it.
    db.set_anon_policy(CLIN, json.dumps(CLINICAL_POLICY))
    # `org.ops` and `org.clinic.protocol` deliberately get NONE. See the
    # README, and `agent.py policy-drill` for what happens if you forget.

    def tool_def(name, description, **extra):
        fields = {"tool_name": name, "kind": "definition",
                  "tool_description": description, "created_at": EPOCH_MS}
        fields.update(extra)
        return db.add("tool", json.dumps(fields), ns=NS)

    extract = tool_def("extract", "read the referral letter as memory returns it")
    lookup = tool_def("code_lookup", "ask the outside coding service for a code "
                                     "and a triage suggestion")
    triage = tool_def("triage", "apply the clinic's own protocol over the suggestion")
    review = tool_def("clinician_review", "a clinician accepts or redirects the "
                                          "referral, and signs it",
                      executor_kind="client")
    accept = tool_def("accept", "book the referral into this clinic")
    redirect = tool_def("redirect", "send the referral to the right service")

    wf = db.add("workflow", json.dumps({
        "name": "clinical-referral-triage",
        "nodes": ["extract", "code_lookup", "triage", "clinician_review",
                  "accept", "redirect"],
        "edges": [
            {"src": "extract", "dst": "code_lookup"},
            {"src": "code_lookup", "dst": "triage"},
            {"src": "triage", "dst": "clinician_review"},
            {"src": "clinician_review", "dst": "accept",
             "cond": 'decision == "accept"'},
            {"src": "clinician_review", "dst": "redirect",
             "cond": 'decision == "redirect"'},
        ],
        "bindings": {"extract": extract, "code_lookup": lookup, "triage": triage,
                     "clinician_review": review, "accept": accept,
                     "redirect": redirect},
        "retries": {"code_lookup": 1},
        "created_at": EPOCH_MS,
    }), ns=NS)

    db.add("skill", json.dumps({
        "name": "referral-triage-judgment",
        "description": "how this desk reads a referral letter",
        "instructions": "Triage decides urgency, not identity. Nothing about "
                        "who the patient IS belongs in a request that leaves "
                        "the clinic. A missing required identifier stops the "
                        "referral at the desk -- never triage a letter you "
                        "cannot pin to a patient. The coding service suggests; "
                        "a clinician decides, and signs.",
        "created_at": EPOCH_MS,
    }), ns=NS)

    # 2. The protocol -- the desk's own operating knowledge, read back as
    #    INPUT on every run. It is not patient data and it must never be
    #    rewritten, so its namespace has no policy.
    protocol = read_json(os.path.join(FIXTURES, "protocol.json"))
    for ident in protocol["required_identifiers"]:
        db.add_fact("intake", "mg:required_identifier", ident, ns=PROT, idempotent=True)
    for term in protocol["complaint_terms"]:
        db.add_fact("triage", "mg:complaint_term", term, ns=PROT, idempotent=True)
    for term in protocol["in_scope"]:
        db.add_fact("triage", "mg:in_scope", term, ns=PROT, idempotent=True)
    db.add_fact("triage", "mg:out_of_scope_route", protocol["out_of_scope_route"],
                ns=PROT, idempotent=True)
    db.add_fact("protocol", "mg:version", protocol["protocol_version"],
                ns=PROT, idempotent=True)

    # 3. Retrieval + presentation ship IN the file and replicate with it.
    db.cal('DEFINE TEMPLATE rule_line AS "- {{subject}} {{relation}} {{object}}"')
    # The context a triage run is allowed to see. `letter` reads the clinical
    # namespace, so it comes back pseudonymized; `protocol` does not, so the
    # rules arrive intact. One query, two treatments, decided by the file.
    db.cal('DEFINE QUERY "referral_ctx"($session) '
           'DESCRIPTION "what a triage run may see about one referral" '
           'AS { ASSEMBLE "referral_ctx" FROM '
           'judgment: (RECALL skills LIMIT 2), '
           'protocol: (RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 60), '
           'letter: (RECALL events WHERE namespace = "org.clinic.referrals" '
           'AND session_id = $session RECENT 3) '
           'BUDGET 4000 tokens FORMAT json }')
    # Deliberately NO clinical group: a desk briefing is about the desk.
    db.cal('DEFINE QUERY "desk_pulse"() '
           'DESCRIPTION "the desk briefing itself: plan, tools, protocol, activity" '
           'AS { ASSEMBLE "desk_pulse" FROM '
           'plan: (RECALL workflows LIMIT 3), '
           'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), '
           'activity: (RECALL tools WHERE kind != "definition" RECENT 40), '
           'protocol: (RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 40) '
           'BUDGET 2500 tokens FORMAT markdown }')

    trigger = db.trigger_add(json.dumps({
        "kind": "polling",
        "connector": "mock",
        "scope": INBOX_SCOPE,
        "interval_secs": 1,
        "workflow": wf,
        "dedup_key": ["/referral_id"],
        "context_query": "referral_ctx($session = /referral_id)",
    }), "triage every referral that reaches the chest pain service", NS)

    emit({"workflow": wf, "trigger": trigger,
          "policies": json.loads(db.anon_policies())})
    return 0


def intake():
    """File today's letters into the clinical namespace, IDENTIFIED.

    This is the clinic's system of record and it is inside the trust
    boundary, so it stores the truth: `mode: "egress"` rewrites reads, never
    writes. Interning the patient and the referring GP as fact SUBJECTS is
    also what makes their names detectable in the letter's prose -- `subject`
    is an identity field by construction, and the gate seeds its per-namespace
    known-identity list from the file's own subjects.
    """
    db = open_db()
    filed = []
    for name in inbox_files():
        ref = read_json(os.path.join(INBOX, name))
        rid = ref["referral_id"]
        already = json.loads(db.recall(rid, k=1, ns=NS))
        if any(g["fields"].get("relation") == "mg:filed" for g in already):
            continue
        patient = ref["patient"]
        who = patient["name"]
        db.add_fact(who, "mg:referral", rid, ns=CLIN, idempotent=True)
        for field, relation in (("dob", "mg:dob"), ("mrn", "mg:mrn"),
                                ("phone", "mg:phone"), ("email", "mg:email")):
            if patient.get(field):
                db.add_fact(who, relation, patient[field], ns=CLIN, idempotent=True)
        db.add_fact(ref["referring_clinician"], "mg:practice", ref["source"],
                    ns=CLIN, idempotent=True)
        db.add("event", json.dumps({"content": ref["letter"], "role": "user",
                                    "session_id": rid}), ns=CLIN)
        # The operational marker carries the referral id and NOTHING else.
        db.add_fact(rid, "mg:filed", ref["source"], ns=NS, idempotent=True)
        filed.append(rid)
    emit({"filed": filed})
    return 0


def ingest():
    """One heartbeat tick over the referral inbox."""
    db = open_db()
    report = json.loads(db.trigger_run(
        connector_cmd=self_cmd("connector"),
        tool_cmd=self_cmd("tools"),
        max_usd_micros=2_000_000, max_wall_ms=300_000, ask_ttl_sec=3600,
    ))
    emit(report)
    return 0


def trigger_state():
    db = open_db()
    emit(json.loads(db.trigger_status()))
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
    """The clinician's queue. Note what is NOT in it: the run journal lives in
    `org.ops`, and it never held an identifier in the first place -- the
    context it recorded was pseudonymized on its way out of the clinical
    namespace. That is why the operational namespace needs no policy."""
    db = open_db()
    rows = []
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        rows.append({"run_id": run_id, "ask": ask_id,
                     "marker": marker(item.get("referral_id", "?")),
                     "referral_id": item.get("referral_id"),
                     "source": item.get("source"),
                     "complaint": state.get("complaint"),
                     "proposed_urgency": state.get("urgency"),
                     "urgency_source": state.get("urgency_source"),
                     "proposed_route": state.get("proposed_route"),
                     "narrative": state.get("narrative")})
    emit(rows)
    return 0


def review(path):
    """A clinician's decision, read from a fixture the way a case-manager
    webhook would deliver it. The desk cannot sign its own triage: the
    runtime refuses the principal that started the run."""
    note = read_json(path)
    principal = note.get("clinician", "user:unknown")
    ref = note.get("marker")
    verdict = note.get("decision")
    because = note.get("because", "")
    if verdict not in ("accept", "redirect") or not because:
        sys.stderr.write("a triage decision needs a verdict and a reason\n")
        return 3

    db = open_db()
    for run_id, ask_id, state in pending_asks(db):
        item = state.get("item", state)
        rid = item.get("referral_id", "?")
        if marker(rid) != ref:
            continue
        proposed = state.get("urgency")
        signed = note.get("urgency", proposed)
        corrected = signed != proposed
        result = {"decision": verdict, "urgency": signed,
                  "urgency_source": "clinician" if corrected
                                    else state.get("urgency_source"),
                  "corrected": corrected, "responder": principal,
                  "because": because,
                  "redirect_to": note.get("redirect_to", state.get("redirect_to"))}
        try:
            db.run_respond(run_id, ask_id, json.dumps(result), principal)
        except ValueError as e:
            sys.stderr.write("respond refused: %s\n" % e)
            return 4
        if corrected:
            # THE LESSON. A clinician overruled the coding service, and the
            # reason they gave is now the clinic's rule for that complaint.
            # It is in `org.clinic.protocol`, which has no anonymization
            # policy -- the desk reads it back as input on every later run.
            complaint = state.get("complaint") or "unclassified"
            db.add_fact(complaint, "mg:triage_urgency", signed,
                        ns=PROT, idempotent=True)
            db.add_fact(complaint, "mg:triage_rule_by", principal,
                        ns=PROT, idempotent=True)
            db.record_tool_call("triage", "corr:urgency:%s" % slug(complaint),
                                is_error=False, run_id=run_id)
        outcome = json.loads(db.run_resume(run_id, tool_cmd=self_cmd("tools")))
        emit({"run_id": run_id, "referral_id": rid, "decision": verdict,
              "responder": principal, "corrected": corrected,
              "proposed_urgency": proposed, "signed_urgency": signed,
              "outcome": outcome})
        return 0
    sys.stderr.write("no parked run matches marker %s\n" % ref)
    return 5


def letter_text(db, rid):
    """The referral as memory returns it, plus this handle's live mapping."""
    grains = json.loads(db.cal(
        'RECALL events WHERE namespace = "org.clinic.referrals" '
        'AND session_id = "%s" RECENT 3 FORMAT json' % rid))
    text = ""
    for g in grains.get("grains") or []:
        text = g["fields"].get("content") or text
    mapping = {}
    for row in json.loads(db.anon_mappings()):
        if row["ns"] == CLIN:
            mapping = row["mapping"]
    return text, mapping, grains.get("anonymized") or {}


def outbound(rid):
    """Exactly what would leave the clinic for this referral.

    Nothing here rewrites anything. The text is what `RECALL` handed back.
    """
    db = open_db()
    try:
        text, mapping, block = letter_text(db, rid)
    except ValueError as e:
        # Fail-closed (D6). The file's policy demands a detector chain this
        # host cannot supply, so the read REFUSES rather than handing back
        # the identifiers it was told to hide.
        emit({"referral_id": rid, "refused": True, "error": str(e)})
        return 1
    emit({"referral_id": rid, "narrative": text,
          "placeholders": sorted(set(TOKEN.findall(text))),
          "anonymized": block, "mapping_size": len(mapping)})
    return 0


def ledger_row(rid):
    if not os.path.exists(LEDGER):
        return None
    for line in open(LEDGER, encoding="utf-8"):
        row = json.loads(line)
        if row.get("referral_id") == rid:
            return row
    return None


def letter(argv):
    """The clinician's acknowledgement, with the real values put back.

    The body is composed from what memory returned -- placeholders and all --
    and `rehydrate_text` restores the originals from the mapping this process
    is holding. The mapping never left, so nothing had to be looked up.
    """
    rid = argv[0]
    who = argv[1] if len(argv) > 1 else "user:asha"
    db = open_db(actor=who)
    text, mapping, _ = letter_text(db, rid)
    row = ledger_row(rid) or {}
    body = (
        "CHEST PAIN SERVICE -- REFERRAL ACKNOWLEDGEMENT\n"
        "Referral: %s    Outcome: %s    Urgency: %s (%s)\n"
        "Signed by: %s\n\n"
        "%s\n" % (rid, row.get("route", "pending"), row.get("urgency", "-"),
                  row.get("urgency_source", "-"), row.get("decided_by", "-"), text))
    restored = json.loads(db.rehydrate_text(body, json.dumps(mapping)))
    os.makedirs(LETTERS, exist_ok=True)
    path = os.path.join(LETTERS, "%s.txt" % rid)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(restored["text"])
    emit({"referral_id": rid, "path": path, "replaced": restored["replaced"],
          "unmatched": restored["unmatched"], "reader": who})
    return 0


def reveal(argv):
    """Reverse a placeholder on purpose, and leave a record that you did.

    `reveal_tokens` is admin-gated and Tier-2 audited: the audit Observation
    names a subject FINGERPRINT, never the identity -- an immutable grain
    naming the person it un-masked would defeat the point.
    """
    rid = argv[0]
    who = argv[1] if len(argv) > 1 else "user:asha"
    db = open_db(actor=who)
    text, _, _ = letter_text(db, rid)
    tokens = sorted(set(TOKEN.findall(text)))
    revealed = json.loads(db.reveal_tokens(CLIN, json.dumps(tokens)))["revealed"]
    audit = json.loads(db.cal(
        'RECALL observations WHERE namespace = "agent:authz" RECENT 20 FORMAT json'))
    emit({"referral_id": rid, "reader": who, "tokens": tokens,
          "revealed": revealed,
          "audit": [g["fields"] for g in (audit.get("grains") or [])]})
    return 0


def policies():
    db = open_db()
    probe = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 1 FORMAT json'))
    emit({"declared": json.loads(db.anon_policies()),
          "floor": (probe.get("anonymized") or {}).get("floor", False)})
    return 0


def protocol_rows(db):
    grains = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 40 FORMAT json'))
    return sorted((g["fields"]["subject"], g["fields"]["relation"],
                   g["fields"]["object"], g["hash"])
                  for g in (grains.get("grains") or []))


def policy_drill():
    """Why the OPERATIONAL namespaces never get a policy.

    An egress rewriter is for what leaves. `org.clinic.protocol` does not
    leave -- the desk reads it back as INPUT on every run. Point a rewriter
    at it and the desk's own rules come back keyed by `[PERSON_1]` with the
    version stamped `[DATE_1]`, and `extract` can no longer find a single
    one of them. The same argument covers `org.ops`, where the plan, the
    tool bindings and the run journal live.

    The drill also shows the other half: the FILE was never harmed. Every
    grain hash is identical before, during and after -- `egress` rewrote a
    read, not a byte on disk.
    """
    db = open_db()
    before = protocol_rows(db)
    db.set_anon_policy(PROT, '{"mode":"egress"}')
    during = protocol_rows(db)
    db.clear_anon_policy(PROT)
    after = protocol_rows(db)
    emit({
        "before": [r[:3] for r in before],
        "during": [r[:3] for r in during],
        "after": [r[:3] for r in after],
        # The same grains came back all three times -- rewritten, then not.
        "hashes_stable": sorted(r[3] for r in before)
                         == sorted(r[3] for r in during)
                         == sorted(r[3] for r in after),
        "rows": len(before),
        "still_declared": [p["ns"] for p in json.loads(db.anon_policies())],
    })
    return 0


def floor_check():
    """`set_anonymize_egress_floor` is a HOST cap, not a file truth.

    It turns egress on for every namespace WITHOUT a declared policy, it can
    never weaken one that has one, and reopening the file forgets it. Which
    is also the reason it is not how you protect the clinical namespace: a
    cap you can forget to set is not a policy.
    """
    db = open_db()
    plain = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 40 FORMAT json'))
    db.set_anonymize_egress_floor(True)
    capped = json.loads(db.cal(
        'RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 40 FORMAT json'))
    del db
    reopened = json.loads(open_db().cal(
        'RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 40 FORMAT json'))

    def view(res):
        return {"floor": (res.get("anonymized") or {}).get("floor", False),
                "subjects": sorted({g["fields"]["subject"]
                                    for g in (res.get("grains") or [])})}
    emit({"before": view(plain), "with_floor": view(capped),
          "reopened": view(reopened)})
    return 0


def harden(argv):
    """Extend the detector chain beyond the Tier-0 floor.

    Tier-0 catches shapes (dates, phones, emails, MRNs) and identities the
    memory already knows as subjects. It cannot catch a third party who
    appears once, in prose, and was never interned -- a relative named in a
    letter. `detectors: ["tier0","ner"]` says the policy DEMANDS a Tier-1
    backend; a host without one fails the read closed rather than serving
    the name raw.
    """
    db = open_db()
    policy = dict(CLINICAL_POLICY)
    policy["detectors"] = ["tier0", "ner"]
    policy["because"] = (argv[0] if argv else
                         "the Tier-0 floor cannot see a relative named once in prose")
    db.set_anon_policy(CLIN, json.dumps(policy))
    emit({"policy": policy, "declared": json.loads(db.anon_policies()),
          "host_has_ner": HAS_NER})
    return 0


def brief():
    db = open_db()
    print(db.cal('RUN "desk_pulse"()'))
    print(db.cal('RECALL facts WHERE namespace = "org.clinic.protocol" LIMIT 20 '
                 'FORMAT TEMPLATE rule_line'))
    return 0


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
          for r in json.loads(db.run_list(200))])
    return 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    rest = sys.argv[2:]
    if cmd == "tools":
        return tool_main()
    if cmd == "connector":
        return connector_main()
    if cmd == "service":
        return service_main()
    if cmd == "ner":
        return ner_main()
    if cmd == "seed":
        return seed()
    if cmd == "intake":
        return intake()
    if cmd == "ingest":
        return ingest()
    if cmd == "trigger-state":
        return trigger_state()
    if cmd == "asks":
        return asks()
    if cmd == "review":
        return review(rest[0])
    if cmd == "outbound":
        return outbound(rest[0])
    if cmd == "letter":
        return letter(rest)
    if cmd == "reveal":
        return reveal(rest)
    if cmd == "policies":
        return policies()
    if cmd == "policy-drill":
        return policy_drill()
    if cmd == "floor-check":
        return floor_check()
    if cmd == "harden":
        return harden(rest)
    if cmd == "brief":
        return brief()
    if cmd == "improve":
        return improve()
    if cmd == "govern":
        return govern(rest)
    if cmd == "runs":
        return runs()
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
