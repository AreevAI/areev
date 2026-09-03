#!/usr/bin/env python3
"""A transparent tee for any JSON-on-stdio adapter: records every call.

    tee_llm.py DUMP_DIR ADAPTER [ADAPTER ARGS...]

Forwards stdin to the adapter, the adapter's stdout to stdout, and writes
{argv, request, stdout, stderr, returncode} to DUMP_DIR/<stamp>.<op>.json.
The loop engine fail-softs a backend that answers with nothing usable, so
"the model proposed nothing" and "the model answered in a shape the parser
dropped" are the same empty funnel — this is how the two are told apart.
Stdlib only; adds nothing to the protocol.
"""
import json
import os
import subprocess
import sys
import time


def main():
    if len(sys.argv) < 3:
        print("usage: tee_llm.py DUMP_DIR ADAPTER [ARGS...]", file=sys.stderr)
        sys.exit(2)
    dump_dir, argv = sys.argv[1], sys.argv[2:]
    os.makedirs(dump_dir, exist_ok=True)
    raw = sys.stdin.buffer.read()
    try:
        op = (json.loads(raw.decode("utf-8")).get("op") or "call")
    except (ValueError, UnicodeDecodeError):
        op = "call"
    p = subprocess.run(argv, input=raw, capture_output=True)
    stamp = "%d-%06d" % (int(time.time()), int((time.time() % 1) * 1e6))
    with open(os.path.join(dump_dir, "%s.%s.json" % (stamp, op)), "w", encoding="utf-8") as fh:
        json.dump({
            "argv": argv,
            "request": raw.decode("utf-8", "replace"),
            "stdout": p.stdout.decode("utf-8", "replace"),
            "stderr": p.stderr.decode("utf-8", "replace"),
            "returncode": p.returncode,
        }, fh, ensure_ascii=False, indent=1)
    sys.stdout.buffer.write(p.stdout)
    sys.stderr.buffer.write(p.stderr)
    sys.exit(p.returncode)


if __name__ == "__main__":
    main()
