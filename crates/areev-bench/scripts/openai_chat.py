#!/usr/bin/env python3
"""Reader/judge helper for `areev-bench accuracy`: reads a prompt on stdin,
calls the OpenAI Chat Completions API, prints the completion on stdout.

    usage:  openai_chat.py [model]        (default: gpt-4o-mini)
    key:    $OPENAI_API_KEY, else /tmp/areev_openai_key, else ~/.areev_openai_key

Wire it into the harness as the reader and/or judge command:
    AREEV_LLM_CMD='python3 .../openai_chat.py gpt-4o-mini' \
    AREEV_JUDGE_CMD='python3 .../openai_chat.py gpt-4o' \
    cargo run --release -p areev-bench --bin accuracy -- locomo10.json 10

Stdlib only (urllib) — no pip install.
"""
import json
import os
import random
import sys
import time
import urllib.error
import urllib.request


def get_key() -> str:
    k = os.environ.get("OPENAI_API_KEY")
    if k:
        return k.strip()
    for p in ("/tmp/areev_openai_key", os.path.expanduser("~/.areev_openai_key")):
        if os.path.exists(p):
            return open(p).read().strip()
    sys.exit("no OpenAI key: set $OPENAI_API_KEY or write /tmp/areev_openai_key")


def call(model: str, prompt: str, key: str) -> str:
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0,
            "max_tokens": 256,
        }
    ).encode()
    req = urllib.request.Request(
        "https://api.openai.com/v1/chat/completions",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    # 8 retries with exponential backoff + jitter; honor Retry-After on 429s so
    # a burst of concurrent workers self-throttles instead of hammering the API.
    for attempt in range(8):
        try:
            with urllib.request.urlopen(req, timeout=90) as r:
                out = json.load(r)
            return out["choices"][0]["message"]["content"].strip()
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503) and attempt < 7:
                ra = e.headers.get("Retry-After") if e.headers else None
                try:
                    delay = float(ra) if ra else 2 ** attempt
                except (TypeError, ValueError):
                    delay = 2 ** attempt
                time.sleep(min(delay, 30) + random.uniform(0, 1.0))
                continue
            sys.stderr.write(f"openai_chat HTTPError {e.code}: {e.read()[:200]!r}\n")
            return ""
        except Exception as e:  # noqa: BLE001
            if attempt < 7:
                time.sleep(min(2 ** attempt, 30) + random.uniform(0, 1.0))
                continue
            sys.stderr.write(f"openai_chat error: {e}\n")
            return ""
    return ""


def main() -> None:
    model = sys.argv[1] if len(sys.argv) > 1 else "gpt-4o-mini"
    print(call(model, sys.stdin.read(), get_key()))


if __name__ == "__main__":
    main()
