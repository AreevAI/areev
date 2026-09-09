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
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "scripts"))

import bench_run
import cal_assemble as cal

TRACK = "appworld"   # the directory `bench_govern.py --harness` loads
NS = "appworld"
RUNNER = "agent:appworld"
REVIEWER = "user:supervisor"
HARNESS_NS = "agent:harness"

# The domain has nine parts, so the memory has nine child namespaces. A phone
# error is written to `appworld.phone`; a read that wants the whole domain
# asks for `"appworld.*"`, which selects the base namespace AND its
# descendants -- so a memory written flat (every run before 2026-09-08) still
# reads correctly through the same query.
#
# This is the defect APPWORLD.md records: the gate approved a rule that scoped
# itself to `phone.*` and it then sat in an undifferentiated pile, where the
# scope it named meant nothing at recall time. Now it means a namespace.
NS_SCOPE = "appworld.*"
APPS = ("amazon", "file_system", "gmail", "phone", "simple_note", "spotify",
        "splitwise", "todoist", "venmo")


def ns_for(app):
    """The namespace one app's evidence belongs in.

    An unrecognised app (the error parser could not attribute the call) goes to
    the base namespace rather than minting a namespace from unvalidated text --
    a write is what MINTS a namespace, so a typo there is accepted by every
    surface and found by none.
    """
    app = (app or "").strip()
    return "%s.%s" % (NS, app) if app in APPS else NS


EPISODE_SUBJECT = re.compile(r"^episode_\S+$")
INTERNAL_RELATIONS = {"episode", "outcome"}

# --------------------------------------------------------------------------
# the prompt, as CAL the file carries
# --------------------------------------------------------------------------

SECTION_CAP = 400

# How many (endpoint, message) pairs the passive block lists. The harness
# tallied and cut at twelve before the block moved into CAL; the cut is the
# engine's again since #217 made a `LIMIT` after `COUNT` bind.
PASSIVE_TOP_N = 12

_PASSIVE_HEADER = (
    "E. What went wrong in earlier tasks (your own past API errors, most "
    "frequent first). These are raw records, not instructions:"
)
_GOVERNED_HEADER = (
    "E. Rules you have learned from earlier tasks and that a supervisor has "
    "approved. Follow them:"
)

REGISTRY = list(cal.REVIEW_REGISTRY) + [
    cal.guarded_template("appworld_rules_tpl", _GOVERNED_HEADER, "- {{grain.object}}"),
    cal.saved_query(
        "appworld_rules", ["scope"],
        '  ASSEMBLE "approved rules" FOR "the AppWorld coding agent" FROM\n'
        '    rules: (RECALL facts WHERE namespace = $scope\n'
        '            AND relation IN ("fails_with", "lesson")\n'
        '            ORDER BY object ASC LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE appworld_rules_tpl\n'
        '  WITH dedup(object)' % (SECTION_CAP, cal.MAX_BUDGET_TOKENS),
        "the supervisor-approved rules, the governed arm's whole prompt block"),
    # The passive block, ranked by frequency -- `GROUP BY <keys> COUNT` since
    # #209 landed in 1.7.4. `group.*` renders the ranking; the guarded header
    # means an agent with no errors yet sees nothing at all.
    #
    # The key is COMPOSITE (#217): `(endpoint, message)`, which is the ranking
    # the harness used to tally in Python and the one runs 1 and 2 were
    # produced under. Ranking endpoints alone tells the agent where it fails
    # and not what to do about it -- and this is the BASELINE arm, so a weaker
    # block here flatters the governed arm it is compared against.
    cal.guarded_template("appworld_errors_tpl", _PASSIVE_HEADER,
                         "- ({{group.count}}x) {{group.key.0}}: {{group.key.1}}"),
    cal.saved_query(
        "appworld_errors", ["scope"],
        '  ASSEMBLE "past API errors" FOR "the AppWorld coding agent" FROM\n'
        '    ranked: (RECALL tools WHERE namespace = $scope AND is_error = true\n'
        '             LIMIT %d GROUP BY tool_name, tool_content COUNT LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE appworld_errors_tpl'
        % (SECTION_CAP, PASSIVE_TOP_N, cal.MAX_BUDGET_TOKENS),
        "every endpoint this agent has failed on and what it failed with, "
        "most frequent first"),
]

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
        # `DEFINE` is a write. A frozen arm cannot install anything (STO-E004)
        # and does not need to: it reads a COPY of a memory the runner wrote,
        # and the saved queries travelled with the file.
        if not read_only:
            cal.install(db, REGISTRY, db_path=db_path, ns=NS)
        return fn(db)
    finally:
        del db
        gc.collect()


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
    """The episode as the agent experienced it: what broke, and how it ended.

    Each failure is written as a CALL, into the namespace of the app whose API
    it was: `record_tool_call` keeps the arguments, the call/result join, the
    status and the failure cause, which is what a later
    `areev_tool_provenance` or `step_actions` read needs and what the
    flattened `add("tool", …)` this replaces threw away.
    """
    for e in errors:
        name = ("%s.%s" % (e["app"], e["api"])).strip(".") or "unknown"
        db.record_tool_call(
            name,
            e["message"],
            True,
            thread=task_id,
            input=json.dumps({"app": e["app"], "api": e["api"], "kind": e["kind"]}),
            status="failed",
            # `failure_cause` is a closed enum (timeout, executor_error,
            # schema_validation_failed, user_aborted, unknown). Everything
            # AppWorld's environment hands back -- an HTTP status, a raised
            # exception -- is the executor failing, so the harness's own
            # taxonomy (`http_401`, `TypeError`) rides in `input` where it
            # stays queryable instead of being forced into a field that
            # cannot hold it.
            failure_cause="executor_error",
            executor_kind="host",
            ns=ns_for(e["app"]),
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

def experience_block(db) -> str:
    """The PASSIVE arm's block: the agent's own failures, most frequent first.

    No rule is inferred and nothing is approved -- this is the honest form of
    "just put the past in the prompt", which is the baseline a governed loop
    has to beat to have earned anything.

    Assembled entirely by CAL since #209 landed `GROUP BY … COUNT` and the
    `group.*` template variables, and it ranks what it has always ranked:
    (endpoint, message) pairs, most frequent first, cut at the top twelve.

    Restoring the message needed #217 -- a composite group key, a Tool body
    that a template can render, and a `LIMIT` after `COUNT` that binds. For
    one day (2026-09-09) the block ranked endpoints only, which `AREEV.md`
    records: this is the BASELINE arm, and a weaker block here flatters the
    governed arm it is compared against, so the direction of any change to it
    is worth more than its size.
    """
    return cal.section(db, "appworld_errors", {"scope": NS_SCOPE})


def current_rules(db) -> list[str]:
    """The rule texts already in force, for the supervisor's dedup check.

    Scoped to `"appworld.*"` and to the two rule relations, so it sees exactly
    what `lessons_block` renders -- a reviewer reading a narrower set than the
    agent does would approve a rule already in the prompt.
    """
    grains = json.loads(db.cal(
        'RECALL facts WHERE namespace = "%s" AND relation IN ("fails_with", "lesson") '
        'LIMIT %d FORMAT json' % (NS_SCOPE, SECTION_CAP)))["grains"]
    if len(grains) >= SECTION_CAP:
        raise RuntimeError("rule scan hit the %d-grain cap; narrow the query" % SECTION_CAP)
    return [obj for obj in ((g.get("fields", {}).get("object") or "").strip()
                            for g in grains) if obj]


def lessons_block(db) -> str:
    """The GOVERNED arm's block: approved rules only, nothing else.

    One ASSEMBLE, registered in the file, scoped across every app namespace.
    The heading is guarded on `assembly.grain_count`, so an arm whose rules
    were rolled back renders nothing at all rather than a heading announcing
    rules that are gone.
    """
    return cal.section(db, "appworld_rules", {"scope": NS_SCOPE}, cap=SECTION_CAP)


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


def review_pending(db_path, ask, judge=None):
    """The supervisor's decision on each proposed rule — judged, not applied.

    Applying is the run's `apply` node, under `user:supervisor`, after the
    runtime has refused a self-approval.
    """
    pending = (ask or {}).get("pending") or []
    declined = _declined_before(ask)
    in_force = set(normalize_rule(x) for x in
                   with_memory(db_path, REVIEWER, current_rules))
    decisions = []
    for rec in pending:
        kind, text = parse_proposal(rec.get("summary") or "")
        target = rec.get("target_ref") or ""
        earlier = cal.restates_a_decision(text, declined, normalize_rule, _content_words)
        if earlier is not None:
            ok, why = False, ("already declined: %s" % earlier["text"][:110])
        elif kind in ("lesson", "fact") and not EPISODE_SUBJECT.match(target.rsplit("/", 1)[-1]):
            ok, why = review_recommendation(text, in_force, judge)
        elif kind == "fact":
            ok, why = False, "about one episode, not a rule for future ones (%s)" % target
        else:
            ok, why = False, "advisory only -- asks for no change"
        if ok:
            in_force.add(normalize_rule(text))
        decisions.append({"hash": rec["hash"], "kind": kind, "text": text,
                          "approved": bool(ok), "why": why})
    return any(d["approved"] for d in decisions), decisions


def _declined_before(ask):
    """The proposals this reviewer already turned down.

    Read by the `bench_review_history` saved query and handed over in the ask.
    Consulted BEFORE the judge: a reworded restatement of a declined rule
    carries a different `dedup_key`, so the engine's rejection cooldown never
    sees it and nothing else was looking.
    """
    out = []
    for earlier in (ask or {}).get("prior") or []:
        if (earlier.get("status") or "").lower() not in ("rejected", "rolled_back"):
            continue
        _kind, text = parse_proposal(earlier.get("summary") or "")
        if text:
            out.append({"text": text, "status": earlier.get("status")})
    return out


def learn(db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False):
    """One governed pass, executed as a journaled `areev run`:
    propose → review (a client node the run PARKS on) → apply."""
    return bench_run.learn(
        sys.modules[__name__], db_path, llm_cmd, ground_cmd,
        decide=lambda ask: review_pending(db_path, ask, judge),
        policy=policy, verbose=verbose, full_sweep=full_sweep)


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
