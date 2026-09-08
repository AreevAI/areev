#!/usr/bin/env python3
"""Freeze each arm's learned memory and record exactly what it will carry.

    python3 freeze.py --workdir ~/mg/local/areev-runs/appworld/run1

Copies `<arm>/learn.db` to `<arm>/eval.db` and prints the block each arm's
agent will read on every held-out episode, verbatim, into `<arm>/block.txt`.

That transcript is the point. The blocks ARE the manipulation, so a reader
who wants to know what the governed arm was actually told should not have to
re-derive it from a database -- and neither should we, later, when the result
needs explaining. The held-out arms then open these copies read-only, so what
is written here is what was read.
"""
from __future__ import annotations

import argparse
import importlib.util
import os

HERE = os.path.dirname(os.path.abspath(__file__))


def _sibling(name: str):
    spec = importlib.util.spec_from_file_location(
        "areev_appworld_" + name, os.path.join(HERE, name + ".py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


memory = _sibling("memory")

ARMS = {"passive": "passive", "governed": "governed"}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", required=True)
    args = ap.parse_args()
    workdir = os.path.expanduser(args.workdir)

    failed = False
    for arm, mode in ARMS.items():
        learn = os.path.join(workdir, arm, "learn.db")
        evaldb = os.path.join(workdir, arm, "eval.db")
        if not os.path.exists(learn):
            print(f"MISSING {learn} -- the {arm} learn pass has not produced a memory")
            failed = True
            continue
        memory.copy_memory(learn, evaldb)
        block = memory.block_for(evaldb, mode, read_only=True)
        out = os.path.join(workdir, arm, "block.txt")
        with open(out, "w", encoding="utf-8") as fh:
            fh.write(block + "\n")

        lines = [line for line in block.splitlines() if line.startswith("- ")]
        print(f"=== {arm} -> {evaldb}")
        print(f"    {len(lines)} line(s), {len(block)} chars, written to {out}")
        for line in lines:
            print("    " + line[:150])
        if not block.strip():
            print("    (EMPTY -- this arm is identical to arm A, and the comparison is vacuous)")
        print()

    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
