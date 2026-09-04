#!/usr/bin/env python3
"""The Areev bridge for the τ² retail agent.

What goes into memory is what a deployed agent would actually have: the tool
calls it made (errors flagged), one outcome record per episode, and — when
the customer pushed back in their own words — that sentence, as a human
observation. What comes out is the LESSONS block, assembled from live grains
on every episode, so apply/rollback is the only lever on behaviour.

Deliberately NOT recorded: the task's gold actions, the reward, or which
withheld clause an episode needed. Those name the answer. The outcome record
carries only the episode's observable shape plus whether it was accepted —
the same contract the synthetic bench's episodes use.
"""
import gc
import json
import os
import re
import subprocess

NS = "retail"
RUNNER = "agent:retail-desk"
REVIEWER = "user:supervisor"
DESK = "retail_desk"
HARNESS_NS = "agent:harness"

INTERNAL_RELATIONS = {"episode", "outcome"}
EPISODE_SUBJECT = re.compile(r"^episode_\S+$")


def with_memory(db_path, actor, fn):
    """Open as `actor`, run `fn(db)`, and guarantee the handle is released.

    Not a context manager on purpose: `with open(...) as db` binds the handle
    in the caller's frame, where it outlives the block and the next open
    fails STO-E002.
    """
    import areev
    db = areev.Areev(db_path, ns=NS, actor=actor)
    try:
        return fn(db)
    finally:
        del db
        gc.collect()


def _facts(db, ns=NS, limit=400):
    return json.loads(db.cal('RECALL facts WHERE namespace = "%s" LIMIT %d FORMAT json'
                             % (ns, limit)))["grains"]


def lessons_markdown(db):
    """The LESSONS block, from live grains, on every episode."""
    rules = []
    for g in _facts(db):
        f = g.get("fields", {})
        rel, obj = f.get("relation"), (f.get("object") or "").strip()
        if not obj:
            continue
        if rel in ("lesson", "fails_with"):
            rules.append(obj)
        elif (not EPISODE_SUBJECT.match(f.get("subject") or "")
              and rel not in INTERNAL_RELATIONS):
            rules.append("%s: %s" % (rel.replace("_", " "), obj))
    return "\n".join("- %s" % r for r in sorted(set(rules)))


def record_episode(db, rec, calls):
    """One episode's experience: the tool calls, the customer's pushback, and
    the outcome record."""
    for c in calls:
        if not (c.get("tool") or "").strip():
            continue  # a nameless call is a malformed reply, not an action
        body = json.dumps(c.get("error") and {"error": c["error"]} or c.get("args") or {})
        db.add("tool", json.dumps({
            "tool_name": c["tool"],
            "is_error": bool(c.get("error")),
            "content": (c.get("error") or body)[:600],
        }), ns=NS)

    # The customer's own words, when they are a complaint rather than a
    # request. One sentence a person said is the rarest and most valuable
    # evidence a memory holds, and it is stated once by nature.
    for who, text in rec.get("transcript", []):
        if who != "customer":
            continue
        t = text.strip()
        if len(t) < 15 or not _is_pushback(t):
            continue
        db.add("observation", json.dumps({
            "content": t[:600], "observer_id": "customer",
            "observer_type": "human", "subject": DESK, "task": rec["task_id"],
        }), ns=NS)

    # The outcome record: the episode's observable shape plus accepted/rejected.
    # Never the gold actions, never the reward's reason, never the rule.
    db.add("fact", json.dumps({
        "subject": "episode_%s" % rec["task_id"],
        "relation": "outcome",
        "object": json.dumps({
            "accepted": bool(rec.get("reward", 0) >= 1.0),
            "tools_called": rec.get("tools_called", []),
            "tool_errors": rec.get("tool_errors", 0),
            "ended": rec.get("terminated", ""),
            "turns": rec.get("steps", 0),
        }),
    }), ns=NS)


_PUSHBACK = re.compile(
    r"\b(that'?s not|didn'?t|did not|wrong|still|but i|i asked|no,|actually|"
    r"you (haven'?t|have not|need to|should)|why (is|are|did|can'?t)|"
    r"can'?t you|isn'?t (that|this)|i already)\b", re.I)


def _is_pushback(text):
    return bool(_PUSHBACK.search(text))


def policy_file(db_path, policy):
    """The binding takes host policy as a file; a JSON string is written
    beside the memory so the run directory records what it ran under."""
    if policy and policy.lstrip().startswith("{"):
        path = os.path.join(os.path.dirname(os.path.abspath(db_path)), "loop-policy.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(policy)
        return path
    return policy


def make_judge(review_cmd):
    """The supervisor reading a proposed rule: one model call, on a fixed
    rubric, with no access to the tasks, the gold actions or the reward."""
    if not review_cmd:
        return None
    argv = review_cmd.split()

    def judge(system, rule):
        req = json.dumps({"op": "chat", "temperature": 0, "tools": [],
                          "messages": [{"role": "system", "content": system},
                                       {"role": "user", "content": rule}]})
        p = subprocess.run(argv, input=req.encode(), capture_output=True, timeout=180)
        if p.returncode != 0:
            raise RuntimeError("reviewer failed: %s" % p.stderr.decode()[:200])
        return (json.loads(p.stdout.decode()).get("message") or {}).get("content") or ""

    return judge


REVIEW_RUBRIC = """You supervise a retail customer-service agent. It handles one customer per conversation, using tools to look up and change orders.

The agent has proposed a RULE for itself to follow in all FUTURE conversations, based on how earlier ones went. You decide whether to adopt it.

Approve the rule only if BOTH hold:
  (a) it tells the AGENT something to do, or to do differently, in a future conversation — a step to take, an order to take steps in, or a check to run before acting; and
  (b) it would be correct for a retail desk: it must not invent store policy that could harm a customer (waiving charges, promising refunds outside the normal methods, skipping a confirmation), and it must not contradict what the tools actually do.

Reject the rule if any of these hold:
  - it is about ONE customer, ONE order or ONE conversation instead of stating a general practice;
  - it restates something the agent obviously already does, or is too vague to change any behaviour ("be careful", "handle errors properly", "verify everything");
  - it tells someone other than the agent what to do;
  - it would produce a wrong or harmful action.

Answer with JSON only: {"approve": true|false, "reason": "<one short sentence, addressed to the agent>"}"""


def normalize_rule(text):
    return re.sub(r"[^a-z0-9 ]", "", (text or "").lower())


_STOP = set("a an the to of in on for and or is are be as at by with all any every each "
            "from into that this it its not no do does use using when if then than must "
            "should always never ensure make sure before after even they them their there "
            "which who whom you your".split())


def _content_words(norm):
    return {w for w in norm.split() if w not in _STOP and len(w) > 2}


def _too_similar(norm, approved, threshold=0.6):
    mine = _content_words(norm)
    if not mine:
        return None
    for other in approved:
        theirs = _content_words(other)
        if theirs and len(mine & theirs) / len(mine | theirs) >= threshold:
            return other
    return None


def review_recommendation(text, already_approved, judge=None):
    t = (text or "").strip()
    if not t:
        return False, "empty rule"
    norm = normalize_rule(t)
    if norm in already_approved:
        return False, "already in force"
    near = _too_similar(norm, already_approved)
    if near is not None:
        return False, "restates a rule already in force: %s" % near[:70]
    if judge is None:
        return False, "no reviewer available to approve this"
    raw = judge(REVIEW_RUBRIC, t)
    try:
        verdict = json.loads(raw[raw.index("{"):raw.rindex("}") + 1])
    except (ValueError, json.JSONDecodeError):
        return False, "reviewer returned no usable verdict"
    return bool(verdict.get("approve")), (verdict.get("reason") or "").strip()[:200] or "reviewed"


def parse_proposal(summary):
    m = re.search(r'record lesson:\s*"(.*)"\s*$', summary, re.S)
    if m:
        return "lesson", m.group(1).strip()
    m = re.search(r'record fact:\s*(.+)$', summary, re.S)
    if m:
        return "fact", m.group(1).strip()
    return "advisory", summary.strip()


def current_rules(db):
    return [f["object"] for f in (g.get("fields", {}) for g in _facts(db))
            if f.get("relation") in ("lesson", "fails_with") and f.get("object")]


def learn(db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False):
    """One governed pass: propose under the runner, decide under the supervisor."""
    policy = policy_file(db_path, policy)
    rep = json.loads(with_memory(
        db_path, RUNNER,
        lambda db: db.loop_run(llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=policy,
                               full_sweep=full_sweep)))
    out = {"pending": 0, "applied": 0, "rejected": 0, "errors": [],
           "funnel": rep.get("llm_funnel"), "decisions": []}
    if verbose and out["funnel"]:
        print("   funnel:", json.dumps(out["funnel"]))

    def review(rdb):
        in_force = set(normalize_rule(x) for x in current_rules(rdb))
        pend = json.loads(rdb.recommendations('{"status":"pending"}'))
        out["pending"] = len(pend)
        for rec in pend:
            kind, text = parse_proposal(rec.get("summary") or "")
            target = rec.get("target_ref") or ""
            if kind in ("lesson", "fact") and not EPISODE_SUBJECT.match(target.rsplit("/", 1)[-1]):
                ok, why = review_recommendation(text, in_force, judge)
            elif kind == "fact":
                ok, why = False, "about one episode, not a rule for future ones (%s)" % target
            else:
                ok, why = False, "advisory only — asks for no change"
            try:
                if ok:
                    rdb.apply_recommendation(rec["hash"], why)
                    in_force.add(normalize_rule(text))
                    out["applied"] += 1
                    if verbose:
                        print("   APPROVED %s" % text[:110])
                else:
                    rdb.dismiss_recommendation(rec["hash"], why)
                    out["rejected"] += 1
                    if verbose:
                        print("   rejected (%s) %s" % (why[:40], text[:60]))
                out["decisions"].append({"hash": rec["hash"], "kind": kind, "text": text,
                                         "approved": bool(ok), "why": why})
            except ValueError as e:
                out["errors"].append(str(e)[:120])

    with_memory(db_path, REVIEWER, review)
    return out


def rollback_all(db, because="evaluation arm A: withdrawing the learned rules"):
    out = []
    for rec in json.loads(db.recommendations('{"status":"applied"}')):
        try:
            db.rollback_recommendation(rec["hash"], because)
            out.append(rec["hash"][:12])
        except ValueError as e:
            out.append("REFUSED %s" % str(e)[:80])
    return out


def copy_memory(src_db, dst_db):
    import shutil
    for suffix in ("", "-wal"):
        if os.path.exists(src_db + suffix):
            shutil.copy(src_db + suffix, dst_db + suffix)


def journal_eval_run(db_path, evalset, run_id, records, note=None, at_ms=None):
    """A held-out pass recorded as an evalset run the Verify gate can read."""
    passed = sum(1 for r in records if r.get("reward", 0) >= 1.0)
    summary = {"run_id": run_id, "passed": passed, "failed": len(records) - passed,
               "total": len(records), "solved": passed}
    if note:
        summary["note"] = note

    def write(db):
        fields = {"subject": "evalset:%s" % evalset, "relation": "mg:eval_run",
                  "object": json.dumps(summary), "run_id": run_id}
        if at_ms is not None:
            fields["created_at"] = int(at_ms)
        db.add("fact", json.dumps(fields), ns=HARNESS_NS)
        return summary

    return with_memory(db_path, REVIEWER, write)
