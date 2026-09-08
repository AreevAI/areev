#!/usr/bin/env python3
"""The Areev bridge for the AppWorld coding agent.

What goes into memory is what a deployed agent could actually know: the code
it ran, the API errors the environment handed back, and one outcome record
per episode. What comes out is a block appended to the agent's prompt --
either the raw experience (the passive arm) or the approved rules (the
governed arm) -- so what is in that block is the only lever on behaviour.

Deliberately NOT recorded: the task's ground-truth solution, its evaluation
code, or its score. Those name the answer. AppWorld hides the unit tests
from the agent and so does this. The score reaches memory in exactly one
place -- `journal_eval_run`, written by the HARNESS under `agent:harness`
after a held-out pass -- because the Verify gate has to read an outcome
series, and an agent that could read its own grade would be a different
experiment.
"""
from __future__ import annotations

import gc
import json
import os
import re
import subprocess

NS = "appworld"
RUNNER = "agent:appworld"
REVIEWER = "user:supervisor"
HARNESS_NS = "agent:harness"

EPISODE_SUBJECT = re.compile(r"^episode_\S+$")
INTERNAL_RELATIONS = {"episode", "outcome"}

# The environment states its own failures in a stable shape; these are the
# ones worth remembering, and they are matched rather than guessed at.
_EXCEPTION = re.compile(r"^Exception: (.+)$", re.M)
_API_CALL = re.compile(r"apis\.([a-z_]+)\.([a-z_]+)\s*\(")
# The environment names the offending endpoint in the message itself for
# parameter errors, and echoes the failing source line in the traceback for
# everything else. Both are better evidence than the last call in the chunk.
_NAMED_IN_MESSAGE = re.compile(r"passed to the ([a-z_]+) API of the ([a-z_]+) app")
_TRACEBACK_LINE = re.compile(r'^\s+File "<python-input>".*?$(.*)', re.M | re.S)


def with_memory(db_path, actor, fn, read_only=False):
    """Open as `actor`, run `fn(db)`, and guarantee the handle is released.

    Not a context manager on purpose: `with open(...) as db` binds the handle
    in the caller's frame, where it outlives the block and the next open
    fails STO-E002 (the embedded backend is single-writer per file).

    `read_only=True` is how a held-out arm reads the memory it is being
    evaluated on: every write is refused with STO-E004, so the memory under
    test is frozen by the store rather than by our promising not to write.
    """
    import areev

    # The store will not create the directory it is asked to live in, and the
    # arms' memories are addressed as <workdir>/<arm>/<phase>.db -- so without
    # this the first episode of every arm dies with STO-E001.
    parent = os.path.dirname(os.path.abspath(db_path))
    if parent:
        os.makedirs(parent, exist_ok=True)
    db = areev.Areev(db_path, ns=NS, actor=actor, read_only=read_only)
    try:
        return fn(db)
    finally:
        del db
        gc.collect()


def _facts(db, ns=NS, limit=400):
    return json.loads(
        db.cal('RECALL facts WHERE namespace = "%s" LIMIT %d FORMAT json' % (ns, limit))
    )["grains"]


# --------------------------------------------------------------------------
# writing: one episode's experience
# --------------------------------------------------------------------------


def summarize_error(code: str, output: str) -> dict | None:
    """Turn one failed interaction into the smallest fact that could teach.

    Returns None for an interaction that did not fail -- a successful call is
    not evidence of anything the agent needs to change.
    """
    message = _EXCEPTION.search(output or "")
    if not message:
        return None
    text = message.group(1).strip()

    # Attribution, best evidence first. A code chunk routinely calls several
    # APIs, so the last one in it is often NOT the one that failed -- that
    # mis-attribution silently teaches a rule about the wrong endpoint.
    named = _NAMED_IN_MESSAGE.search(output or "")
    if named:
        api, app = named.group(1), named.group(2)
    else:
        traceback = _TRACEBACK_LINE.search(output or "")
        calls = _API_CALL.findall(traceback.group(1)) if traceback else []
        if not calls:
            calls = _API_CALL.findall(code or "")
        app, api = calls[-1] if calls else ("", "")
    status = re.match(r"Response status code is (\d+)", text)
    return {
        "app": app,
        "api": api,
        "kind": ("http_%s" % status.group(1)) if status else text.split(":")[0][:40],
        "message": text[:400],
    }


def record_episode(db, task_id: str, errors: list[dict], steps: int, hit_cap: bool) -> None:
    """The episode as the agent experienced it: what broke, and how it ended."""
    for e in errors:
        name = ("%s.%s" % (e["app"], e["api"])).strip(".") or "unknown"
        db.add(
            "tool",
            json.dumps({"tool_name": name, "is_error": True, "content": e["message"]}),
            ns=NS,
        )

    kinds: dict[str, int] = {}
    for e in errors:
        kinds[e["kind"]] = kinds.get(e["kind"], 0) + 1

    db.add(
        "fact",
        json.dumps(
            {
                "subject": "episode_%s" % task_id,
                "relation": "outcome",
                "object": json.dumps(
                    {
                        "api_errors": len(errors),
                        "error_kinds": kinds,
                        "apps_touched": sorted({e["app"] for e in errors if e["app"]}),
                        "steps": steps,
                        "ended": "max_steps" if hit_cap else "stopped",
                    }
                ),
            }
        ),
        ns=NS,
    )


# --------------------------------------------------------------------------
# reading: the block that goes into the prompt
# --------------------------------------------------------------------------

_PASSIVE_HEADER = (
    "E. What went wrong in earlier tasks (your own past API errors, most "
    "frequent first). These are raw records, not instructions:"
)
_GOVERNED_HEADER = (
    "E. Rules you have learned from earlier tasks and that a supervisor has "
    "approved. Follow them:"
)


def experience_block(db, limit: int = 12) -> str:
    """The PASSIVE arm's block: the agent's own errors, deduplicated.

    No rule is inferred and nothing is approved -- this is the honest form of
    "just put the past in the prompt", which is the baseline a governed loop
    has to beat to have earned anything.
    """
    counts: dict[str, int] = {}
    grains = json.loads(
        db.cal('RECALL tools WHERE namespace = "%s" LIMIT 400 FORMAT json' % NS)
    )["grains"]
    for g in grains:
        f = g.get("fields", {})
        if not f.get("is_error"):
            continue
        # The store projects a Tool grain's body as `tool_content`; `content`
        # is what it was written under. Reading only the latter silently
        # produced blocks of bare API names with no error text at all.
        body = (f.get("tool_content") or f.get("content") or "").strip()
        line = "%s: %s" % (f.get("tool_name") or "?", body)
        counts[line] = counts.get(line, 0) + 1
    if not counts:
        return ""
    top = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:limit]
    body = "\n".join("- (%dx) %s" % (n, line[:300]) for line, n in top)
    return "%s\n%s" % (_PASSIVE_HEADER, body)


def current_rules(db) -> list[str]:
    return [
        f["object"]
        for f in (g.get("fields", {}) for g in _facts(db))
        if f.get("relation") in ("lesson", "fails_with") and f.get("object")
    ]


def lessons_block(db) -> str:
    """The GOVERNED arm's block: approved rules only, nothing else."""
    rules = sorted(set(r.strip() for r in current_rules(db) if r and r.strip()))
    if not rules:
        return ""
    return "%s\n%s" % (_GOVERNED_HEADER, "\n".join("- %s" % r for r in rules))


def block_for(db_path: str, mode: str, read_only: bool = False) -> str:
    """The one function the agent calls. `none` reads nothing at all."""
    if mode == "none":
        return ""
    reader = {"passive": experience_block, "governed": lessons_block}[mode]
    return with_memory(db_path, RUNNER, reader, read_only=read_only)


# --------------------------------------------------------------------------
# the governed pass
# --------------------------------------------------------------------------


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
    rubric, with no access to the tasks, the solutions or the scores."""
    if not review_cmd:
        return None
    argv = review_cmd.split()

    def judge(system, rule):
        req = json.dumps(
            {
                "op": "chat",
                "temperature": 0,
                "tools": [],
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": rule},
                ],
            }
        )
        p = subprocess.run(argv, input=req.encode(), capture_output=True, timeout=180)
        if p.returncode != 0:
            raise RuntimeError("reviewer failed: %s" % p.stderr.decode()[:200])
        return (json.loads(p.stdout.decode()).get("message") or {}).get("content") or ""

    return judge


REVIEW_RUBRIC = """You supervise an agent that completes everyday tasks by writing Python that calls the APIs of nine apps (email, phone, Venmo, Spotify, Amazon, Todoist, SimpleNote, Splitwise, a file system) on behalf of one user.

The agent has proposed a RULE for itself to follow in all FUTURE tasks, based on how earlier ones went. You decide whether to adopt it.

Approve the rule only if BOTH hold:
  (a) it tells the AGENT something to do, or to do differently, in a future task -- a step to take, an order to take steps in, or a check to run before acting; and
  (b) it would be correct for these apps: it must not contradict what the APIs actually do, and it must not invent an endpoint, a parameter or a value.

Reject the rule if any of these hold:
  - it is about ONE task, ONE app-specific value or ONE conversation instead of stating a general practice;
  - it restates something the agent obviously already does, or is too vague to change any behaviour ("be careful", "handle errors properly", "check everything");
  - it hard-codes a credential, a token, an id or an answer from a previous task;
  - it tells someone other than the agent what to do;
  - it would produce a wrong or destructive action.

Answer with JSON only: {"approve": true|false, "reason": "<one short sentence, addressed to the agent>"}"""


def normalize_rule(text):
    return re.sub(r"[^a-z0-9 ]", "", (text or "").lower())


_STOP = set(
    "a an the to of in on for and or is are be as at by with all any every each "
    "from into that this it its not no do does use using when if then than must "
    "should always never ensure make sure before after even they them their there "
    "which who whom you your".split()
)


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
        verdict = json.loads(raw[raw.index("{") : raw.rindex("}") + 1])
    except (ValueError, json.JSONDecodeError):
        return False, "reviewer returned no usable verdict"
    return bool(verdict.get("approve")), (verdict.get("reason") or "").strip()[:200] or "reviewed"


def parse_proposal(summary):
    m = re.search(r'record lesson:\s*"(.*)"\s*$', summary, re.S)
    if m:
        return "lesson", m.group(1).strip()
    m = re.search(r"record fact:\s*(.+)$", summary, re.S)
    if m:
        return "fact", m.group(1).strip()
    return "advisory", summary.strip()


def learn(db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False):
    """One governed pass: propose under the runner, decide under the supervisor."""
    policy = policy_file(db_path, policy)
    rep = json.loads(
        with_memory(
            db_path,
            RUNNER,
            lambda db: db.loop_run(
                llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=policy, full_sweep=full_sweep
            ),
        )
    )
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
                ok, why = False, "advisory only -- asks for no change"
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
    """A held-out pass recorded as an evalset run the Verify gate can read.

    Written under `agent:harness`, never under the agent -- this is the one
    place a score touches the memory, and it is the harness that puts it there.
    """
    passed = sum(1 for r in records if r.get("success"))
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
