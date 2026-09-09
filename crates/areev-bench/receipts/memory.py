#!/usr/bin/env python3
"""The Areev bridge: record → loop → review → apply/rollback → lesson assembly.

The one honesty rule this module enforces: the LESSONS prompt section is
assembled from LIVE grains on every read, so Areev's own apply/rollback is
the only lever that can change the agent's prompt — there is no harness flag.

Handles are opened for one operation and dropped, never nested: one memory
is one file and one open handle (STO-E002), and the agent, the recorder and
the reviewer are three different actors. That is not a workaround — the
reviewer being a separate identity from the runner is what the Review gate
checks, so the constraint and the governance model agree.
"""
import gc
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "scripts"))

import accountant as acct
import bench_run
import cal_assemble as cal

TRACK = "receipts"   # the directory `bench_govern.py --harness` loads
NS = "ledger"
RUNNER = "agent:receipt-capture"
REVIEWER = "user:accountant"
CAPTURE_ENTITY = "receipt_capture"


def capture_entity(profile=None):
    """The subject every observation and convention hangs off. It was
    hard-coded to `receipt_capture` for every corpus, and the loop's evidence
    therefore told the proposer it was reading receipts whatever the
    documents were: on registration forms it wrote rules "on every receipt"
    and the reviewer, told they were forms, refused all seven. The default
    keeps every published run byte-identical."""
    if not profile:
        return CAPTURE_ENTITY
    return re.sub(r"[^a-z0-9]+", "_", profile["document_noun"].lower()).strip("_") + "_capture"

# Relations this harness writes itself. They are evidence for the loop, never
# instructions for the agent, so they must never render into the prompt.
INTERNAL_RELATIONS = {"reading_result", "correction", "capture_attempt"}

# The subjects this harness writes per document (`document_0007`). A proposal
# targeting one of these is about a single receipt, not a rule for future ones.
DOCUMENT_SUBJECT = re.compile(r"^document_\d+$")
DOCUMENT_SUBJECT_SEQ = re.compile(r"^document_(\d+)$")

# --------------------------------------------------------------------------
# the prompt, as CAL the file carries
# --------------------------------------------------------------------------
#
# Every model-facing block below is an ASSEMBLE registered in the memory as a
# saved query, rendered by a template registered in the same file. Nothing
# here is assembled in Python: the harness runs `RUN "ledger_rules"($ns=…)`
# and puts the answer in the prompt.
#
# The move was made byte-for-byte on purpose. `structure.py` measured what
# CAL's DEFAULT markdown costs on these grains -- a seven-rule memory scored
# 35 against the hand-assembled prompt's 141, because `FORMAT markdown`
# prefixes every rule with its subject and relation and suffixes it with a
# date. So the templates below emit the published bytes exactly, and
# `selftest.py` asserts that against the retired hand-rolled renderer. The
# published numbers stay comparable; the assembly is the engine's.

# Rules the agent must follow. `lesson` comes from the LLM leg, `fails_with`
# from the deterministic one, and they are one section because the agent has
# no use for the distinction.
RULE_RELATIONS = ("lesson", "fails_with")

# LIMIT for each section. Well above any observed run (a 320-document
# deployment writes ~11 rules) and checked at read time: a section that comes
# back holding exactly this many has dropped rows, and `cal.section(cap=…)`
# raises rather than handing the model a prompt missing its oldest rules --
# the failure this harness hit at 300, and would have hit again at 1000.
SECTION_CAP = 500

RULES_HEADING = (
    "## INSTRUCTIONS FROM THE ACCOUNTANT\n"
    "These come from the person who files these documents and they OVERRIDE "
    "the day-one instruction above. If a rule names a field to capture, that "
    "field is REQUIRED: put it in your JSON, in addition to the day-one field."
)
SAID_HEADING = (
    "## WHAT THE ACCOUNTANT HAS TOLD YOU\n"
    "Everything the person who files these has said, oldest first."
)


def _cal_list(values):
    return "(%s)" % ", ".join('"%s"' % v for v in values)


REGISTRY = list(cal.REVIEW_REGISTRY) + [
    # -- renderers ---------------------------------------------------------
    # `{{#if assembly.grain_count}}` is what makes a withdrawn rule set render
    # to the empty string rather than to a heading announcing rules that are
    # no longer there. Arm A depends on it.
    cal.guarded_template("ledger_rules_tpl", RULES_HEADING, "- {{grain.object}}"),
    cal.guarded_template("ledger_conventions_tpl", "## CONVENTIONS",
                         "- {{grain.relation | humanize}}: {{grain.object}}"),
    cal.guarded_template("ledger_said_tpl", SAID_HEADING, "- {{grain.object}}"),

    # -- reads -------------------------------------------------------------
    # `ORDER BY object ASC` inside the source is the `sorted(set(...))` the
    # hand-rolled renderer did, and `WITH dedup(object)` is the `set(...)`.
    cal.saved_query(
        "ledger_rules", ["ns"],
        '  ASSEMBLE "operating rules" FOR "the capture agent" FROM\n'
        '    rules: (RECALL facts WHERE namespace = $ns AND relation IN %s\n'
        '            ORDER BY object ASC LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE ledger_rules_tpl\n'
        '  WITH dedup(object)' % (_cal_list(RULE_RELATIONS), SECTION_CAP,
                                  cal.MAX_BUDGET_TOKENS),
        "the approved rules, as the capture agent reads them"),
    # Scoped by SUBJECT, so a long deployment's thousands of per-document
    # facts never enter the scan. The NOT IN list is the harness's own
    # evidence relations plus the two rule relations, which have their own
    # section.
    cal.saved_query(
        "ledger_conventions", ["ns", "subject"],
        '  ASSEMBLE "learned conventions" FOR "the capture agent" FROM\n'
        '    conventions: (RECALL facts WHERE namespace = $ns AND subject = $subject\n'
        '                  AND relation NOT IN %s\n'
        '                  ORDER BY relation ASC LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE ledger_conventions_tpl\n'
        '  WITH dedup(object)'
        % (_cal_list(sorted(set(RULE_RELATIONS) | INTERNAL_RELATIONS)), SECTION_CAP,
           cal.MAX_BUDGET_TOKENS),
        "conventions learned about the task as a whole"),
    # Arm C: everything the accountant said, oldest first, deduplicated on the
    # exact sentence. `observer_type = "human"` is a store-side filter, not a
    # Python one, so a machine observation can never reach this block.
    cal.saved_query(
        "ledger_said", ["ns"],
        '  ASSEMBLE "what the accountant said" FOR "the capture agent" FROM\n'
        '    said: (RECALL observations WHERE namespace = $ns\n'
        '           AND observer_type = "human"\n'
        '           ORDER BY seq ASC LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE ledger_said_tpl\n'
        '  WITH dedup(object)' % (SECTION_CAP, cal.MAX_BUDGET_TOKENS),
        "the ungoverned baseline's prompt section"),
]


def is_dsn(path):
    """A `postgres://` memory (one schema), as opposed to a file."""
    return str(path).startswith(("postgres://", "postgresql://"))


def redact(path):
    """A memory reference safe to print or record: a DSN loses its password.
    A file path is returned unchanged."""
    s = str(path)
    if not is_dsn(s):
        return s
    return re.sub(r"://([^:/@]+):[^@]*@", r"://\1:***@", s)


def bench_db(default_path, override=None):
    """The memory a harness opens (#200): `--db` / `AREEV_BENCH_DB` when set —
    a file path or a `postgres://…?schema=…` DSN, handed to `areev.Areev`
    verbatim — else the file the harness derives. Unset, every published
    file-backed run is unchanged."""
    return override or os.environ.get("AREEV_BENCH_DB") or default_path


def with_memory(db_path, actor, fn):
    """Open as `actor`, run `fn(db)`, and guarantee the handle is released.

    Deliberately not a context manager: `with open_memory(...) as db` binds the
    handle in the CALLER's frame, so it outlives the block and the next open
    raises STO-E002. Passing a function keeps the only reference inside this
    frame, where deleting it actually drops the refcount to zero.

    `fn` must not return the handle or anything holding it.
    """
    import areev
    db = areev.Areev(db_path, ns=NS, actor=actor)
    try:
        # The prompt sections are saved queries in the FILE, not strings in
        # this module (`../CLAUDE.md`, "Register the read as a saved query").
        # `DEFINE` is a write, so it happens here, on the one path that holds a
        # writable handle -- a held-out arm reading a frozen copy finds the
        # registry already in the file it was handed.
        cal.install(db, REGISTRY, db_path=db_path, ns=NS)
        return fn(db)
    finally:
        del db
        gc.collect()


# CAL caps LIMIT at 1000. A scan that hits it has dropped grains, and the
# prompt built from it is silently missing rules.
CAP = 1000


def _recall(db, where):
    grains = json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s"%s LIMIT %d FORMAT json' % (NS, where, CAP)))["grains"]
    if len(grains) >= CAP:
        raise RuntimeError(
            "memory scan hit the %d-grain cap; the prompt would be missing rules. "
            "Narrow the query." % CAP)
    return grains


def document_facts(db, fields):
    """{seq: {field: value}} for the filed rows, one relation-scoped read per
    field so each stays under the cap up to 1000 documents."""
    rows = {}
    for field in fields:
        for g in _recall(db, ' AND relation = "%s"' % field):
            f = g.get("fields", {})
            m = DOCUMENT_SUBJECT_SEQ.match(f.get("subject") or "")
            val = (f.get("object") or "").strip()
            if m and val:
                rows.setdefault(int(m.group(1)), {})[field] = val
    return rows


def _lessons(db):
    return (_recall(db, ' AND relation = "lesson"')
            + _recall(db, ' AND relation = "fails_with"'))


def lessons_markdown(db, profile=None):
    """The LESSONS section, assembled from live memory on every document.

    Two ASSEMBLE sections, both registered in the file: the approved rules and
    the learned conventions. Reads only what a human approved and applied --
    `lesson` from the LLM leg, `fails_with` from the deterministic one, and an
    approved `fact` proposal on the capture entity (a convention such as
    `date_format = DD/MM/YYYY`; the loop proposes these as readily as it
    proposes rules, and discarding them threw away the model's own answer to
    "what format?" while the same question kept being asked).

    A rolled-back lesson stops rendering -- the templates guard their heading
    on `assembly.grain_count`, so a withdrawn rule set renders to the EMPTY
    STRING and not to a heading announcing rules that are gone. That is what
    makes the paired evaluation causal rather than a flag flip.

    The rules are stated as instructions that OUTRANK the day-one prompt.
    Rendered as mere "rules you have been given", the agent kept returning the
    day-one field alone while its own prompt carried "always capture the
    Category" -- it followed the instruction it was given on day one and read
    the rest as background.
    """
    return cal.block(db, [
        ("ledger_rules", {"ns": NS}, SECTION_CAP),
        ("ledger_conventions", {"ns": NS, "subject": capture_entity(profile)}, SECTION_CAP),
    ])


def corrections_markdown(db):
    """The ungoverned baseline's prompt section: every word the accountant
    said, in the order they said it, with nothing removed.

    This is arm C. It reads the SAME memory arm B does and renders it without
    the loop — no proposal, no review, no supersession, and above all no
    retraction. It is the generous form of the baseline: it gets the complete
    record of corrections, including the ones the governed run's trajectory
    elicited, without paying anything for the governance. If arm B beats it,
    the difference is what proposing, reviewing and retracting are worth; if
    it does not, they are worth nothing and that is the result.

    A store-everything memory system is this arm, not arm A.

    A person repeats themselves; the record keeps every instance and so does
    the prompt, except for byte-identical restatements, which would only be
    padding -- that is `WITH dedup(object)`, which keeps the FIRST occurrence
    and so preserves "oldest first".
    """
    said = cal.section(db, "ledger_said", {"ns": NS}, cap=SECTION_CAP)
    # The retired hand-rolled renderer ended this block with a newline and the
    # agent's prompt was built around that. `cal.section` strips the trailing
    # newlines every renderer adds, so it is put back here rather than being
    # left to a template whose braces cannot carry a trailing blank line.
    return (said + "\n") if said else ""


def record_correction(db, seq, message, corrections, profile=None):
    """What the run leaves in memory: the accountant's words, and the row.

    Recorded the way a production system would, which is not the same as
    recording the diff. An earlier version stored one grain per *correction*
    and the model duly reasoned about the corrections as a dataset — every
    lesson it wrote was advice to whoever was making the corrections, which
    tells a capture agent nothing. The framing of the evidence chose the
    audience of the lesson.

    So: the accountant's message is a human Observation under ONE subject, so
    a rule reads as a rule about the task rather than about one document; and
    the approved values are plain triples — the row that actually goes in the
    ledger, which is both the honest production record and the thing
    vendor-level regularities can be learned from.
    """
    if message:
        db.add("observation", json.dumps({
            "content": message,
            "observer_id": REVIEWER, "observer_type": "human",
            "subject": capture_entity(profile), "seq": seq,
        }), ns=NS)
    for field, value in corrections.items():
        db.add("fact", json.dumps({
            "subject": "document_%04d" % seq, "relation": field,
            "object": str(value),
        }), ns=NS)


def current_rules(db):
    """The rule texts already in force, for the reviewer's dedup check.

    Relation-scoped, like the section that renders them: a whole-namespace
    scan raises past 1000 facts (about 250 documents), and the reviewer
    silently losing its oldest rules would let an already-approved rule be
    approved a second time.
    """
    return [obj for obj in ((g.get("fields", {}).get("object") or "").strip()
                            for g in _lessons(db)) if obj]


def parse_proposal(summary):
    """(kind, text) out of a rendered recommendation summary.

    The engine renders the proposal into the summary as `— record lesson:
    "..."` or `— record fact: rel = "..."`. Only a lesson becomes a rule the
    agent reads, so only a lesson is put to the accountant; anything else is
    turned down with that stated as the reason.
    """
    m = re.search(r'record lesson:\s*"(.*)"\s*$', summary, re.S)
    if m:
        return "lesson", m.group(1).strip()
    m = re.search(r'record fact:\s*(.+)$', summary, re.S)
    if m:
        return "fact", m.group(1).strip()
    return "advisory", summary.strip()


def make_judge(review_cmd):
    """The accountant reading a proposed rule — one model call, no truth access.

    Given the same rubric and the field names a person would have, and nothing
    from the ledger, so it judges a rule the way the accountant does rather
    than by checking the answer.
    """
    if not review_cmd:
        return None
    argv = review_cmd.split()

    def judge(system, rule):
        req = json.dumps({"op": "chat", "temperature": 0, "tools": [],
                          "messages": [{"role": "system", "content": system},
                                       {"role": "user", "content": rule}]})
        p = subprocess.run(argv, input=req.encode(), capture_output=True, timeout=120)
        if p.returncode != 0:
            raise RuntimeError("reviewer failed: %s" % p.stderr.decode()[:200])
        return (json.loads(p.stdout.decode()).get("message") or {}).get("content") or ""

    return judge


def policy_file(db_path, policy, name="loop-policy.json", policy_dir=None):
    """The binding takes the host policy as a FILE (host config lives outside
    the memory, like the CLI's --policy). A JSON string is written beside the
    memory so the run directory records the policy it ran under; a path is
    passed through.

    `name` exists because a later leg writing its own policy into the same
    directory silently overwrites the run's. That happened: the regress leg
    clobbered every run's `loop-policy.json` about twelve minutes after the
    experience phase wrote it, so the file naming the run's policy recorded a
    different one. `run.config.json` was the only honest record. Each leg now
    writes its own file."""
    if policy and policy.lstrip().startswith("{"):
        # Beside the memory for a file. A Postgres memory has no "beside", so
        # the caller's work dir takes it (the run's own record directory).
        if policy_dir is None:
            policy_dir = os.getcwd() if is_dsn(db_path) else os.path.dirname(os.path.abspath(db_path))
        path = os.path.join(policy_dir, name)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(policy)
        return path
    return policy


def review_pending(profile, db_path, ask, judge=None):
    """The accountant's decision on each proposed rule — judged, not applied.

    Applying is the run's `apply` node, under `user:accountant`, after the
    runtime has already refused a self-approval. Splitting the two is what
    makes the decision auditable: this function reads the memory and answers,
    and every answer carries its reason, so the ledger shows what was turned
    down as well as what was taken.

    `ask` is what the run parked on: the pending batch, plus what this
    reviewer already decided in the last 90 days, plus the held-out outcome
    series. The prior decisions are consulted BEFORE the judge is: a rule the
    accountant already declined is declined again with the earlier reason
    rather than put to a fresh model call that might answer differently. The
    engine's own cooldown cannot catch that case — a reworded proposal carries
    a different `dedup_key`.
    """
    pending = (ask or {}).get("pending") or []
    declined = _declined_before(ask)
    in_force = set(acct.normalize_rule(x) for x in
                   with_memory(db_path, REVIEWER, current_rules))
    decisions = []
    for rec in pending:
        kind, text = parse_proposal(rec.get("summary") or "")
        target = rec.get("target_ref") or ""
        earlier = cal.restates_a_decision(text, declined, acct.normalize_rule,
                                          acct._content_words)
        if earlier is not None:
            ok, why = False, ("already declined: %s" % earlier["text"][:110])
        elif kind == "lesson":
            ok, why = acct.review_recommendation(profile, text, in_force, judge)
        elif kind == "fact" and not DOCUMENT_SUBJECT.match(target.rsplit("/", 1)[-1]):
            # A convention learned about the task as a whole. Judged on the
            # same rubric.
            #
            # The discriminator is "is this about ONE document", not "does the
            # subject match a name we picked": the model is never told what the
            # capture entity is called and reasonably invents one (live:
            # `entity:invoice_processing`). Gating on an exact match silently
            # rejected every convention it proposed, which is a fact about this
            # harness rather than about the model.
            ok, why = acct.review_recommendation(profile, text, in_force, judge)
        elif kind == "fact":
            ok, why = False, ("a fact about one document, not a rule for "
                              "future ones (%s)" % target)
        else:
            ok, why = False, "advisory only — asks for no change"
        if ok:
            in_force.add(acct.normalize_rule(text))
        decisions.append({"hash": rec["hash"], "kind": kind, "text": text,
                          "approved": bool(ok), "why": why})
    return any(d["approved"] for d in decisions), decisions


def _declined_before(ask):
    """The proposals this reviewer already turned down, as (text) records.

    Read from the memory by the `bench_review_history` saved query and handed
    over in the ask, so the reviewer never has to open the memory to know what
    it already said.
    """
    out = []
    for earlier in (ask or {}).get("prior") or []:
        if (earlier.get("status") or "").lower() not in ("rejected", "rolled_back"):
            continue
        _kind, text = parse_proposal(earlier.get("summary") or "")
        if text:
            out.append({"text": text, "status": earlier.get("status")})
    return out


def learn(profile, db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False, policy_dir=None):
    """One governed pass, executed as a journaled `areev run`.

    propose (the loop's analyzers) → review (the accountant, a client node the
    run PARKS on) → apply. The runtime refuses a responder equal to the
    principal that triggered the ask, so the Review gate's separation of
    duties is enforced and journaled rather than being this module's promise.

    Returns: pending, applied, rejected, errors, funnel, decisions, run_id.
    `policy` is the host policy JSON (e.g. {"discover_objective":"learner"}).
    """
    return bench_run.learn(
        sys.modules[__name__], db_path, llm_cmd, ground_cmd,
        decide=lambda ask: review_pending(profile, db_path, ask, judge),
        policy=policy, verbose=verbose, full_sweep=full_sweep,
        policy_dir=policy_dir)


def rollback_all(db, because="evaluation arm A: withdrawing the learned rules"):
    """Withdraw every applied recommendation, through the governance path."""
    out = []
    for rec in json.loads(db.recommendations('{"status":"applied"}')):
        try:
            db.rollback_recommendation(rec["hash"], because)
            out.append(rec["hash"][:12])
        except ValueError as e:
            out.append("REFUSED %s" % str(e)[:80])
    return out


def copy_memory(src_db, dst_db):
    """Copy a memory file with its WAL, so an arm mutates its own copy.

    A Postgres memory is a schema, not a file, and cannot be copied from here:
    the callers that need a second memory say plainly what they do instead
    (`evaluate.py` rolls back on the one memory for arm A; snapshots and
    per-pass learner copies are refused)."""
    import shutil
    if is_dsn(src_db) or is_dsn(dst_db):
        raise SystemExit(
            "cannot copy %s: a Postgres memory is a schema, not a file. Provision a "
            "second schema and point AREEV_BENCH_DB at it, or run this step against a "
            "file memory." % redact(src_db))
    for suffix in ("", "-wal"):
        if os.path.exists(src_db + suffix):
            shutil.copy(src_db + suffix, dst_db + suffix)
