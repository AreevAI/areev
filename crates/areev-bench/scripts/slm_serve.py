#!/usr/bin/env python3
"""The tuned small model as an agent leg: same JSON-on-stdio contract as
openrouter_toolcall.py, served by a local mlx_lm.server.

    slm_serve.py NAME [--port 8081] [--seed N] [--model REPO]

One JSON request on stdin ({"op":"chat","messages":[...],"temperature":0}),
one JSON response on stdout ({"message":{"role","content"},"usage":{...}}).
So `AGENT_CMD="python3 slm_serve.py qwen2.5-1.5b-lora --port 8081"` drops
the SLM into every driver unchanged — the harness cannot tell a local model
from a hosted one, which is the point: the comparison is the model, not the
plumbing.

Usage is metered like every other leg (script "slm_serve.py", model
"mlx-slm:NAME"), priced at zero marginal by cost.py and given a shadow
price, so the cost chart can show both readings.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request


def meter(model, usage, op):
    path = os.environ.get("AREEV_USAGE_LOG")
    if not path or not usage:
        return
    try:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(json.dumps({"ts": int(time.time() * 1000), "script": os.path.basename(sys.argv[0]),
                                 "model": model, "provider": "local-mlx", "op": op,
                                 "prompt_tokens": int(usage.get("prompt_tokens") or 0),
                                 "completion_tokens": int(usage.get("completion_tokens") or 0),
                                 "latency_ms": usage.get("latency_ms")}) + "\n")
    except OSError:
        pass


def main():
    argv = sys.argv[1:]
    if not argv:
        sys.exit("usage: slm_serve.py NAME [--port P] [--seed N]")
    # NAME is the metering label. `model` is what the server was started
    # with -- mlx_lm.server treats the request's model field as a repo to
    # LOAD, so a label it does not recognise makes it go to the Hub and 404
    # every call. That cost the first evaluation run all 120 documents.
    name, port, seed, i = argv[0], 8081, None, 1
    model = "mlx-community/Qwen2.5-1.5B-Instruct-4bit"
    while i < len(argv):
        if argv[i] == "--port":
            port, i = int(argv[i + 1]), i + 2
        elif argv[i] == "--seed":
            seed, i = int(argv[i + 1]), i + 2
        elif argv[i] == "--model":
            model, i = argv[i + 1], i + 2
        else:
            sys.exit("unknown flag %s" % argv[i])
    req = json.load(sys.stdin)
    if req.get("op") == "probe":
        print(json.dumps({"model": "mlx-slm:" + name}))
        return
    # A reasoning-tuned base may think aloud before the JSON; give it room,
    # and hand back only the object -- the agent's reply always starts with
    # {"fields". A tuned adapter learns to skip the preamble, which the
    # usage ledger shows as completion tokens falling.
    body = {"model": model, "messages": req.get("messages", []),
            "temperature": req.get("temperature", 0), "max_tokens": 1200}
    if seed is not None:
        body["seed"] = seed
    data = json.dumps(body).encode()
    r = urllib.request.Request("http://127.0.0.1:%d/v1/chat/completions" % port, data=data,
                               headers={"Content-Type": "application/json"})
    t0 = time.time()
    try:
        with urllib.request.urlopen(r, timeout=180) as resp:
            out = json.load(resp)
    except urllib.error.URLError as e:
        sys.stderr.write("slm_serve: server at :%d unreachable (%s) — start mlx_lm.server first\n" % (port, e))
        sys.exit(1)
    msg = out["choices"][0]["message"]
    content = msg.get("content") or ""
    k = content.rfind('{"fields"')
    if k > 0:
        content = content[k:]
    msg["content"] = content
    usage = out.get("usage") or {}
    usage["latency_ms"] = int((time.time() - t0) * 1000)
    meter("mlx-slm:" + name, usage, "chat")
    print(json.dumps({"message": {"role": msg.get("role", "assistant"), "content": msg.get("content")},
                      "usage": {"prompt_tokens": usage.get("prompt_tokens", 0),
                                "completion_tokens": usage.get("completion_tokens", 0),
                                "latency_ms": usage["latency_ms"]}}))


if __name__ == "__main__":
    main()
