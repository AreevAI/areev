#!/usr/bin/env python3
"""Assemble a prompt block through CAL, and prove it agrees with a hand-rolled one.

Shared by every harness in this crate (see `../CLAUDE.md`, "Use the product's
own surfaces"). Three entry points:

    from cal_assemble import assemble, install, section

    install(db, REGISTRY)                 # DEFINE TEMPLATE / DEFINE QUERY, once per file
    block = section(db, "lessons", ns="ledger", subject="receipt_capture")
    block = assemble(db, "operating rules", [("rules", 'RECALL facts WHERE relation = "lesson"')],
                     budget_tokens=300, fmt="markdown", dedup="object")

    python3 cal_assemble.py --db PATH --ns NS --relation lesson   # the parity smoke

## Why a prompt section is a saved query, not a Python string

A `DEFINE QUERY` persists as a `qry:<name>` meta row: it travels with the
`.db`, replicates through bundles, and is visible from the CLI, MCP and the
console. A read that lives as a Python f-string means a memory handed to
someone else does not carry how to read it -- the knowledge is stranded in
the harness that happened to write the file. Same for `DEFINE TEMPLATE`
(`tpl:<name>`): the renderer travels too, so the prompt a published number
was produced under is IN the artifact rather than in a commit.

## The one shape every section here uses

    DEFINE TEMPLATE <name> HEADER {{{#if assembly.grain_count}}<heading>{{/if}}}
                           ELEMENT {- {{grain.object}}}

The `{{#if assembly.grain_count}}` guard is load-bearing: a section with no
grains must render to the EMPTY STRING, because that is what a rolled-back
lesson set has to look like. Without it a governed arm whose rules were
withdrawn would still carry the heading that announces them, and the paired
evaluation would no longer be causal.

Ordering comes from a stage inside the source -- `(RECALL facts WHERE
relation = "lesson" ORDER BY object ASC LIMIT 50)` -- which is what makes a
CAL-assembled block byte-identical to the `sorted(set(...))` the harnesses
hand-rolled. That position did not parse before 2026-09-08; CAL-W016 named it
as the fix while the parser refused it (see `docs/cal-reference.md`,
"A parenthesised source carries its own pipeline").

The smoke exists because "we switched to ASSEMBLE" is a claim about output,
not about intent. It checks four things a harness actually depends on:

  1. PARITY      -- ASSEMBLE selects the same grain bodies the hand-rolled
                    RECALL path selects. Not byte-identical: the point is that
                    swapping the assembler does not change WHICH grains reach
                    the model.
  2. DETERMINISM -- the same statement twice returns byte-identical text. An
                    assembler that reorders between calls would make two arms
                    of one experiment incomparable.
  3. BUDGET      -- with `BUDGET n tokens`, the rendered block estimates at or
                    under n. A budget that does not bind is a comment.
  4. FORMATS     -- markdown, sml, toon and json render the SAME grain count.
                    A format that silently drops rows is a token saving that
                    is really a data loss.
"""
from __future__ import annotations

import argparse
import json
import os


def _q(text: str) -> str:
    """CAL string literal."""
    return '"%s"' % str(text).replace('"', '\\"')


def _lit(value) -> str:
    """A CAL literal of the right TYPE.

    Quoting everything is wrong and fails quietly: a saved query bound with
    `$now = "1788922013848"` compares a string against a numeric field, so
    `valid_to > $now` silently selects the wrong rows rather than erroring.
    Booleans and numbers are literals in CAL; everything else is a string.
    """
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    return _q(value)


# `ASSEMBLE` applies a token budget whether or not you ask for one: the default
# is 4000 and the ceiling is 16000 (CAL-E033 above that). It used to drop grains
# to that budget SILENTLY -- AppWorld's error selection returned 79 of 229 under
# the default -- which is what #208 was filed for. As of 1.7.4 the drop
# announces itself as `CAL-W017`, and `raise_on_truncation` below turns that
# warning into an exception rather than a line nobody reads.
#
# Every prompt section in this crate still states its budget, at the ceiling,
# because a stated bound is the point: these blocks must not lose rows, and a
# binding budget would change published prompt bytes. The ceiling is not a
# guarantee of no trimming -- 300 long grains still trip it -- which is exactly
# why the warning is now checked. A track that WANTS progressive disclosure
# (Full -> Summary -> Omit) sets a lower budget deliberately and says so where
# its numbers are published.
MAX_BUDGET_TOKENS = 16_000

# The warnings that mean "this answer is a window, not the whole match". For a
# prompt section that is data loss, not news, so it raises.
TRUNCATION_WARNINGS = ("CAL-W015", "CAL-W017")


def raise_on_truncation(payload, name):
    """Refuse an answer the engine has told us is incomplete.

    `CAL-W017` (the budget dropped grains) and `CAL-W015` (the widened scan
    came back full) both mean the block is missing rows. A harness that
    printed these and carried on would be publishing a number produced from a
    truncated prompt -- the failure #208 was filed for.
    """
    for w in payload.get("warnings") or []:
        if any(w.startswith(code) for code in TRUNCATION_WARNINGS):
            raise RuntimeError("%s: %s" % (name, w))


def assemble_statement(topic, sources, budget_tokens=None, fmt="markdown",
                       dedup=None, audience=None, priority=None):
    """Build the CAL statement. Clause order is fixed by OMS §8.2 and a clause
    written out of order is a parse error, so it is built here rather than
    concatenated at each call site."""
    parts = ["ASSEMBLE %s" % _q(topic)]
    if audience:
        parts.append("FOR %s" % _q(audience))
    parts.append("FROM " + ", ".join("%s: (%s)" % (label, query) for label, query in sources))
    if budget_tokens:
        parts.append("BUDGET %d tokens" % budget_tokens)
    if priority:
        parts.append("PRIORITY " + ", ".join("%s: %s" % (k, v) for k, v in priority.items()))
    parts.append("FORMAT %s" % fmt)
    if dedup:
        parts.append("WITH dedup(%s)" % dedup)
    return "\n".join(parts)


def assemble(db, topic, sources, budget_tokens=None, fmt="markdown",
             dedup=None, audience=None, priority=None):
    """Run it. Returns the parsed result: {"text", "grain_count", ...}."""
    statement = assemble_statement(topic, sources, budget_tokens, fmt, dedup, audience, priority)
    return json.loads(db.cal(statement))


# --------------------------------------------------------------------------
# the registry: templates and saved queries that travel with the file
# --------------------------------------------------------------------------

# `DEFINE` is a write. A read-only handle (STO-E004) cannot install anything,
# which is deliberate -- a held-out arm reads a FROZEN memory and must not be
# able to change how it reads. So installation belongs on the write path, and
# a reader that finds no registry says so rather than quietly falling back to
# a hand-rolled read that would no longer be the measured one.
_INSTALLED: set[tuple[str, str]] = set()


def install(db, registry, db_path=None, ns=None, force=False):
    """Register this harness's templates and saved queries into the memory.

    Idempotent: `DEFINE` overwrites, and the (path, ns) pair is remembered
    for the process so a per-episode write path can call this unconditionally
    without re-running a dozen statements every episode. `force=True` skips
    the memo -- for a smoke that wants the statements actually executed.
    """
    key = (str(db_path or ""), str(ns or ""))
    if not force and key != ("", "") and key in _INSTALLED:
        return 0
    for statement in registry:
        db.cal(statement)
    if key != ("", ""):
        _INSTALLED.add(key)
    return len(registry)


def installed_queries(db):
    """The saved queries this memory carries, by name."""
    info = json.loads(db.cal("DESCRIBE QUERIES")).get("info", {})
    return {q.get("name") for q in info.get("queries", [])}


def section(db, name, params=None, cap=None):
    """Run one saved query and return its rendered text.

    Returns the empty string when the section has no grains -- every template
    here guards its heading with `{{#if assembly.grain_count}}`, so an empty
    section is empty rather than a bare heading.

    `cap` is the LIMIT the saved query was written with. A section that comes
    back holding exactly that many grains has silently lost the rest, and the
    prompt built from it is missing rules -- the failure the receipts harness
    hit once at 300 and again at 1000. Passing the cap turns that into a
    raise; omitting it says the section is bounded by construction.

    A grain dropped by the token budget is caught separately, by
    `raise_on_truncation`: since 1.7.4 the engine says so as `CAL-W017`, and a
    prompt section that lost rows is a wrong prompt, not a warning.
    """
    params = params or {}
    bindings = ", ".join("$%s = %s" % (k, _lit(v)) for k, v in sorted(params.items()))
    stmt = 'RUN %s(%s)' % (_q(name), bindings)
    payload = json.loads(db.cal(stmt))
    raise_on_truncation(payload, "section %r" % name)
    text = payload.get("text")
    if text is None:
        raise RuntimeError(
            "saved query %r returned no rendered text (got keys %s); a prompt "
            "section must FORMAT to text" % (name, sorted(payload)))
    if cap is not None and int(payload.get("grain_count") or 0) >= cap:
        raise RuntimeError(
            "section %r hit its %d-grain cap; the prompt would be missing rows. "
            "Narrow the saved query." % (name, cap))
    return text.strip("\n")


def rows(db, name, params=None, cap=None):
    """Run a saved query that renders `FORMAT json` and return its grains.

    For the reads whose ROWS the harness needs rather than a rendered block:
    the reviewer's prior decisions and the held-out outcome series, both of
    which are Python inputs, not prompt sections. The frequency ranking that
    used to be here is gone -- `GROUP BY … COUNT` (#209) and a composite key
    (#217) put it back in the engine, where the ordering is a contract rather
    than a loop in this file.
    """
    params = params or {}
    bindings = ", ".join("$%s = %s" % (k, _lit(v)) for k, v in sorted(params.items()))
    payload = json.loads(db.cal('RUN %s(%s)' % (_q(name), bindings)))
    raise_on_truncation(payload, "saved query %r" % name)
    got = payload.get("grains")
    if got is None and isinstance(payload.get("text"), str):
        got = json.loads(payload["text"])
    got = got or []
    if cap is not None and len(got) >= cap:
        raise RuntimeError(
            "saved query %r hit its %d-grain cap; the block would be missing "
            "rows. Narrow the query." % (name, cap))
    return got


def block(db, sections, sep="\n\n"):
    """Join the non-empty sections -- the shape every harness's prompt block
    already had (`"\n\n".join(parts)`, or `"\n"` where the sections are one
    flat list rather than headed blocks).

    `sections` is a list of (name, params) or (name, params, cap).
    """
    out = []
    for spec in sections:
        name, params = spec[0], spec[1]
        cap = spec[2] if len(spec) > 2 else None
        text = section(db, name, params, cap)
        if text:
            out.append(text)
    return sep.join(out)


def guarded_template(name, heading, element, summary=None):
    """`DEFINE TEMPLATE` for one prompt section.

    `heading` renders only when the section has grains; `element` renders once
    per grain. Braces are the section delimiters, so a body containing `{` or
    `}` outside a `{{...}}` expression cannot be expressed here -- which is
    why every heading in this crate is plain prose.
    """
    parts = ["DEFINE TEMPLATE %s" % name]
    if heading:
        parts.append("  HEADER {{{#if assembly.grain_count}}%s{{/if}}}" % heading)
    parts.append("  ELEMENT {%s}" % element)
    if summary:
        parts.append("  ELEMENT_SUMMARY {%s}" % summary)
    return "\n".join(parts)


def saved_query(name, params, body, description=None):
    """`DEFINE QUERY` wrapper. `params` are bare names (no `$`)."""
    head = 'DEFINE QUERY %s(%s)' % (_q(name), ", ".join("$" + p for p in params))
    if description:
        head += "\n  DESCRIPTION %s" % _q(description)
    return "%s\nAS {\n%s\n}" % (head, body)


# --------------------------------------------------------------------------
# the review context: what the reviewer already decided
# --------------------------------------------------------------------------
#
# The loop's GENERATION side already reads history: `areev-loop` dedupes a
# candidate against every recommendation already recorded (`dedup_key`), and
# a REJECTION starts an exponential cooldown on that key — 7d, 14d, 28d, …
# capped at 90 — so a finding the reviewer turned down stops re-surfacing on
# a fixed cadence. Harnesses get that for free by dismissing through
# `dismiss_recommendation`.
#
# What the harnesses did NOT read is the same history on the REVIEW side. A
# reviewer that judges each proposal against only the rules currently in force
# will happily approve a REWORDING of something it declined last month: the
# reword carries a different `dedup_key`, so the engine's cooldown never sees
# it, and nothing else was looking. These two queries close that, and they are
# saved queries so a memory carries how its own review was conducted.
#
# The window is a literal, not a parameter: `SINCE $window` is refused
# (CAL-E059 → CAL-E002, "expected string literal"), so a caller wanting a
# different window registers a different query.
REVIEW_WINDOW = "90d"
REVIEW_HISTORY_CAP = 300

REVIEW_REGISTRY = [
    saved_query(
        "bench_review_history", [],
        '  RECALL recommendations WHERE rec_status IN ("rejected", "applied")\n'
        '  SINCE "%s"\n'
        '  LIMIT %d\n'
        '  FORMAT json' % (REVIEW_WINDOW, REVIEW_HISTORY_CAP),
        "what the reviewer already ruled on in the last %s, and how" % REVIEW_WINDOW),
    saved_query(
        "bench_outcomes", ["ns"],
        '  RECALL facts WHERE namespace = $ns AND relation = "mg:eval_run"\n'
        '  LIMIT 100\n'
        '  FORMAT json',
        "the held-out outcome series the Verify gate reads"),
]


def review_history(db):
    """[{status, summary, analyzer, target_ref}] — decided, newest first."""
    out = []
    for g in rows(db, "bench_review_history", cap=REVIEW_HISTORY_CAP):
        f = g.get("fields", g) if isinstance(g, dict) else {}
        out.append({"status": f.get("rec_status") or f.get("status") or "",
                    "summary": f.get("summary") or "",
                    "analyzer": f.get("analyzer") or "",
                    "target_ref": f.get("target_ref") or ""})
    return out


def outcomes(db, ns):
    """The eval-run series, so a reviewer can see whether the last approvals
    moved anything before approving more."""
    out = []
    for g in rows(db, "bench_outcomes", {"ns": ns}):
        f = g.get("fields", g) if isinstance(g, dict) else {}
        body = f.get("object")
        if isinstance(body, str):
            try:
                body = json.loads(body)
            except ValueError:
                continue
        if isinstance(body, dict):
            out.append(body)
    return out


def restates_a_decision(text, prior, normalize, content_words, threshold=0.6):
    """The prior decision this proposal restates, if any.

    Same Jaccard-over-content-words test each track already uses for "already
    in force" — reused rather than reimplemented, so a reviewer's notion of
    "the same rule" does not depend on which side of the decision it is on.
    """
    mine = content_words(normalize(text or ""))
    if not mine:
        return None
    for earlier in prior:
        theirs = content_words(normalize(earlier.get("text") or ""))
        if theirs and len(mine & theirs) / len(mine | theirs) >= threshold:
            return earlier
    return None


# --------------------------------------------------------------------------
# the parity smoke
# --------------------------------------------------------------------------

FAILURES: list[str] = []


def check(name, ok, detail=""):
    print(("  ok   " if ok else "  FAIL ") + name + (("  " + str(detail)[:160]) if detail else ""))
    if not ok:
        FAILURES.append(name)


def _bodies_from_recall(db, ns, noun, where, limit=400):
    """What the hand-rolled path selects: RECALL ... FORMAT json, read in Python."""
    query = 'RECALL %s WHERE namespace = %s%s LIMIT %d FORMAT json' % (
        noun, _q(ns), where, limit)
    grains = json.loads(db.cal(query))["grains"]
    out = []
    for g in grains:
        f = g.get("fields", {})
        body = f.get("object") or f.get("tool_content") or f.get("content") or ""
        if body:
            out.append(body.strip())
    return out


def run_smoke(db_path, ns, noun, where, budget, actor):
    import areev

    print(f"\n=== {os.path.basename(os.path.dirname(db_path))}/{os.path.basename(db_path)} "
          f"ns={ns} {noun}{where or ''}")
    db = areev.Areev(db_path, ns=ns, actor=actor, read_only=True)

    recall_bodies = _bodies_from_recall(db, ns, noun, where)
    if not recall_bodies:
        print("  (no grains match -- nothing to compare)")
        return

    source = [("rows", 'RECALL %s WHERE namespace = %s%s' % (noun, _q(ns), where))]

    # 1. parity
    got = assemble(db, "parity", source, fmt="json")
    a_bodies = []
    for g in json.loads(got["text"]) if isinstance(got.get("text"), str) else got.get("grains", []):
        f = g.get("fields", g) if isinstance(g, dict) else {}
        body = f.get("object") or f.get("tool_content") or f.get("content") or ""
        if body:
            a_bodies.append(body.strip())
    check("ASSEMBLE selects the same grain bodies as RECALL",
          set(a_bodies) == set(recall_bodies),
          f"assemble={len(set(a_bodies))} recall={len(set(recall_bodies))}")

    # 2. determinism
    one = assemble(db, "determinism", source, budget_tokens=budget, fmt="markdown")
    two = assemble(db, "determinism", source, budget_tokens=budget, fmt="markdown")
    check("the same statement twice is byte-identical", one["text"] == two["text"])

    # 3. budget binds
    est = len(one["text"]) / 4.0  # rough; the real check is that it shrank
    unbudgeted = assemble(db, "determinism", source, fmt="markdown")
    check(f"BUDGET {budget} tokens binds (or the whole set already fits)",
          len(one["text"]) <= len(unbudgeted["text"]),
          f"budgeted={len(one['text'])}ch (~{est:.0f} tok) full={len(unbudgeted['text'])}ch")

    # 4. formats agree on how many grains they carry
    counts = {}
    for fmt in ("markdown", "sml", "toon", "json"):
        try:
            got = assemble(db, "formats", source, fmt=fmt)
            # FORMAT json returns the grains themselves rather than a rendered
            # block, so it carries no grain_count -- count the rows instead.
            if "grain_count" in got:
                counts[fmt] = got["grain_count"]
            else:
                rows = got.get("grains")
                if rows is None and isinstance(got.get("text"), str):
                    rows = json.loads(got["text"])
                counts[fmt] = len(rows or [])
        except Exception as exc:  # noqa: BLE001
            counts[fmt] = f"ERROR {str(exc)[:60]}"
    check("markdown, sml, toon and json carry the same grain count",
          len({v for v in counts.values() if isinstance(v, int)}) == 1
          and all(isinstance(v, int) for v in counts.values()),
          json.dumps(counts))

    # token cost per format, for the record -- this is the axis a hand-rolled
    # block forfeits, so it is measured rather than asserted.
    sizes = {}
    for fmt in ("markdown", "sml", "toon", "json"):
        try:
            got = assemble(db, "cost", source, fmt=fmt)
            sizes[fmt] = len(got["text"] if isinstance(got.get("text"), str) else json.dumps(got))
        except Exception:  # noqa: BLE001
            sizes[fmt] = -1
    cheapest = min((v, k) for k, v in sizes.items() if v > 0)[1]
    print(f"       chars by format: {sizes}  -> cheapest: {cheapest}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", action="append", required=True,
                    help="memory to smoke; repeatable as PATH:NS")
    ap.add_argument("--noun", default="facts")
    ap.add_argument("--relation", default=None)
    ap.add_argument("--budget", type=int, default=200)
    ap.add_argument("--actor", default="user:local")
    args = ap.parse_args()

    where = ' AND relation = %s' % _q(args.relation) if args.relation else ""
    for spec in args.db:
        path, _, ns = spec.partition(":")
        run_smoke(os.path.expanduser(path), ns or "shared", args.noun, where,
                  args.budget, args.actor)

    print()
    if FAILURES:
        print(f"FAILED: {len(FAILURES)} check(s): {', '.join(FAILURES[:4])}")
        return 1
    print("ASSEMBLE agrees with the hand-rolled path, is deterministic, "
          "honours its budget and does not lose grains between formats.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
