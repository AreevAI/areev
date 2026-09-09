#!/usr/bin/env python3
"""The byte-parity gate: CAL's assembly == the renderer it replaced.

    python3 scripts/parity_check.py            # all harnesses
    python3 scripts/parity_check.py receipts   # one

Every harness in this crate moved its prompt blocks from hand-rolled
`RECALL`-and-join to `ASSEMBLE` saved queries registered in the memory file
(`../CLAUDE.md`, "Use the product's own surfaces"). Rule 4 of that document is
that a harness is never changed after seeing its result -- so the move had to
leave the prompt bytes untouched, and "it does" is a claim about output, not
about intent.

This file holds the RETIRED renderers, verbatim, as the reference
implementation. It seeds a memory with the grains each harness actually
writes, renders the block both ways, and asserts the strings are equal. When
they diverge, it prints both.

The retired renderers exist ONLY here. Nothing imports them, and no arm can
run under them -- keeping a live fallback would be the parallel system this
change exists to remove.

Keyless: no model, no network, no API key.
"""
from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
if HERE not in sys.path:
    sys.path.insert(0, HERE)


def harness(track, name="memory"):
    """Load `<track>/<name>.py` under a unique module name.

    Every harness calls its bridge `memory.py`, so a plain import binds
    whichever directory happens to be first on the path -- which silently
    compared one track's block against another track's renderer.
    """
    import importlib.util

    key = "areev_bench_%s_%s" % (track, name)
    if key in sys.modules:
        return sys.modules[key]
    path = os.path.join(ROOT, track)
    if path not in sys.path:
        sys.path.insert(0, path)
    spec = importlib.util.spec_from_file_location(key, os.path.join(path, name + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[key] = module
    spec.loader.exec_module(module)
    return module

FAILURES: list[str] = []


def check(name, got, want):
    if got == want:
        print("  ok   " + name)
        return
    FAILURES.append(name)
    print("  FAIL " + name)
    print("       CAL  : " + repr(got))
    print("       retired: " + repr(want))


def fresh(root, name):
    path = os.path.join(root, name)
    for suffix in ("", "-wal", ".blobs"):
        p = path + suffix
        if os.path.isdir(p):
            shutil.rmtree(p)
        elif os.path.exists(p):
            os.remove(p)
    return path


# ==========================================================================
# receipts
# ==========================================================================

def retired_lessons_markdown(db, mem, profile=None):
    """`receipts/memory.py::lessons_markdown`, as it stood before the move."""
    def recall(where):
        return json.loads(db.cal(
            'RECALL facts WHERE namespace = "%s"%s LIMIT %d FORMAT json'
            % (mem.NS, where, 1000)))["grains"]

    rules, conventions = [], []
    lessons = recall(' AND relation = "lesson"') + recall(' AND relation = "fails_with"')
    for g in lessons:
        obj = (g.get("fields", {}).get("object") or "").strip()
        if obj:
            rules.append(obj)
    entity = mem.capture_entity(profile)
    for g in recall(' AND subject = "%s"' % entity):
        f = g.get("fields", {})
        rel, obj = f.get("relation"), (f.get("object") or "").strip()
        if not obj or rel in ("lesson", "fails_with"):
            continue
        if rel not in mem.INTERNAL_RELATIONS:
            conventions.append("%s: %s" % (rel.replace("_", " "), obj))

    parts = []
    if rules:
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


def retired_corrections_markdown(db, mem):
    """`receipts/memory.py::corrections_markdown`, as it stood before the move."""
    grains = json.loads(db.cal(
        'RECALL observations WHERE namespace = "%s" LIMIT %d FORMAT json'
        % (mem.NS, 1000)))["grains"]
    said = []
    for g in grains:
        f = g.get("fields", {})
        if f.get("observer_type") != "human":
            continue
        text = (f.get("object") or "").strip()
        if text:
            said.append((int(f.get("seq") or 0), text))
    said.sort()
    if not said:
        return ""
    seen, lines = set(), []
    for _seq, text in said:
        if text not in seen:
            seen.add(text)
            lines.append("- " + text)
    return ("## WHAT THE ACCOUNTANT HAS TOLD YOU\n"
            "Everything the person who files these has said, oldest first.\n"
            + "\n".join(lines) + "\n")


def receipts_case(root):
    mem = harness("receipts")

    print("\n=== receipts")
    path = fresh(root, "ledger.db")

    def seed(db):
        # Rules arrive newest-last, so recall order is the REVERSE of
        # alphabetical: a check that passed under either order would prove
        # nothing about the ORDER BY.
        for rule in ["park anything with no total",
                     "always capture the Category field",
                     "the vendor name is the top line, not the footer"]:
            db.add("fact", json.dumps({"subject": "receipt_capture",
                                       "relation": "lesson", "object": rule}), ns=mem.NS)
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "fails_with",
                                   "object": "totals written 1.234,56 parse as 1.234"}), ns=mem.NS)
        # A duplicate rule: `WITH dedup(object)` is the retired `set(...)`.
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "lesson",
                                   "object": "park anything with no total"}), ns=mem.NS)
        # Conventions on the capture entity, and one INTERNAL relation that
        # must never render.
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "date_format",
                                   "object": "DD/MM/YYYY"}), ns=mem.NS)
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "currency_hint",
                                   "object": "RM is Malaysian ringgit"}), ns=mem.NS)
        db.add("fact", json.dumps({"subject": "receipt_capture", "relation": "capture_attempt",
                                   "object": "seq 3 parked"}), ns=mem.NS)
        # Per-document facts, which the conventions section is subject-scoped
        # to exclude.
        db.add("fact", json.dumps({"subject": "document_0007", "relation": "total",
                                   "object": "18.90"}), ns=mem.NS)
        # Arm C's observations, with a repeat and one machine observation.
        for i, txt in enumerate(["you missed the Category again",
                                 "dates here are DD/MM/YYYY",
                                 "you missed the Category again",
                                 "park it when there is no total"]):
            db.add("observation", json.dumps({
                "content": txt, "observer_id": mem.REVIEWER, "observer_type": "human",
                "subject": "receipt_capture", "seq": i}), ns=mem.NS)
        db.add("observation", json.dumps({
            "content": "extraction confidence 0.4", "observer_id": mem.RUNNER,
            "observer_type": "agent", "subject": "receipt_capture", "seq": 99}), ns=mem.NS)

    mem.with_memory(path, mem.RUNNER, seed)

    def compare(db):
        check("receipts: LESSONS block",
              mem.lessons_markdown(db), retired_lessons_markdown(db, mem))
        check("receipts: arm C corrections block",
              mem.corrections_markdown(db), retired_corrections_markdown(db, mem))
        print("       block:\n" + "\n".join("       | " + l
                                            for l in mem.lessons_markdown(db).split("\n")))

    mem.with_memory(path, mem.REVIEWER, compare)

    # The empty case is the one arm A depends on: a memory with no rules must
    # render to the EMPTY STRING, not to a heading announcing rules that are
    # not there.
    empty = fresh(root, "empty.db")
    mem.with_memory(empty, mem.RUNNER, lambda db: db.add(
        "fact", json.dumps({"subject": "document_0001", "relation": "total", "object": "1.00"}),
        ns=mem.NS))

    def empty_case(db):
        check("receipts: a memory with no rules renders empty", mem.lessons_markdown(db), "")
        check("receipts: a memory with nothing said renders empty", mem.corrections_markdown(db), "")

    mem.with_memory(empty, mem.RUNNER, empty_case)

    # The registry is IN the file: that is the whole point of a saved query.
    def registry_travels(db):
        names = __import__("cal_assemble").installed_queries(db)
        check("receipts: the saved queries travel with the file",
              {"ledger_rules", "ledger_conventions", "ledger_said"} <= names, True)

    copy = fresh(root, "copied.db")
    mem.copy_memory(path, copy)
    mem.with_memory(copy, mem.RUNNER, registry_travels)


# ==========================================================================
# tau2
# ==========================================================================

def retired_tau2_lessons(db, mem):
    """`tau2/memory.py::lessons_markdown`, as it stood before the move.

    Note the merged sort: rule texts and "relation: object" conventions went
    into ONE alphabetical list. CAL orders within a section, not across
    sections, so the replacement renders them as two runs of lines. The
    check below therefore compares the SET and the per-section order, and
    `tau2/README.md` records the interleaving change.
    """
    grains = json.loads(db.cal('RECALL facts WHERE namespace = "%s" LIMIT 400 FORMAT json'
                               % mem.NS))["grains"]
    rules = []
    for g in grains:
        f = g.get("fields", {})
        rel, obj = f.get("relation"), (f.get("object") or "").strip()
        if not obj:
            continue
        if rel in ("lesson", "fails_with"):
            rules.append(obj)
        elif (not mem.EPISODE_SUBJECT.match(f.get("subject") or "")
              and rel not in mem.INTERNAL_RELATIONS):
            rules.append("%s: %s" % (rel.replace("_", " "), obj))
    return "\n".join("- %s" % r for r in sorted(set(rules)))


def tau2_case(root):
    mem = harness("tau2")

    print("\n=== tau2")
    path = fresh(root, "retail.db")

    def seed(db):
        for rule in ["always confirm the order id before cancelling",
                     "read the policy before promising an exception"]:
            db.add("fact", json.dumps({"subject": mem.DESK, "relation": "lesson",
                                       "object": rule}), ns=mem.NS)
        db.add("fact", json.dumps({"subject": mem.DESK, "relation": "fails_with",
                                   "object": "modify_pending_order rejects a delivered order"}),
               ns=mem.NS)
        db.add("fact", json.dumps({"subject": mem.DESK, "relation": "refund_window",
                                   "object": "30 days from delivery"}), ns=mem.NS)
        # Evidence, not instruction: an episode's own record must never render.
        db.add("fact", json.dumps({"subject": "episode_task_17", "relation": "outcome",
                                   "object": json.dumps({"accepted": False})}), ns=mem.NS)
        db.add("fact", json.dumps({"subject": "episode_task_17", "relation": "note",
                                   "object": "this customer was in a hurry"}), ns=mem.NS)

    mem.with_memory(path, mem.RUNNER, seed)

    def compare(db):
        got, want = mem.lessons_markdown(db), retired_tau2_lessons(db, mem)
        check("tau2: the same lines reach the model",
              sorted(got.split("\n")), sorted(want.split("\n")))
        check("tau2: an episode's own record never renders",
              "this customer was in a hurry" in got, False)
        check("tau2: rules are alphabetical within their section",
              got.split("\n")[:3],
              sorted(["- always confirm the order id before cancelling",
                      "- modify_pending_order rejects a delivered order",
                      "- read the policy before promising an exception"]))
        print("       block:\n" + "\n".join("       | " + l for l in got.split("\n")))

    mem.with_memory(path, mem.REVIEWER, compare)

    def empty_case(db):
        check("tau2: a memory with no rules renders empty", mem.lessons_markdown(db), "")

    mem.with_memory(fresh(root, "retail-empty.db"), mem.RUNNER, empty_case)

    # The tool-call lifecycle: input, correlation id, status and failure cause
    # survive, which the flattened `add("tool", …)` discarded.
    def calls(db):
        mem.record_episode(db, {"task_id": "task_17", "reward": 0.0, "transcript": [],
                                "tools_called": ["get_order"], "tool_errors": 1,
                                "terminated": "agent", "steps": 4},
                           [{"tool": "get_order", "args": {"order_id": "W123"},
                             "id": "call_a", "result": "not found", "error": "not found"}])

    def inspect(db):
        rows = json.loads(db.cal(
            'RECALL tools WHERE namespace = "%s" LIMIT 20 FORMAT json' % mem.NS))["grains"]
        f = rows[0].get("fields", {}) if rows else {}
        # `input` round-trips as parsed JSON, not as the string handed in.
        got = f.get("input")
        check("tau2: the call's input survives",
              json.loads(got) if isinstance(got, str) else got, {"order_id": "W123"})
        # The store projects the call id as `tool_call_id`.
        check("tau2: the call/result id survives", f.get("tool_call_id"), "call_a")
        check("tau2: the call is marked failed", f.get("status"), "failed")
        check("tau2: the failure cause is the store's enum",
              f.get("failure_cause"), "executor_error")
        check("tau2: the refusal text is the result", f.get("tool_content"), "not found")

    tools = fresh(root, "retail-calls.db")
    mem.with_memory(tools, mem.RUNNER, calls)
    mem.with_memory(tools, mem.RUNNER, inspect)


# ==========================================================================
# appworld
# ==========================================================================

def retired_appworld_lessons(db, mem):
    """`appworld/memory.py::lessons_block`, as it stood before the move."""
    grains = json.loads(db.cal('RECALL facts WHERE namespace = "%s" LIMIT 400 FORMAT json'
                               % mem.NS))["grains"]
    rules = [f["object"] for f in (g.get("fields", {}) for g in grains)
             if f.get("relation") in ("lesson", "fails_with") and f.get("object")]
    rules = sorted(set(r.strip() for r in rules if r and r.strip()))
    if not rules:
        return ""
    return "%s\n%s" % (mem._GOVERNED_HEADER, "\n".join("- %s" % r for r in rules))


def appworld_case(root):
    mem = harness("appworld")

    print("\n=== appworld")
    path = fresh(root, "appworld.db")

    def seed(db):
        for rule in ["send the email before marking the task done",
                     "always page through phone contacts; the first page is not all of them"]:
            db.add("fact", json.dumps({"subject": "appworld_agent", "relation": "lesson",
                                       "object": rule}), ns=mem.NS)
        # Errors, into the app namespaces the APPWORLD.md defect asked for.
        mem.record_episode(db, "task_1", [
            {"app": "phone", "api": "search_contacts", "kind": "http_401",
             "message": "Response status code is 401: token expired"},
            {"app": "phone", "api": "search_contacts", "kind": "http_401",
             "message": "Response status code is 401: token expired"},
            {"app": "spotify", "api": "show_playlist", "kind": "http_404",
             "message": "Response status code is 404: no such playlist"},
            {"app": "", "api": "", "kind": "TypeError",
             "message": "TypeError: unhashable type"},
        ], steps=7, hit_cap=False)

    mem.with_memory(path, mem.RUNNER, seed)

    def compare(db):
        check("appworld: governed block",
              mem.lessons_block(db), retired_appworld_lessons(db, mem))
        passive = mem.experience_block(db)
        # The block moved into CAL (#209, #217) and ranks what it always
        # ranked: `(endpoint, message)` pairs, most frequent first. Asserted
        # as the shape rather than as byte parity with the retired renderer,
        # because the ordering contract is the engine's now.
        check("appworld: the passive block ranks (endpoint, message) by frequency",
              passive.split("\n")[1],
              "- (2x) phone.search_contacts: Response status code is 401: token expired")
        check("appworld: an unattributed error still reaches the block",
              "- (1x) unknown: TypeError: unhashable type" in passive, True)
        print("       passive:\n" + "\n".join("       | " + l for l in passive.split("\n")))

    mem.with_memory(path, mem.REVIEWER, compare)

    def namespaces(db):
        # The point of the child namespaces: a task about the phone can recall
        # the phone's evidence, and `"appworld.*"` still gets all of it.
        phone = json.loads(db.cal(
            'RECALL tools WHERE namespace = "appworld.phone" LIMIT 50 FORMAT json'))["grains"]
        every = json.loads(db.cal(
            'RECALL tools WHERE namespace = "appworld.*" LIMIT 50 FORMAT json'))["grains"]
        check("appworld: phone evidence lives in appworld.phone", len(phone), 2)
        check("appworld: the prefix scope still sees every app", len(every), 4)
        check("appworld: an unattributed app stays in the base namespace",
              len(json.loads(db.cal(
                  'RECALL tools WHERE namespace = "appworld" LIMIT 50 FORMAT json'))["grains"]), 1)

    mem.with_memory(path, mem.RUNNER, namespaces)

    def empty_case(db):
        check("appworld: a memory with no rules renders empty", mem.lessons_block(db), "")
        check("appworld: a memory with no errors renders empty", mem.experience_block(db), "")

    mem.with_memory(fresh(root, "appworld-empty.db"), mem.RUNNER, empty_case)

    # -- the budget regression -------------------------------------------
    #
    # `ASSEMBLE` applies a token budget whether or not you ask for one — the
    # default is 4000 — and a budget that binds DROPS GRAINS SILENTLY: no
    # warning, and `total_available` reports the POST-budget count, so a
    # caller cannot tell a full answer from a truncated one.
    #
    # This is not hypothetical. Wrapping AppWorld's error SELECTION in an
    # ASSEMBLE returned 79 of 229 grains on run 1's own memory, and the small
    # seed above was far too small to notice. So: seed past the default and
    # assert nothing is lost. Every section that renders many rows needs a
    # check like this one.
    big = fresh(root, "appworld-big.db")

    def seed_many(db):
        errors = [{"app": "phone", "api": "api_%03d" % i, "kind": "http_401",
                   "message": "Response status code is 401: token expired on endpoint %03d "
                              "with a message long enough to cost real tokens" % i}
                  for i in range(200)]
        # One endpoint fails repeatedly, and it sorts LAST by key — so it can
        # only lead the block if the ranking really is by frequency.
        errors += [errors[199]] * 4
        mem.record_episode(db, "task_big", errors, steps=200, hit_cap=False)

    mem.with_memory(big, mem.RUNNER, seed_many)

    def all_rows(db):
        return mem.experience_block(db)

    block = mem.with_memory(big, mem.RUNNER, all_rows)
    # The cut is the engine's `LIMIT` after `COUNT` (#217), applied to the
    # ranking before the budget ever sees it — so this asserts a bound the
    # block ASKED for, not whatever the default budget happened to leave. The
    # silent-drop case the comment above describes is still covered, by the
    # 200-rule check below and by `cal.section` raising on `CAL-W017`.
    check("appworld: the ranking is cut at the top N, not by the budget",
          block.count("\n- "), mem.PASSIVE_TOP_N)
    check("appworld: the most frequent pair leads the ranking",
          block.split("\n")[1].split(":")[0], "- (5x) phone.api_199")

    def many_rules(db):
        for i in range(200):
            db.add("fact", json.dumps({
                "subject": "appworld_agent", "relation": "lesson",
                "object": "rule %03d: a learned instruction long enough that two "
                          "hundred of them cost well past four thousand tokens" % i}),
                ns=mem.NS)
        return mem.lessons_block(db)

    rules_block = mem.with_memory(big, mem.RUNNER, many_rules)
    check("appworld: 200 approved rules all reach the prompt",
          rules_block.count("\n- "), 200)

    # And when a budget DOES bind, the read refuses rather than quietly
    # handing back a short prompt. Since 1.7.4 the engine says so (CAL-W017,
    # #208) and `cal.raise_on_truncation` turns that into an exception —
    # before it, this was the failure that returned 79 of 229 grains in
    # silence. The ceiling is not a guarantee of no trimming, only a stated
    # bound, which is exactly why the warning is checked.
    ca = __import__("cal_assemble")
    raised = {}

    def squeeze(db):
        db.cal('DEFINE QUERY "tiny" AS { ASSEMBLE "r" FROM '
               'r: (RECALL facts WHERE namespace = "appworld" '
               'AND relation = "lesson" LIMIT 400) '
               'BUDGET 60 tokens FORMAT TEMPLATE appworld_rules_tpl }')
        try:
            ca.section(db, "tiny")
        except RuntimeError as exc:
            raised["why"] = str(exc)

    mem.with_memory(big, mem.RUNNER, squeeze)
    check("appworld: a budget that binds raises instead of shortening the prompt",
          "CAL-W017" in (raised.get("why") or ""), True)

    # A frozen arm reads a copy read-only: the saved queries must have
    # travelled with the file, because a read-only handle cannot install them.
    frozen = fresh(root, "appworld-frozen.db")
    mem.copy_memory(path, frozen)
    check("appworld: a frozen read-only arm still assembles its block",
          mem.block_for(frozen, "governed", read_only=True),
          retired_appworld_lessons.__doc__ and mem.with_memory(
              path, mem.REVIEWER, lambda db: retired_appworld_lessons(db, mem)))


# ==========================================================================
# persist
# ==========================================================================

def persist_case(root):
    """`persist/pastbench/prompt.py` -- the two injected sections CAL owns.

    Imports `prompt.py`, not `areev_backend.py`: the backend imports
    PAST-Bench, which is not a dependency of this repo, and the prompt is
    exactly the part that should be checkable without it.
    """
    import areev

    sys.path.insert(0, os.path.join(ROOT, "persist", "pastbench"))
    import prompt  # noqa: PLC0415

    print("\n=== persist")
    path = fresh(root, "persist.db")
    db = areev.Areev(path, ns=prompt.NS, actor="agent:assistant")
    try:
        prompt.install(db, db_path=path)
        empty_now = int(__import__("time").time() * 1000)
        check("persist: an empty memory still shows all three headings",
              (prompt.notes_block(db, empty_now), prompt.profile_block(db),
               prompt.skills_block(db)),
              (prompt.empty_notes_block(), prompt.empty_profile_block(),
               prompt.empty_skills_block()))

        db.add("fact", json.dumps({"subject": "user", "relation": "profile",
                                   "object": "prefers bullet summaries"}), ns=prompt.NS)
        # Notes: a live one, an expired one that must NOT render, and one with
        # a declared expiry still in force that must carry its own label.
        now = int(__import__("time").time() * 1000)
        day = 86_400_000
        db.add("fact", json.dumps({"subject": "assistant", "relation": "note",
                                   "object": "the Q3 waiver has lapsed",
                                   "valid_to": now - day}), ns=prompt.NS)
        db.add("fact", json.dumps({"subject": "assistant", "relation": "lesson",
                                   "object": "confirm the vendor before paying",
                                   "valid_to": now + day}), ns=prompt.NS)
        # A durable fact that is neither: renders as "subject relation: object".
        db.add("fact", json.dumps({"subject": "vendor", "relation": "sla",
                                   "object": "48 hours"}), ns=prompt.NS)
        # The harness's own bookkeeping, which must never reach the prompt.
        db.add("fact", json.dumps({"subject": "session:s1", "relation": "title",
                                   "object": "an earlier session"}), ns=prompt.NS)
        db.add("fact", json.dumps({"subject": "evalset:h", "relation": "mg:eval_run",
                                   "object": json.dumps({"passed": 3})}), ns=prompt.NS)
        db.add("fact", json.dumps({"subject": "user", "relation": "profile",
                                   "object": "is in Chennai, UTC+5:30"}), ns=prompt.NS)
        # A fact that is NOT a profile entry must not reach the profile block.
        db.add("fact", json.dumps({"subject": "assistant", "relation": "note",
                                   "object": "escalate refunds over 5k"}), ns=prompt.NS)

        # Recall order is newest-first, and neither query orders -- that is
        # the order these sections have always rendered in.
        check("persist: the profile block",
              prompt.profile_block(db),
              prompt.PROFILE_HEADING + "\n- is in Chennai, UTC+5:30\n- prefers bullet summaries")
        check("persist: a note is not a profile entry",
              "escalate refunds" in prompt.profile_block(db), False)
        # The skills section is CAL again since #207 made `description`
        # filterable on skills. The retired skill must not render, and this is
        # the check that would have caught the silent version: before #207,
        # pushing the filter down on `object` matched every row without
        # warning.
        db.add("skill", json.dumps({"name": "close_month",
                                    "description": "the month-end close",
                                    "instructions": "1. reconcile"}), ns=prompt.NS)
        db.add("skill", json.dumps({"name": "old_flow", "description": prompt.RETIRED,
                                    "instructions": ""}), ns=prompt.NS)
        check("persist: the skills block",
              prompt.skills_block(db),
              prompt.SKILLS_HEADING + "\n- close_month — the month-end close")
        check("persist: a retired skill does not render",
              "old_flow" in prompt.skills_block(db), False)

        # Notes moved into CAL with #206. NOT byte-identical: the two line
        # shapes are two sections now, where the retired reader interleaved
        # them in one recall-ordered list. Asserted as the new shape.
        notes = prompt.notes_block(db, now)
        check("persist: an expired note does not render",
              "lapsed" in notes, False)
        check("persist: a live note with an expiry carries its label",
              "confirm the vendor before paying (until " in notes, True)
        check("persist: a durable fact renders as subject relation: object",
              "- vendor sla: 48 hours" in notes, True)
        check("persist: session and evalset bookkeeping never render",
              ("session:" in notes) or ("evalset" in notes), False)
        check("persist: an empty notes section still shows the heading",
              prompt.notes_block(db, now).startswith(prompt.NOTES_HEADING), True)
        print("       block:\n" + "\n".join(
            "       | " + l for l in (prompt.notes_block(db, now) + "\n"
                                      + prompt.profile_block(db) + "\n"
                                      + prompt.skills_block(db)).split("\n")))
    finally:
        del db
        __import__("gc").collect()


# ==========================================================================
# persist/horizon
# ==========================================================================

def retired_horizon_block(lessons):
    """`persist/horizon/areev_agent/agent.py`, as it stood before the move."""
    if not lessons:
        return ""
    return ("Learned from the earlier sessions (proposed by review of the memory, approved "
            "by a reviewer) — apply these without being asked:\n"
            + "\n".join("  - " + x for x in lessons) + "\n\n")


def horizon_case(root):
    """Imports `agent.py` only for its REGISTRY and readers.

    That module imports `harbor`, which is not a dependency of this repo, so
    the prompt functions are exercised against a real memory here without
    loading it: the registry statements and the two readers are re-created
    from the module's source text via a namespace that stubs the imports.
    """
    import areev

    src_path = os.path.join(ROOT, "persist", "horizon", "areev_agent", "agent.py")
    if not os.path.exists(src_path):
        print("\n=== persist/horizon  (not present, skipped)")
        return

    print("\n=== persist/horizon")
    # Pull just the block this check needs, rather than importing `harbor`.
    ns_globals = {"__name__": "horizon_prompt_probe"}
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import cal_assemble as ca  # noqa: PLC0415

    src = open(src_path, encoding="utf-8").read()
    start = src.index("SECTION_CAP = 500")
    end = src.index("REGISTRY = [")
    tail = src.index("]", src.index("everything else the loop stored as a durable fact")) + 1
    exec(compile(src[start:tail], src_path, "exec"), {"cal": ca, **ns_globals}, ns_globals)  # noqa: S102

    NS = "desk:horizon"
    SECTION_CAP = ns_globals["SECTION_CAP"]
    HEADER = ns_globals["LESSONS_HEADER"]

    path = fresh(root, "horizon.db")
    db = areev.Areev(path, ns=NS, actor="agent:assistant")
    try:
        ca.install(db, ns_globals["REGISTRY"], db_path=path, ns=NS)
        check("horizon: an empty memory contributes nothing to the prompt",
              ca.block(db, [("horizon_lessons", {"ns": NS}, SECTION_CAP),
                            ("horizon_other", {"ns": NS}, SECTION_CAP)], sep="\n"), "")

        db.add("fact", json.dumps({"subject": "assistant", "relation": "lesson",
                                   "object": "search the earlier sessions before acting"}), ns=NS)
        db.add("fact", json.dumps({"subject": "assistant", "relation": "note",
                                   "object": "the ops handoff lives in session 3"}), ns=NS)
        db.add("fact", json.dumps({"subject": "vendor", "relation": "sla",
                                   "object": "48 hours"}), ns=NS)
        # An eval score must never render into the agent's own prompt.
        db.add("fact", json.dumps({"subject": "evalset:h", "relation": "mg:eval_run",
                                   "object": json.dumps({"passed": 3})}), ns=NS)

        body = ca.block(db, [("horizon_lessons", {"ns": NS}, SECTION_CAP),
                             ("horizon_other", {"ns": NS}, SECTION_CAP)], sep="\n")
        got = (HEADER + "\n" + body + "\n\n") if body else ""
        want = retired_horizon_block([
            "the ops handoff lives in session 3",
            "search the earlier sessions before acting",
            "vendor sla: 48 hours",
        ])
        # Recall order is newest-first and the retired reader did not sort, so
        # compare the line SET plus the framing.
        check("horizon: the same lines reach the model",
              sorted(l for l in got.split("\n") if l.strip()),
              sorted(l for l in want.split("\n") if l.strip()))
        check("horizon: an eval score never renders", "passed" in got, False)
        check("horizon: the header and trailing blank line are preserved",
              got.startswith(HEADER + "\n") and got.endswith("\n\n"), True)
        print("       block:\n" + "\n".join("       | " + l for l in got.rstrip().split("\n")))
    finally:
        del db
        __import__("gc").collect()


# ==========================================================================
# the host seam: modules this repo cannot import, checked statically
# ==========================================================================

def host_contract_case(root):
    """Do the harness modules still match the prompt API they call?

    `persist/pastbench/areev_backend.py` imports PAST-Bench and
    `persist/horizon/areev_agent/agent.py` imports `harbor`; neither is a
    dependency of this repo, so nothing here can import them and a rename or
    an arity change in `prompt.py` would be found only by a paid run on
    somebody else's machine. That is the most expensive place to find it.

    So: parse them, resolve what they import from the prompt module, and check
    every call site's arity against the real signature. Not a substitute for
    running the benchmark — it cannot catch a wrong VALUE — but it does catch
    the class of break this crate's own refactors keep creating.
    """
    import ast
    import inspect

    print("\n=== host seam (static)")
    sys.path.insert(0, os.path.join(ROOT, "persist", "pastbench"))
    import prompt  # noqa: PLC0415

    src = open(os.path.join(ROOT, "persist", "pastbench", "areev_backend.py"),
               encoding="utf-8").read()
    tree = ast.parse(src)

    imported = []
    for n in ast.walk(tree):
        if isinstance(n, ast.ImportFrom) and n.module == "prompt":
            imported += [(a.name, a.asname) for a in n.names]
    check("persist: the backend imports names prompt.py actually defines",
          [nm for nm, _ in imported if not hasattr(prompt, nm)], [])

    alias = {(asn or nm): nm for nm, asn in imported}
    problems = []
    for n in ast.walk(tree):
        if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id in alias:
            fn = getattr(prompt, alias[n.func.id])
            try:
                sig = inspect.signature(fn)
            except (TypeError, ValueError):
                continue
            required = sum(1 for prm in sig.parameters.values()
                           if prm.default is inspect._empty
                           and prm.kind in (prm.POSITIONAL_ONLY, prm.POSITIONAL_OR_KEYWORD))
            given = len(n.args) + len(n.keywords)
            if given < required or given > len(sig.parameters):
                problems.append("%s line %d: %d args vs %s" % (n.func.id, n.lineno, given, sig))
    check("persist: every backend call site matches the prompt signature",
          problems, [])
    print("       checked %d imported names, %d call sites"
          % (len(imported), sum(1 for n in ast.walk(tree)
                                if isinstance(n, ast.Call)
                                and isinstance(n.func, ast.Name)
                                and n.func.id in alias)))

    # horizon defines its readers in the same module it uses them from, so the
    # check there is that the module still parses and still defines them.
    hz = os.path.join(ROOT, "persist", "horizon", "areev_agent", "agent.py")
    if os.path.exists(hz):
        htree = ast.parse(open(hz, encoding="utf-8").read())
        defined = {n.name for n in ast.walk(htree) if isinstance(n, ast.FunctionDef)}
        check("horizon: the prompt readers are still defined",
              {"lessons_block", "live_lessons", "_with_memory"} <= defined, True)


CASES = {"receipts": receipts_case, "tau2": tau2_case, "appworld": appworld_case,
         "persist": persist_case, "horizon": horizon_case,
         "host-contract": host_contract_case}


def main(argv):
    want = argv[1:] or sorted(CASES)
    root = tempfile.mkdtemp(prefix="areev-bench-parity-")
    try:
        for name in want:
            if name not in CASES:
                raise SystemExit("unknown harness %r; have %s" % (name, ", ".join(sorted(CASES))))
            CASES[name](root)
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print()
    if FAILURES:
        print("FAILED: %d check(s): %s" % (len(FAILURES), ", ".join(FAILURES)))
        return 1
    print("byte parity holds: every block CAL assembles is the block the "
          "retired renderer produced.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
