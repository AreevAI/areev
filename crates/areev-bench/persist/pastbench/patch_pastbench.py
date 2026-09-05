#!/usr/bin/env python3
"""The one edit the Areev arms need in PAST-Bench, applied to a checkout:

    python patch_pastbench.py /path/to/PAST-Bench

`past-bench evolve` builds a persistence backend — and therefore allows
`--compare-no-persistence` — only for agent names it knows
(`hermes*`, `nanobot`, `zeroclaw`). The adapter and backend themselves are
registered at import time by `run.py` without touching the benchmark; this
allowlist is the one place a name is hard-coded. The patch adds
`areev*` to it and nothing else. Idempotent; prints what it did.
"""
import sys
from pathlib import Path

root = Path(sys.argv[1])
cli = root / "src" / "past_bench" / "cli.py"
s = cli.read_text(encoding="utf-8")
old = 'if args.agent.startswith("hermes") or args.agent in {"nanobot", "zeroclaw"}:'
mid = 'if args.agent.startswith("hermes") or args.agent.startswith("areev") or args.agent in {"nanobot", "zeroclaw"}:'
new = ('if args.agent.startswith("hermes") or args.agent.startswith("areev") or args.agent.startswith("mem0") '
       'or args.agent in {"nanobot", "zeroclaw"}:')
if new in s:
    print("already patched:", cli)
elif mid in s:
    cli.write_text(s.replace(mid, new, 1), encoding="utf-8")
    print("patched (mem0 added):", cli)
elif old in s:
    cli.write_text(s.replace(old, new, 1), encoding="utf-8")
    print("patched:", cli)
else:
    raise SystemExit("allowlist line not found in %s — PAST-Bench changed shape; re-read cli.py" % cli)
