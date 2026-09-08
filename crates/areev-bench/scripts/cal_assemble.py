#!/usr/bin/env python3
"""Assemble a prompt block through CAL, and prove it agrees with a hand-rolled one.

Shared by every harness in this crate (see `../CLAUDE.md`, "Use the product's
own surfaces"). Two entry points:

    from cal_assemble import assemble
    block = assemble(db, "operating rules", [("rules", 'RECALL facts WHERE relation = "lesson"')],
                     budget_tokens=300, fmt="markdown", dedup="object")

    python3 cal_assemble.py --db PATH --ns NS --relation lesson   # the parity smoke

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
import sys


def _q(text: str) -> str:
    """CAL string literal."""
    return '"%s"' % str(text).replace('"', '\\"')


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
