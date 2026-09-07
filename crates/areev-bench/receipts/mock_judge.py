#!/usr/bin/env python3
"""A keyless rule reviewer for the dry run: approves a rule that names the
date format, rejects everything else. Proves the review gate is wired (a
rejected rule must not render); it never stands in for the rubric judge on
a measured run."""
import json
import sys

req = json.load(sys.stdin)
rule = next((m["content"] for m in req.get("messages", []) if m["role"] == "user"), "")
ok = "DD/MM/YYYY" in rule
print(json.dumps({"message": {"content": json.dumps({
    "approve": ok,
    "reason": "names the date format" if ok else "mock judge: not a date-format rule",
})}}))
