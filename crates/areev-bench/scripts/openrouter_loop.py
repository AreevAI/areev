#!/usr/bin/env python3
"""Areev Loop `--llm-cmd` / `--ground-cmd` backend over OpenRouter.

One JSON request on stdin, one JSON response on stdout (the DISCOVER / GROUND /
VERIFY / ENRICH protocol in examples/llm/README.md). Stdlib only — no SDK, no
new dependency, per the repo's dependency-light policy.

    openrouter_loop.py MODEL [--provider PROVIDER] [--seed N] [--selfcheck]

Key: $OPENROUTER_API_KEY. Base: $OPENROUTER_BASE_URL or the public endpoint.

Pin the provider on any published run. This adapter decides which LESSONS
get authored, so an unpinned loop leg makes the CONTENT of state B vary from
run to run independently of the agent — a harder confound to spot than a
noisy agent, because the success rate moves while the ledger still looks
plausible. `--seed` narrows what is left where the provider honors it.

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
# Five, not three. The engine FAIL-SOFTS a failing loop call by design, and
# for the GROUND leg that means a single flaky minute silently voids an entire
# learn pass: every draft is treated as ungrounded, the ledger comes back
# empty, and the run reads as a model that had nothing to say. Losing a pass
# is far more expensive to a measurement than waiting a few more seconds, and
# retrying is strictly cheaper than the re-run it otherwise costs.
RETRIES = 5


def fail(msg, code=2):
    print(f"openrouter_loop: {msg}", file=sys.stderr)
    sys.exit(code)


class HttpFail(Exception):
    """An HTTP error a caller may want to classify rather than die on."""

    def __init__(self, code, detail):
        super().__init__(detail)
        self.code = code
        self.detail = detail


def post(body, key, raise_http=False):
    req = urllib.request.Request(
        f"{BASE}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    delay = 3
    for attempt in range(RETRIES):
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code not in (429, 500, 502, 503, 504) or attempt == RETRIES - 1:
                detail = f"HTTP {e.code}: {e.read()[:200]!r}"
                if raise_http:
                    raise HttpFail(e.code, detail) from e
                fail(detail, 1)
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
        fail("usage: openrouter_loop.py MODEL [--provider P] [--seed N] [--selfcheck]")
    model = argv[0]
    provider = None
    seed = None
    selfcheck = False
    i = 1
    while i < len(argv):
        if argv[i] == "--provider" and i + 1 < len(argv):
            provider, i = argv[i + 1], i + 2
        elif argv[i] == "--seed" and i + 1 < len(argv):
            try:
                seed = int(argv[i + 1])
            except ValueError:
                fail(f"--seed must be an integer, got {argv[i + 1]!r}")
            i += 2
        elif argv[i] == "--selfcheck":
            selfcheck, i = True, i + 1
        else:
            fail(f"unknown argument {argv[i]!r}")

    try:
        req = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        fail(f"request is not JSON: {e}")

    # Probe is answered locally when nothing needs verifying — a
    # misconfigured command must fail at construction, and that check should
    # cost no tokens.
    #
    # A `--provider` pin is the exception, and it is worth a few tokens. The
    # engine FAIL-SOFTS a failing loop call by design: a flaky model must not
    # kill a run. But a pin that resolves to no endpoint is not a flaky model,
    # it is a configuration error, and fail-soft turns it into silence —
    # every draft dropped, an empty ledger, and an arm that measured nothing
    # while looking like a clean null. That happened: `--provider Novita`
    # (a display name, not a tag) and `novita/fp8` (a tag whose endpoint does
    # not accept `response_format`) both 404, and a six-cell run completed
    # with zero LLM findings before anyone noticed. So the pin is verified
    # live, once, and a bad one fails HERE where it is loud.
    if req.get("op") == "probe":
        if provider:
            key = os.environ.get("OPENROUTER_API_KEY")
            if not key:
                fail("OPENROUTER_API_KEY is not set")
            try:
                post({
                    "model": model,
                    "temperature": 0,
                    "max_tokens": 1,
                    "response_format": {"type": "json_object"},
                    "provider": {"order": [provider], "allow_fallbacks": False},
                    # The word "json" has to appear in the messages: OpenAI
                    # rejects response_format=json_object otherwise ("'messages'
                    # must contain the word 'json' in some form"), so a bare
                    # "{}" made every OpenAI-served model fail its own pin check
                    # and read as a bad tag. The real calls below already say
                    # "Return JSON" in their instructions; only the probe was
                    # short enough to trip it.
                    "messages": [{"role": "user", "content": "Reply with the json object {}"}],
                }, key, raise_http=True)
            except HttpFail as e:
                # Classify, because the two failures need opposite responses
                # and reporting one as the other wastes an operator's time.
                # A pin that names no eligible endpoint is permanent: fix the
                # tag. A 429 or 5xx is the upstream having a bad minute:
                # retry, or pin a different endpoint of the same model.
                if e.code in (429, 500, 502, 503, 504):
                    fail(
                        f"{model!r} via --provider {provider!r} is failing "
                        f"upstream right now ({e.detail}). This is transient, "
                        f"not a bad pin — retry, or pin another endpoint of "
                        f"the same model. Failing here rather than mid-run, "
                        f"because a loop backend that dies later is dropped "
                        f"silently and the run completes having learned "
                        f"nothing."
                    )
                fail(
                    f"--provider {provider!r} does not serve {model!r} with the "
                    f"request shape this adapter sends ({e.detail}). Use the "
                    f"endpoint TAG (e.g. 'coreweave/bf16'), not the display "
                    f"name, and check the endpoint supports "
                    f"response_format=json_object:\n"
                    f"  curl -H \"Authorization: Bearer $OPENROUTER_API_KEY\" \\\n"
                    f"    https://openrouter.ai/api/v1/models/{model}/endpoints"
                )
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
    if seed is not None:
        body["seed"] = seed

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
