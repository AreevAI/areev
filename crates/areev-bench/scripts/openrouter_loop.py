#!/usr/bin/env python3
"""Areev Loop `--llm-cmd` / `--ground-cmd` backend over OpenRouter.

One JSON request on stdin, one JSON response on stdout (the DISCOVER / GROUND /
VERIFY / ENRICH protocol in examples/llm/README.md). Stdlib only — no SDK, no
new dependency, per the repo's dependency-light policy.

    openrouter_loop.py MODEL [--provider PROVIDER] [--selfcheck]

Key: $OPENROUTER_API_KEY. Base: $OPENROUTER_BASE_URL or the public endpoint.

Unlike examples/llm/openai.py this forwards EVERY payload key except
`instructions`: GROUND sends `claims`, not `findings`, so a fixed key list
silently empties that stage. `instructions` stays the system role and the
payload stays the user role — evidence text is untrusted and must never reach
the system prompt.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("OPENROUTER_BASE_URL", "https://openrouter.ai/api/v1")
RETRIES = 3


def fail(msg, code=2):
    print(f"openrouter_loop: {msg}", file=sys.stderr)
    sys.exit(code)


def post(body, key):
    req = urllib.request.Request(
        f"{BASE}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    delay = 2
    for attempt in range(RETRIES):
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code not in (429, 500, 502, 503, 504) or attempt == RETRIES - 1:
                fail(f"HTTP {e.code}: {e.read()[:200]!r}", 1)
            wait = e.headers.get("Retry-After")
            time.sleep(float(wait) if wait and wait.isdigit() else delay)
        except urllib.error.URLError as e:
            if attempt == RETRIES - 1:
                fail(f"connection: {e}", 1)
            time.sleep(delay)
        delay *= 2
    fail("retries exhausted", 1)


def main():
    argv = sys.argv[1:]
    if not argv:
        fail("usage: openrouter_loop.py MODEL [--provider P] [--selfcheck]")
    model = argv[0]
    provider = None
    selfcheck = False
    i = 1
    while i < len(argv):
        if argv[i] == "--provider" and i + 1 < len(argv):
            provider, i = argv[i + 1], i + 2
        elif argv[i] == "--selfcheck":
            selfcheck, i = True, i + 1
        else:
            fail(f"unknown argument {argv[i]!r}")

    try:
        req = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        fail(f"request is not JSON: {e}")

    # Probe is answered locally: a misconfigured command must fail at
    # construction, and that check should cost no tokens.
    if req.get("op") == "probe":
        print(json.dumps({"model": model}))
        return

    instructions = req.get("instructions", "")
    payload = {k: v for k, v in req.items() if k != "instructions"}

    if selfcheck:
        print(json.dumps({"op": req.get("op"), "forwarded_keys": sorted(payload)}))
        return

    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        fail("OPENROUTER_API_KEY is not set")

    body = {
        "model": model,
        "temperature": 0,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": instructions + " Respond with only the JSON object."},
            {"role": "user", "content": json.dumps(payload)},
        ],
    }
    if provider:
        body["provider"] = {"order": [provider], "allow_fallbacks": False}

    resp = post(body, key)
    try:
        content = resp["choices"][0]["message"]["content"]
    except (KeyError, IndexError):
        fail(f"unexpected response shape: {json.dumps(resp)[:200]}", 1)
    # The engine's parsers are unwrap_or_default: garbage yields no findings
    # rather than an error, so emit the model's text as-is.
    print(content if content else "{}")


if __name__ == "__main__":
    main()
