#!/usr/bin/env python3
"""A deterministic mock `--llm-cmd` backend: no model, no network, no key.

Echoes a canned response per op so you can test the wiring and CI can exercise
the whole LLM path. **Not for real use, and never a learning claim** — what it
proves is that the path from a draft to a governed, applied, rollbackable
change exists, not that a model would propose anything.

Two modes:

  1. Default — one advisory draft citing the first bundled evidence hash, so a
     DISCOVER finding survives the loop's cite-check.

  2. Fixture-driven — set `AREEV_MOCK_LLM_FIXTURE=<file.json>` and the DISCOVER
     response is read from that file. This is how an example pins the exact
     proposal it wants to demonstrate. Any draft with no `evidence` gets the
     first bundled hash filled in, because the hashes are content addresses
     that do not exist until the memory has been written — a fixture cannot
     know them, and hard-coding one would make the fixture rot on any change
     to the data it was captured from.

The DISCOVER draft's optional `proposal` field selects what change is being
asked for (docs/loop.md, "LLM enrichment"). Every kind still goes through
GROUND + VERIFY and a human review with a BECAUSE before it can apply.
"""
import json
import os
import sys

req = json.load(sys.stdin)
op = req.get("op")


def discover():
    ev = req.get("evidence", [])
    if not ev:
        # Abstention is a first-class answer: with nothing to reflect on,
        # proposing something would be the wrong behavior to model.
        return {"recommendations": []}
    first = ev[0]["hash"]
    path = os.environ.get("AREEV_MOCK_LLM_FIXTURE")
    if path:
        with open(path, encoding="utf-8") as fh:
            canned = json.load(fh)
        for d in canned.get("recommendations", []):
            d.setdefault("evidence", [first])
            d.setdefault("confidence", 0.9)
        return canned
    return {"recommendations": [{
        "summary": "mock: a human should double-check this cluster",
        "target": "entity:test/mock",
        "guidance": "mock guidance note",
        "evidence": [first],
        "confidence": 0.9,
    }]}


if op == "probe":
    print(json.dumps({"model": "mock"}))
elif op == "discover":
    print(json.dumps(discover()))
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
