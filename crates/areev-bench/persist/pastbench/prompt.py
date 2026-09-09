#!/usr/bin/env python3
"""PAST-Bench's injected memory block, as CAL the memory file carries.

Separate from `areev_backend.py` on purpose: that module imports PAST-Bench,
so it cannot be loaded without the benchmark installed, and the prompt is the
part a reader (or a keyless CI check) most wants to look at. Nothing here
imports PAST-Bench.

THREE of the four injected sections are saved ASSEMBLE queries registered in
the memory, rendered by templates registered beside them, under a stated token
budget. One is not.

Two moved once 1.7.4 landed: **Skills** (#207 made `description` filterable, so
the retired-skill filter is a `WHERE` clause) and **Notes** (#206 made
`valid_to` filter, sort and render, so an expired note is excluded by the query
and a live one carries its own "(until …)" label).

The Notes move is NOT byte-identical, and that was a deliberate call rather
than an oversight — see `NOTES_ORDER_CHANGED` below.

  Earlier sessions  one line per SESSION, whose title is a regex over that
                    session's first event. Text extraction landed (#210) and
                    per-group counts landed (#209), but FIRST-of-group did not,
                    so the row this needs still cannot be projected.

That one stays host-composed in `areev_backend.py`. `AREEV.md` in this
directory records the reasoning, which is the record of why the harness looks
the way it does -- and of what a new bench should not expect to express in CAL.
"""
from __future__ import annotations

import os
import sys

_SCRIPTS = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "scripts")
if _SCRIPTS not in sys.path:
    sys.path.insert(0, _SCRIPTS)

import cal_assemble as cal  # noqa: E402

NS = "desk:persist"

SECTION_CAP = 300
# The CAL ceiling, stated rather than defaulted: these blocks must not lose
# rows, and a binding budget would change published prompt bytes. A drop is no
# longer silent either -- `cal.section` raises on CAL-W017. See
# `cal_assemble.MAX_BUDGET_TOKENS`.
SECTION_BUDGET_TOKENS = cal.MAX_BUDGET_TOKENS
PROFILE_BUDGET_TOKENS = SECTION_BUDGET_TOKENS
RETIRED = "retired"

# The Notes section renders in two runs of lines -- notes and lessons first,
# then every other durable fact as "subject relation: object" -- where the
# retired reader interleaved both shapes in ONE recall-ordered list. CAL orders
# WITHIN a section and a template branches only on truthiness (there is no
# value comparison), so the two shapes cannot share a source. The same grains
# reach the model, in the same per-section order; only the interleaving
# changed. Runs 1-4 precede this and are not comparable on prompt bytes.
NOTES_ORDER_CHANGED = "runs 1-4"

NOTES_HEADING = "### Notes"
PROFILE_HEADING = "### User profile"
SKILLS_HEADING = "### Skills (call skill_view for the steps)"

# An unguarded HEADER plus a `{{^assembly.grain_count}}` FOOTER: the heading
# always renders and an empty section says "(none yet)", which is what the
# assistant has always been shown. (The guarded-heading shape the other tracks
# use is for sections that must VANISH when empty -- a withdrawn rule set.)
NONE_YET = "{{^assembly.grain_count}}- (none yet){{/assembly.grain_count}}"

# `(until …)` is the grain's own validity window, rendered by CAL since #206.
_UNTIL = '{{#if grain.valid_to}} (until {{grain.valid_to | date}}){{/if}}'

REGISTRY = [
    "DEFINE TEMPLATE persist_notes_tpl\n"
    "  HEADER {%s}\n"
    "  ELEMENT {- {{grain.object}}%s}\n"
    "  FOOTER {%s}" % (NOTES_HEADING, _UNTIL, NONE_YET),
    "DEFINE TEMPLATE persist_other_tpl\n"
    "  ELEMENT {- {{grain.subject}} {{grain.relation}}: {{grain.object}}%s}" % _UNTIL,
    "DEFINE TEMPLATE persist_profile_tpl\n"
    "  HEADER {%s}\n"
    "  ELEMENT {- {{grain.object}}}\n"
    "  FOOTER {%s}" % (PROFILE_HEADING, NONE_YET),
    # No ORDER BY: recall order (newest first) is the order this section has
    # always rendered in, and this move is byte-for-byte.
    "DEFINE TEMPLATE persist_skills_tpl\n"
    "  HEADER {%s}\n"
    "  ELEMENT {- {{grain.name}} — {{grain.description}}}\n"
    "  FOOTER {%s}" % (SKILLS_HEADING, NONE_YET),
    # An expired note must not render: a temporary exception lapses on its own,
    # which is what the grain's validity window is for (the loop's `staleness`
    # analyzer proposes the tombstone later). The `IS NULL` leg is
    # load-bearing -- a note with no declared expiry never lapses, and dropping
    # it would return only the notes that DO expire.
    cal.saved_query(
        "persist_notes", ["ns", "now"],
        '  ASSEMBLE "notes" FOR "the assistant" FROM\n'
        '    notes: (RECALL facts WHERE namespace = $ns\n'
        '            AND relation IN ("lesson", "note")\n'
        '            AND (valid_to IS NULL OR valid_to > $now)\n'
        '            LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE persist_notes_tpl' % (SECTION_CAP, SECTION_BUDGET_TOKENS),
        "the assistant's own notes and every approved lesson"),
    # Everything else the loop stored as a durable fact. `session:` and
    # `evalset:` subjects are the harness's own bookkeeping -- a transcript and
    # a score -- and neither is an instruction for the assistant.
    cal.saved_query(
        "persist_other", ["ns", "now"],
        '  ASSEMBLE "other durable facts" FOR "the assistant" FROM\n'
        '    other: (RECALL facts WHERE namespace = $ns\n'
        '            AND relation NOT IN ("%s", "lesson", "mg:eval_run", "note", "profile")\n'
        '            AND NOT subject STARTS WITH "session:"\n'
        '            AND NOT subject STARTS WITH "evalset:"\n'
        '            AND (valid_to IS NULL OR valid_to > $now)\n'
        '            LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE persist_other_tpl' % (RETIRED, SECTION_CAP, SECTION_BUDGET_TOKENS),
        "every other durable fact, as subject relation: object"),
    cal.saved_query(
        "persist_profile", ["ns"],
        '  ASSEMBLE "user profile" FOR "the assistant" FROM\n'
        '    profile: (RECALL facts WHERE namespace = $ns AND relation = "profile" LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE persist_profile_tpl' % (SECTION_CAP, PROFILE_BUDGET_TOKENS),
        "what the assistant has learned about the user"),
    # `description != "retired"` is the filter that had to live in Python
    # until #207 made `description` filterable on skills. `WITH dedup(name)`
    # keeps the first occurrence, which is the newest -- the same row the
    # retired reader's dict kept.
    cal.saved_query(
        "persist_skills", ["ns"],
        '  ASSEMBLE "skills" FOR "the assistant" FROM\n'
        '    skills: (RECALL skills WHERE namespace = $ns\n'
        '             AND description != "%s" LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE persist_skills_tpl\n'
        '  WITH dedup(name)' % (RETIRED, SECTION_CAP, SECTION_BUDGET_TOKENS),
        "the procedures the assistant has written for itself"),
]


def install(db, db_path=None, ns=NS):
    return cal.install(db, REGISTRY, db_path=db_path, ns=ns)


def notes_block(db, now_ms, ns=NS):
    """The Notes section: the assistant's notes and lessons, then every other
    durable fact. Expired entries are excluded by the query and a live one
    carries its own "(until …)" label -- both CAL's since #206."""
    return cal.block(db, [
        ("persist_notes", {"ns": ns, "now": now_ms}, SECTION_CAP),
        ("persist_other", {"ns": ns, "now": now_ms}, SECTION_CAP),
    ], sep="\n")


def empty_notes_block():
    return NOTES_HEADING + "\n- (none yet)"


def profile_block(db, ns=NS):
    return cal.section(db, "persist_profile", {"ns": ns}, cap=SECTION_CAP)


def skills_block(db, ns=NS):
    """The skills section, assembled by CAL since #207.

    `live_skills` is still the reader the in-process memory TOOLS use, because
    they need the hash and the fields; this is the prompt's read, and the two
    select the same set -- a retired skill renders in neither.
    """
    return cal.section(db, "persist_skills", {"ns": ns}, cap=SECTION_CAP)


def empty_profile_block():
    return PROFILE_HEADING + "\n- (none yet)"


def empty_skills_block():
    return SKILLS_HEADING + "\n- (none yet)"
