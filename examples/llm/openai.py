#!/usr/bin/env python3
"""Areev Loop --llm-cmd backend over the OpenAI API. One JSON request on stdin,
one JSON response on stdout (see README). Needs OPENAI_API_KEY and `openai`."""
import json
import os
import sys

req = json.load(sys.stdin)
MODEL = os.environ.get("AREEV_LOOP_LLM_MODEL", "gpt-4o-mini")

# Probe is answered locally — no model call.
if req.get("op") == "probe":
    print(json.dumps({"model": MODEL}))
    sys.exit(0)

from openai import OpenAI  # imported lazily so `probe` needs no dependency

# Instructions are the system prompt; the rest of the request is the (untrusted)
# user content — never merge them into the system role. Forward every other key:
# GROUND sends `claims`, so a fixed key list would empty that stage.
payload = {k: v for k, v in req.items() if k != "instructions"}
resp = OpenAI().chat.completions.create(
    model=MODEL,
    response_format={"type": "json_object"},
    messages=[
        {"role": "system", "content": req["instructions"] + " Respond with only the JSON object."},
        {"role": "user", "content": json.dumps(payload)},
    ],
)
print(resp.choices[0].message.content)
