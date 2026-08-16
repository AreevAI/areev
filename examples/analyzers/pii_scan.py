#!/usr/bin/env python3
"""pii_scan.py — a sample external Areev Loop analyzer (`--analyzer-cmd`).

Protocol (one JSON object on stdin, one on stdout):
  probe    {"loop_analyzer":1,"op":"probe"}
           → {"id":"example.pii_scan/1","title":"…","description":"…"}
  analyze  {"loop_analyzer":1,"op":"analyze","now_ms":…,"grains":[…]}
           → {"findings":[{"target":"entity:<ns>/<subject>","summary":"…",
              "severity":"low|medium|high","evidence":["<hash>"],
              "confidence":0.0-1.0}]}

External analyzers run at trust class `command`, auto-apply `never`: they can
SURFACE an issue a human then reviews, never mutate memory. A failure skips
the analyzer for the run, never the pass.

Run it:
  areev loop run --db mem.db --analyzer-cmd 'python3 examples/analyzers/pii_scan.py'
  # Python:  db.loop_run(analyzer_cmd="python3 examples/analyzers/pii_scan.py")
  # Node:    m.loopRun(..., analyzerCmd)
"""
import json
import re
import sys

req = json.loads(sys.stdin.read() or "{}")

if req.get("op") == "probe":
    print(json.dumps({
        "id": "example.pii_scan/1",
        "title": "PII scan (example)",
        "description": "Flags facts whose object looks like an email address.",
    }))
    sys.exit(0)

EMAIL = re.compile(r"[^@\s]+@[^@\s]+\.[^@\s]+")
findings = []
for g in req.get("grains", []):
    if g.get("grain_type") != "fact":
        continue
    fields = g.get("fields", {})
    obj = fields.get("object") or ""
    if EMAIL.search(obj):
        subject = fields.get("subject", "")
        findings.append({
            "target": f"entity:{g.get('namespace', '')}/{subject}",
            "summary": (
                f'fact object for "{subject}" contains an email address — '
                "consider a redacted supersession"
            ),
            "severity": "medium",
            "evidence": [g.get("hash", "")],
            "confidence": 0.9,
        })

print(json.dumps({"findings": findings}))
