#!/usr/bin/env python3
"""PAST-Bench's injected memory block, as CAL the memory file carries.

Separate from `areev_backend.py` on purpose: that module imports PAST-Bench,
so it cannot be loaded without the benchmark installed, and the prompt is the
part a reader (or a keyless CI check) most wants to look at. Nothing here
imports PAST-Bench.

ONE of the four injected sections is a saved ASSEMBLE query registered in the
memory, rendered by a template registered beside it, under a real token
budget. Three are not, and this is the most interesting result of moving this
crate onto the product's own surfaces: PAST-Bench's prompt is the least
CAL-expressible of the five harnesses, for three specific reasons rather than
a shrug.

  Notes             a note may declare a `valid_to`, and an expired one must
                    not render. `valid_to` is not a queryable field on facts
                    (CAL-E060) and not a template variable (CAL-E042), so
                    neither the filter nor the "(until 2026-10-01)" label can
                    move into the query.
  Skills            a retired skill is marked by writing `description:
                    "retired"`, and `description` is not filterable on skills
                    (CAL-E060; `DESCRIBE FIELDS skills` lists `instructions`
                    and `when_to_use` but not `description`). Filtering on
                    `object` instead does not work and does not warn -- it
                    returns every row -- so pushing the filter down would
                    silently put retired skills back in the prompt.
  Earlier sessions  one line per SESSION, whose title is a regex over that
                    session's first event. CAL has `GROUP BY`, but it reorders
                    rows rather than projecting one row per group, and it has
                    no text extraction.

All three stay host-composed in `areev_backend.py`. `AREEV.md` in this
directory records them as the reads a new bench should not expect to express
in CAL, which is worth more to the next harness than a half-converted one.
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
# The CAL ceiling, not a squeeze: this block must not lose rows silently,
# and a binding budget would change published prompt bytes. See
# `cal_assemble.MAX_BUDGET_TOKENS`.
PROFILE_BUDGET_TOKENS = cal.MAX_BUDGET_TOKENS

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
    cal.saved_query(
        "persist_profile", ["ns"],
        '  ASSEMBLE "user profile" FOR "the assistant" FROM\n'
        '    profile: (RECALL facts WHERE namespace = $ns AND relation = "profile" LIMIT %d)\n'
        '  BUDGET %d tokens\n'
        '  FORMAT TEMPLATE persist_profile_tpl' % (SECTION_CAP, PROFILE_BUDGET_TOKENS),
        "what the assistant has learned about the user"),
]


def install(db, db_path=None, ns=NS):
    return cal.install(db, REGISTRY, db_path=db_path, ns=ns)


def profile_block(db, ns=NS):
    return cal.section(db, "persist_profile", {"ns": ns}, cap=SECTION_CAP)


def skills_block(names_and_fields):
    """The skills section, composed here rather than by CAL.

    `names_and_fields` is `live_skills(db)`'s mapping -- the retired ones are
    already gone, because `description` is not filterable on skills and the
    filter cannot be pushed into the query (see the module docstring). Kept in
    this file anyway so the whole injected block reads from one place.
    """
    lines = ["- %s — %s" % (n, f.get("description", ""))
             for n, (_, f) in names_and_fields.items()]
    return SKILLS_HEADING + "\n" + ("\n".join(lines) if lines else "- (none yet)")


def empty_profile_block():
    return PROFILE_HEADING + "\n- (none yet)"


def empty_skills_block():
    return SKILLS_HEADING + "\n- (none yet)"
