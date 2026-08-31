#!/usr/bin/env python3
"""OpenRouter tool-calling adapter for the selfimprove_* benches.

    usage: openrouter_toolcall.py MODEL [--provider PROVIDER] [--seed N] [--selfcheck]
    key:   $OPENROUTER_API_KEY
    base:  $OPENROUTER_BASE_URL (default https://openrouter.ai/api/v1)

Reads ONE JSON request line on stdin (the SELFIMPROVE.md runner protocol):
    {"op":"chat","model":M,"messages":[...],"tools":[...],"temperature":0}
POSTs it to /chat/completions (MODEL from argv wins over the request's model;
--provider pins {"provider":{"order":[P],"allow_fallbacks":false}}) and prints
ONE JSON line on stdout:
    {"message":{"role":...,"content":...,"tool_calls":[{"id","name","arguments"}]},
     "usage":{"prompt_tokens":N,"completion_tokens":N},
     "meta":{"model":...,"provider":...}}
The "meta" key is extra — the Rust side ignores unknown keys; it feeds
transcripts. Nothing but that single line is ever printed to stdout.

--selfcheck validates the request shape and prints a canned normalized
response without touching the network (keyless; CI round-trips use it).

Stdlib only (urllib) — no pip install. Exit codes: 2 = missing key / bad
usage / bad request, 1 = upstream failure after retries.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

RETRY_DELAYS = (2.0, 4.0, 8.0)

CANNED_RESPONSE = {
    "choices": [
        {
            "message": {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "search_customers",
                            "arguments": "{\"query\": \"jane\"}",
                        },
                    }
                ],
            }
        }
    ],
    "usage": {"prompt_tokens": 42, "completion_tokens": 7},
    "model": "selfcheck/canned",
}


def die(msg: str, code: int = 2) -> None:
    sys.stderr.write(f"openrouter_toolcall: {msg}\n")
    sys.exit(code)


def parse_args(argv):
    model, provider, selfcheck, seed = None, None, False, None
    i = 1
    while i < len(argv):
        a = argv[i]
        if a == "--seed":
            i += 1
            if i >= len(argv):
                die("--seed needs a value")
            try:
                seed = int(argv[i])
            except ValueError:
                die(f"--seed must be an integer, got {argv[i]!r}")
            i += 1
            continue
        if a == "--provider":
            i += 1
            if i >= len(argv):
                die("--provider needs a value")
            provider = argv[i]
        elif a == "--selfcheck":
            selfcheck = True
        elif a.startswith("--"):
            die(f"unknown flag {a}; usage: openrouter_toolcall.py MODEL "
                f"[--provider P] [--seed N] [--selfcheck]")
        elif model is None:
            model = a
        else:
            die(f"unexpected argument {a!r}")
        i += 1
    if model is None:
        die("usage: openrouter_toolcall.py MODEL [--provider PROVIDER] "
            "[--seed N] [--selfcheck]")
    return model, provider, selfcheck, seed


def read_request(line: str):
    try:
        req = json.loads(line)
    except ValueError as e:
        die(f"request is not JSON: {e}")
    if not isinstance(req, dict) or req.get("op") != "chat":
        die('request "op" must be "chat"')
    if not isinstance(req.get("messages"), list):
        die('request "messages" must be a list')
    return req


def build_body(req, model, provider, seed=None):
    body = {
        "model": model,  # argv wins over the request's model
        "messages": req.get("messages", []),
        "tools": req.get("tools", []),
        "temperature": req.get("temperature", 0),
    }
    if provider:
        body["provider"] = {"order": [provider], "allow_fallbacks": False}
    if seed is not None:
        # Temperature 0 is not determinism: a seed-1 run measured two
        # BYTE-IDENTICAL eval states 9 points apart (p=0.049). `seed` is
        # honored by ollama and by some OpenRouter providers and ignored by
        # the rest, so it narrows the gap without ever closing it — the
        # harness's A0R state is what actually measures what is left.
        body["seed"] = seed
    return body


def normalize(resp):
    """choices[0].message -> the contract message shape (+ usage, meta)."""
    msg = resp["choices"][0]["message"]
    out_msg = {"role": msg.get("role", "assistant"), "content": msg.get("content")}
    calls = []
    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function", tc)
        args = fn.get("arguments", "{}")
        if not isinstance(args, str):
            args = json.dumps(args)
        calls.append(
            {
                "id": tc.get("id", f"call_{len(calls)}"),
                "name": fn.get("name", ""),
                "arguments": args,
            }
        )
    if calls:
        out_msg["tool_calls"] = calls
    usage = resp.get("usage") or {}
    out = {
        "message": out_msg,
        "usage": {
            "prompt_tokens": int(usage.get("prompt_tokens") or 0),
            "completion_tokens": int(usage.get("completion_tokens") or 0),
        },
        "meta": {"model": resp.get("model")},
    }
    if resp.get("provider") is not None:
        out["meta"]["provider"] = resp.get("provider")
    return out


def post(body, key, base):
    url = base.rstrip("/") + "/chat/completions"
    data = json.dumps(body).encode()
    last = "no attempt made"
    for attempt in range(len(RETRY_DELAYS) + 1):
        req = urllib.request.Request(
            url,
            data=data,
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=110) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            try:
                detail = e.read()[:300].decode("utf-8", "replace")
            except Exception:  # noqa: BLE001 — diagnostics only
                detail = ""
            last = f"HTTP {e.code}: {detail}"
            retryable = e.code == 429 or 500 <= e.code < 600
            if retryable and attempt < len(RETRY_DELAYS):
                delay = RETRY_DELAYS[attempt]
                ra = e.headers.get("Retry-After") if e.headers else None
                try:
                    if ra:
                        delay = max(delay, float(ra))
                except ValueError:
                    pass
                time.sleep(min(delay, 60.0))
                continue
            break
        except urllib.error.URLError as e:
            last = f"URLError: {e.reason}"
            if attempt < len(RETRY_DELAYS):
                time.sleep(RETRY_DELAYS[attempt])
                continue
            break
    sys.stderr.write(f"openrouter_toolcall: request failed: {last}\n")
    sys.exit(1)


def main() -> None:
    model, provider, selfcheck, seed = parse_args(sys.argv)
    line = "" if (selfcheck and sys.stdin.isatty()) else sys.stdin.readline()
    if not line.strip():
        if not selfcheck:
            die("no request on stdin")
        line = json.dumps(
            {
                "op": "chat",
                "model": model,
                "messages": [{"role": "user", "content": "ping"}],
                "tools": [],
                "temperature": 0,
            }
        )
    req = read_request(line)
    body = build_body(req, model, provider, seed)
    if selfcheck:
        out = normalize(CANNED_RESPONSE)
    else:
        key = os.environ.get("OPENROUTER_API_KEY", "").strip()
        if not key:
            die("OPENROUTER_API_KEY is not set")
        base = os.environ.get("OPENROUTER_BASE_URL", "https://openrouter.ai/api/v1")
        resp = post(body, key, base)
        try:
            out = normalize(resp)
        except (KeyError, IndexError, TypeError) as e:
            sys.stderr.write(
                f"openrouter_toolcall: unexpected response shape ({e}); "
                f"body: {json.dumps(resp)[:300]}\n"
            )
            sys.exit(1)
    sys.stdout.write(json.dumps(out) + "\n")


if __name__ == "__main__":
    main()
