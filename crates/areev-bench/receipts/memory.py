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

import accountant as acct

NS = "ledger"
RUNNER = "agent:receipt-capture"
REVIEWER = "user:accountant"
CAPTURE_ENTITY = "receipt_capture"

# Relations this harness writes itself. They are evidence for the loop, never
# instructions for the agent, so they must never render into the prompt.
INTERNAL_RELATIONS = {"reading_result", "correction", "capture_attempt"}

# The subjects this harness writes per document (`document_0007`). A proposal
# targeting one of these is about a single receipt, not a rule for future ones.
DOCUMENT_SUBJECT = re.compile(r"^document_\d+$")


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
        return fn(db)
    finally:
        del db
        gc.collect()


def _facts(db):
    return json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s" LIMIT 300 FORMAT json' % NS))["grains"]


def lessons_markdown(db):
    """The LESSONS section, assembled from live memory on every document.

    Reads only what a human approved and applied: rules recorded as Facts
    (`lesson` from the LLM leg, `fails_with` from the deterministic one).
    A rolled-back lesson stops rendering, which is what makes the paired
    evaluation causal rather than a flag flip.
    """
    rules, conventions = [], []
    for g in _facts(db):
        f = g.get("fields", {})
        rel, obj = f.get("relation"), (f.get("object") or "").strip()
        if not obj:
            continue
        if rel in ("lesson", "fails_with"):
            rules.append(obj)
        elif (not DOCUMENT_SUBJECT.match(f.get("subject") or "")
              and rel not in INTERNAL_RELATIONS):
            # An approved `fact` proposal on the capture entity — a learned
            # convention (date_format = DD/MM/YYYY). The loop proposes these
            # as readily as it proposes rules; discarding them threw away the
            # model's own answer to "what format?" while the same question
            # kept being asked.
            conventions.append("%s: %s" % (rel.replace("_", " "), obj))

    parts = []
    if rules:
        # Stated as instructions that OUTRANK the day-one prompt, because an
        # approved rule that cannot change behaviour breaks the whole chain.
        # Rendered as mere "rules you have been given", the agent kept
        # returning the day-one field alone while its own prompt carried
        # "always capture the Category" — it followed the instruction it was
        # given on day one and read the rest as background.
        parts.append("## INSTRUCTIONS FROM THE ACCOUNTANT\n"
                     "These come from the person who files these documents and "
                     "they OVERRIDE the day-one instruction above. If a rule "
                     "names a field to capture, that field is REQUIRED: put it "
                     "in your JSON, in addition to the day-one field.\n"
                     + "\n".join("- %s" % o for o in sorted(set(rules))))
    if conventions:
        parts.append("## CONVENTIONS\n"
                     + "\n".join("- %s" % o for o in sorted(set(conventions))))
    return "\n\n".join(parts)


def record_correction(db, seq, message, corrections):
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
            "subject": CAPTURE_ENTITY, "seq": seq,
        }), ns=NS)
    for field, value in corrections.items():
        db.add("fact", json.dumps({
            "subject": "document_%04d" % seq, "relation": field,
            "object": str(value),
        }), ns=NS)


def current_rules(db):
    """The rule texts already in force, for the reviewer's dedup check."""
    return [f["object"] for f in (g.get("fields", {}) for g in _facts(db))
            if f.get("relation") in ("lesson", "fails_with") and f.get("object")]


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


def policy_file(db_path, policy):
    """The binding takes the host policy as a FILE (host config lives outside
    the memory, like the CLI's --policy). A JSON string is written beside the
    memory so the run directory records the policy it ran under; a path is
    passed through."""
    if policy and policy.lstrip().startswith("{"):
        path = os.path.join(os.path.dirname(os.path.abspath(db_path)), "loop-policy.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(policy)
        return path
    return policy


def learn(profile, db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False):
    """One governed pass: propose under the runner, decide under the reviewer.

    Returns a dict: pending, applied, rejected, errors, funnel, decisions.
    Applying under a different actor is the Review gate's separation of
    duties — the identity that triggered the finding cannot approve it.
    `policy` is the host policy JSON (e.g. {"discover_objective":"learner"}).
    """
    policy = policy_file(db_path, policy)
    rep = json.loads(with_memory(
        db_path, RUNNER,
        lambda db: db.loop_run(llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=policy,
                               full_sweep=full_sweep)))
    funnel = rep.get("llm_funnel")
    if verbose and funnel:
        print("   funnel:", json.dumps(funnel))

    out = {"pending": 0, "applied": 0, "rejected": 0, "errors": [],
           "funnel": funnel, "decisions": []}

    def review(rdb):
        """The human gate: the accountant decides, one rule at a time.

        Every decision is recorded with its reason — an approve through
        `apply_recommendation`, a reject through `dismiss_recommendation` —
        so the ledger shows what was turned down as well as what was taken.
        """
        in_force = set(acct.normalize_rule(x) for x in current_rules(rdb))
        pend = json.loads(rdb.recommendations('{"status":"pending"}'))
        out["pending"] = len(pend)
        for rec in pend:
            kind, text = parse_proposal(rec.get("summary") or "")
            target = rec.get("target_ref") or ""
            if kind == "lesson":
                ok, why = acct.review_recommendation(profile, text, in_force, judge)
            elif kind == "fact" and not DOCUMENT_SUBJECT.match(target.rsplit("/", 1)[-1]):
                # A convention learned about the task as a whole. Judged on
                # the same rubric.
                #
                # The discriminator is "is this about ONE document", not "does
                # the subject match a name we picked": the model is never told
                # what the capture entity is called and reasonably invents one
                # (live: `entity:invoice_processing`). Gating on an exact match
                # silently rejected every convention it proposed, which is a
                # fact about this harness rather than about the model.
                ok, why = acct.review_recommendation(profile, text, in_force, judge)
            elif kind == "fact":
                ok, why = False, ("a fact about one document, not a rule for "
                                  "future ones (%s)" % target)
            else:
                ok, why = False, "advisory only — asks for no change"
            try:
                if ok:
                    rdb.apply_recommendation(rec["hash"], why)
                    in_force.add(acct.normalize_rule(text))
                    out["applied"] += 1
                    if verbose:
                        print("   APPROVED %s" % text[:100])
                else:
                    rdb.dismiss_recommendation(rec["hash"], why)
                    out["rejected"] += 1
                    if verbose:
                        print("   rejected (%s) %s" % (why[:44], (text or "")[:52]))
                out["decisions"].append({"hash": rec["hash"], "kind": kind,
                                         "text": text, "approved": bool(ok), "why": why})
            except ValueError as e:
                out["errors"].append(str(e)[:110])

    with_memory(db_path, REVIEWER, review)
    if verbose:
        for r in out["errors"]:
            print("   ERROR:", r)
    return out


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
    """Copy a memory file with its WAL, so an arm mutates its own copy."""
    import shutil
    for suffix in ("", "-wal"):
        if os.path.exists(src_db + suffix):
            shutil.copy(src_db + suffix, dst_db + suffix)
