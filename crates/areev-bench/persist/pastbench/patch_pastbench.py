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

# Second edit, a crash guard in the benchmark's own reflection summarizer:
# it reads `messages[-1].message.content[0].text`, and a reflection trace
# whose last message is a tool result (the runner's turn budget ran out on a
# tool call) raises AttributeError and kills the whole family-run — seen on
# the passive arm's memory family in the pilot. Take the last TEXT block
# instead; a trace with none summarizes as "".
se = root / "src" / "past_bench" / "runner" / "self_evolve.py"
t = se.read_text(encoding="utf-8")
old_ref = """    final_text = ""
    if messages and messages[-1].message.content:
        final_text = messages[-1].message.content[0].text
"""
new_ref = """    final_text = ""
    for m in reversed(messages):
        texts = [b.text for b in (m.message.content or []) if getattr(b, "type", "") == "text" and getattr(b, "text", "")]
        if texts:
            final_text = texts[0]
            break
"""
if new_ref in t:
    print("already patched:", se)
elif old_ref in t:
    se.write_text(t.replace(old_ref, new_ref, 1), encoding="utf-8")
    print("patched (reflection summarizer guard):", se)
else:
    raise SystemExit("reflection summarizer not found in %s — re-read self_evolve.py" % se)
