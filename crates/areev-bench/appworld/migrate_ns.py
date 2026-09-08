#!/usr/bin/env python3
"""Re-namespace an AppWorld memory into per-app child namespaces.

    python3 appworld/migrate_ns.py --src OLD.db --dst NEW.db [--dry-run]
    python3 appworld/migrate_ns.py --src OLD.db --swap            # and take its place

Every run before 2026-09-08 wrote its API errors into one flat `appworld`
namespace. `memory.py` now writes each into `appworld.<app>` and reads
`"appworld.*"`, which selects the base namespace AND its descendants -- so a
flat memory still READS correctly and needs no migration to keep working. This
tool exists for the other reason: once the evidence is in per-app namespaces,
a task about the phone can recall the phone's evidence, which is the defect
`APPWORLD.md` records.

## It never writes INTO the source

Grains are immutable and content-addressed, and `namespace` is part of the
content -- so moving a grain between namespaces MINTS A NEW ADDRESS. There is
no in-place edit that could preserve the old one. This tool therefore reads
`--src` and writes a fresh `--dst`, and refuses if the destination exists.
The migrated memory carries a `migrated_from` Observation naming the source
and the per-namespace counts, so a copy is never mistaken for an original.

`--swap` then makes the migrated memory take the source's NAME, moving the
flat original aside to `<name>.flat.db` rather than deleting it. Nothing is
destroyed and the swap is reversible by hand. Every sibling moves with the
memory -- the `-wal`, the `.blobs/` directory and any `.telemetry.db` -- since
a memory whose WAL was left behind is a memory with a torn tail.

That matters for a published run. AppWorld run 1 is a pre-registered null and
its memories are its evidence, so `APPWORLD.md` records the swap, and the flat
originals it names remain on disk under `.flat.db`.

Keyless: no model, no network.
"""
from __future__ import annotations

import argparse
import gc
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import memory as mem  # noqa: E402

# Every plural read, so nothing is left behind. `recommendations` is
# query-only (engine-emitted, lifecycle-gated) and cannot be re-added -- a
# migrated memory starts with an empty review queue, which is stated rather
# than silently true.
TYPES = ("facts", "events", "states", "workflows", "tools", "observations",
         "goals", "reasonings", "consensuses", "consents", "skills", "triggers")
SINGULAR = {t: t[:-1] for t in TYPES}
SINGULAR.update({"consensuses": "consensus", "facts": "fact", "states": "state"})

# What the store owns and a re-add must not carry: the address itself and the
# index-layer bookkeeping. `created_at` IS carried -- dropping it would
# re-date every grain to the migration and destroy the outcome series the
# Verify gate reads.
DROP_FIELDS = {"type", "namespace"}


def read_all(db, noun, limit=1000):
    out, seen = [], set()
    payload = json.loads(db.cal(
        'RECALL %s WHERE namespace = "%s" LIMIT %d FORMAT json'
        % (noun, mem.NS_SCOPE, limit)))
    for g in payload.get("grains", []):
        h = g.get("hash")
        if h in seen:
            continue
        seen.add(h)
        out.append(g)
    if len(out) >= limit:
        raise RuntimeError(
            "%s hit the %d-grain read cap; a partial migration is worse than "
            "none. Raise the cap or migrate in namespace slices." % (noun, limit))
    return out


def app_of(fields):
    """Which app a grain belongs to.

    A Tool grain names its API as `<app>.<api>`, and `record_tool_call` also
    stores `{"app": …}` in the input. Everything else -- rules, episode
    outcomes -- spans apps and stays in the base namespace: a rule is not the
    property of one app just because it was learned from one.
    """
    name = (fields.get("tool_name") or "")
    raw = fields.get("input")
    if isinstance(raw, str):
        try:
            raw = json.loads(raw)
        except (ValueError, TypeError):
            raw = None
    if isinstance(raw, dict) and raw.get("app"):
        return str(raw["app"])
    return name.split(".", 1)[0] if "." in name else ""


def migrate(src, dst, dry_run=False):
    import areev

    if os.path.exists(dst):
        raise SystemExit("refusing to write %s: it already exists" % dst)
    if not os.path.exists(src):
        raise SystemExit("no such memory: %s" % src)

    reader = areev.Areev(src, ns=mem.NS, actor=mem.RUNNER, read_only=True)
    try:
        grains = {noun: read_all(reader, noun) for noun in TYPES}
    finally:
        del reader
        gc.collect()

    plan: dict[str, int] = {}
    payloads = []
    for noun, rows in grains.items():
        for g in rows:
            fields = {k: v for k, v in (g.get("fields") or {}).items()
                      if k not in DROP_FIELDS}
            ns = mem.ns_for(app_of(fields)) if noun == "tools" else mem.NS
            plan[ns] = plan.get(ns, 0) + 1
            payloads.append((SINGULAR[noun], fields, ns))

    print("  %-24s %s" % ("namespace", "grains"))
    for ns in sorted(plan):
        print("  %-24s %d" % (ns, plan[ns]))
    print("  %-24s %d" % ("TOTAL", sum(plan.values())))
    if dry_run:
        print("\n--dry-run: nothing written.")
        return 0

    writer = areev.Areev(dst, ns=mem.NS, actor=mem.RUNNER)
    written, refused = 0, []
    try:
        mem_registry = getattr(mem, "REGISTRY", [])
        for statement in mem_registry:
            writer.cal(statement)
        for grain_type, fields, ns in payloads:
            try:
                writer.add(grain_type, json.dumps(fields), ns=ns)
                written += 1
            except ValueError as exc:
                refused.append("%s: %s" % (grain_type, str(exc)[:140]))
        # The migrated memory says what it is. Without this, a copy is
        # indistinguishable from an original run's artifact.
        writer.add("observation", json.dumps({
            "content": json.dumps({"migrated_from": os.path.abspath(src),
                                   "grains": written, "by": "appworld/migrate_ns.py",
                                   "namespaces": plan}),
            "observer_id": mem.RUNNER, "observer_type": "system",
            "subject": "appworld_memory",
        }), ns=mem.HARNESS_NS)
    finally:
        del writer
        gc.collect()

    print("\n  wrote %d grains to %s" % (written, dst))
    if refused:
        print("  REFUSED %d:" % len(refused))
        for r in refused[:10]:
            print("    " + r)
        return 1
    print("  the source is untouched; the copy carries a migrated_from record.")
    return 0


# The files that ARE one memory. A `.db` alone is a memory with a torn tail.
SIBLINGS = ("", "-wal", ".blobs", ".telemetry.db", ".telemetry.db-wal")


def _move(src, dst):
    moved = []
    for suffix in SIBLINGS:
        a, b = src + suffix, dst + suffix
        if os.path.exists(a):
            if os.path.exists(b):
                raise SystemExit("refusing to move %s onto an existing %s" % (a, b))
            os.rename(a, b)
            moved.append(os.path.basename(b))
    return moved


def swap(src, migrated):
    """Give the migrated memory the source's name; move the flat one aside.

    `<name>.db` -> `<name>.flat.db`, then `<name>.ns.db` -> `<name>.db`.
    Nothing is deleted: the pre-migration memory stays on disk under a name
    that says what it is.
    """
    base = src[:-3] if src.endswith(".db") else src
    flat = base + ".flat.db"
    if os.path.exists(flat):
        raise SystemExit("refusing to swap: %s already exists" % flat)
    aside = _move(src, flat)
    try:
        took = _move(migrated, src)
    except BaseException:
        _move(flat, src)  # put it back rather than leaving the memory nameless
        raise
    print("  moved aside : %s" % ", ".join(aside))
    print("  took its name: %s" % ", ".join(took))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True, help="the memory to read (never written into)")
    ap.add_argument("--dst", help="the memory to create; defaults to <src>.ns.db")
    ap.add_argument("--dry-run", action="store_true", help="report the plan, write nothing")
    ap.add_argument("--swap", action="store_true",
                    help="after migrating, give the new memory the source's name and "
                         "move the flat original to <name>.flat.db")
    args = ap.parse_args(argv)
    src = os.path.expanduser(args.src)
    dst = os.path.expanduser(args.dst) if args.dst else (
        (src[:-3] if src.endswith(".db") else src) + ".ns.db")

    rc = migrate(src, dst, args.dry_run)
    if rc or args.dry_run or not args.swap:
        return rc
    return swap(src, dst)


if __name__ == "__main__":
    raise SystemExit(main())
