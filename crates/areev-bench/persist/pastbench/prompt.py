#!/usr/bin/env python3
"""PAST-Bench's injected memory block, as CAL the memory file carries.

Separate from `areev_backend.py` on purpose: that module imports PAST-Bench,
so it cannot be loaded without the benchmark installed, and the prompt is the
part a reader (or a keyless CI check) most wants to look at. Nothing here
imports PAST-Bench.

TWO of the four injected sections are saved ASSEMBLE queries registered in the
memory, rendered by templates registered beside them, under a stated token
budget. Two are not, and the reasons are specific rather than a shrug.

The Skills section moved once #207 landed in 1.7.4: `description` is now
filterable on skills, so the retired-skill filter that had to live in Python
is a `WHERE` clause. Notes and Earlier sessions stay here, each for a reason
below.

  Notes             the SELECTION is now expressible -- `valid_to` filters and
                    renders since #206 -- but the RENDER is not. A note or
                    lesson renders as its bare object while any other durable
                    fact renders as "subject relation: object", and the retired
                    reader interleaved both shapes in one recall-ordered list.
                    CAL orders within a section, and a template branches only
                    on truthiness (there is no value comparison), so two
                    sources would emit two runs of lines instead of one
                    interleaved run. That is a change to the prompt, not a
                    refactor of it, and this track has published runs 1-4 --
                    so it belongs in the next run's pre-registration, exactly
                    as AppWorld's passive block does.
  Earlier sessions  one line per SESSION, whose title is a regex over that
                    session's first event. Text extraction landed (#210) and
                    per-group counts landed (#209), but FIRST-of-group did not,
                    so the row this needs still cannot be projected.

Both stay host-composed in `areev_backend.py`. `AREEV.md` in this directory
records the reasoning, which is the record of why the harness looks the way it
does -- and of what a new bench should not expect to express in CAL.
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

PROFILE_HEADING = "### User profile"
SKILLS_HEADING = "### Skills (call skill_view for the steps)"

# An unguarded HEADER plus a `{{^assembly.grain_count}}` FOOTER: the heading
# always renders and an empty section says "(none yet)", which is what the
# assistant has always been shown. (The guarded-heading shape the other tracks
# use is for sections that must VANISH when empty -- a withdrawn rule set.)
NONE_YET = "{{^assembly.grain_count}}- (none yet){{/assembly.grain_count}}"

REGISTRY = [
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
