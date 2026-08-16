#!/usr/bin/env python3
"""A deterministic mock --llm-cmd backend: no model, no network — echoes a
canned response per op so you can test the wiring (and CI can exercise the LLM
path). NOT for real use. Cites the first evidence hash so the DISCOVER draft
survives the loop's cite-check."""
import json
import sys

req = json.load(sys.stdin)
op = req.get("op")

if op == "probe":
    print(json.dumps({"model": "mock"}))
elif op == "discover":
    ev = req.get("evidence", [])
    if ev:
        print(json.dumps({"recommendations": [{
            "summary": "mock: a human should double-check this cluster",
            "target": "entity:test/mock",
            "guidance": "mock guidance note",
            "evidence": [ev[0]["hash"]],
        }]}))
    else:
        print(json.dumps({"recommendations": []}))
elif op == "ground":
    # Permissive stub: mark every claim supported (a real backend entails each
    # claim against its cited evidence — see the README).
    claims = req.get("claims", [])
    print(json.dumps({"results": [{"id": c["id"], "supported": True} for c in claims]}))
elif op == "verify":
    # Permissive stub: keep every finding with a fixed confidence (a real
    # backend adversarially refutes each and calibrates confidence).
    findings = req.get("findings", [])
    print(json.dumps({"results": [{"id": f["id"], "keep": True, "confidence": 0.85} for f in findings]}))
elif op == "enrich":
    # Add a guidance note to the first finding, if any.
    f = req.get("findings", [])
    notes = [{"target": f[0]["target"], "guidance": "mock: consider the latest value"}] if f else []
    print(json.dumps({"notes": notes}))
else:
    print("{}")
