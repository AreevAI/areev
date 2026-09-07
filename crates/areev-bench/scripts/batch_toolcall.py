#!/usr/bin/env python3
"""Batch adapter: the same chat requests openrouter_toolcall.py makes one at
a time, submitted together to a batch API. Two shapes: `--api openai`
(OpenAI, Together, Groq: upload a JSONL, create a batch, poll, download the
output JSONL) and `--api openrouter` (POST /api/beta/batches with the
requests inline; the completed batch object carries the results and the
actual cost). OpenRouter accepts only models with a `:batch` variant --
69 of 430 on 2026-09-06, at the batch tier's own list price -- and the
agent model of every study so far has none. For the harness's fixed-prompt held-out reads --
every request known before any answer -- this trades minutes of latency for
a batch price tier and, more to the point, freedom from the rate-limit
storms that cost individual reads a document. The governed stream and mem0's
adds stay synchronous: their next request depends on the last answer.

    batch_toolcall.py --api openai --base-url https://api.openai.com/v1 --model gpt-4o-mini \\
        --in requests.jsonl --out replies.jsonl [--key-env OPENAI_API_KEY] \\
        [--poll 30] [--timeout-hours 24] [--discount 0.5] [--batch-id batch_...]
    batch_toolcall.py --api openrouter --base-url https://openrouter.ai/api/beta \\
        --model openai/gpt-5-mini:batch --key-env OPENROUTER_API_KEY --in ... --out ...
    batch_toolcall.py --selfcheck [--api openai|openrouter]   # against a local mock

requests.jsonl: one {"custom_id": ..., "op": "chat", "messages": [...],
"temperature": 0} per line. replies.jsonl: one {"custom_id", "message",
"usage", "meta", "error"} per line, in the contract shape the synchronous
adapter returns. A state file beside --out records the batch id, so a
killed run can collect later with --batch-id. Every reply is metered into
$AREEV_USAGE_LOG with op "batch" and the tier's discount, which cost.py
applies. Nothing but the standard library, like every script here.
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid

TERMINAL = {"completed", "failed", "expired", "cancelled"}


def die(msg, code=2):
    sys.stderr.write("batch_toolcall: %s\n" % msg)
    sys.exit(code)


def http(method, url, key, data=None, headers=None, timeout=120, not_found_ok=False):
    """`not_found_ok` returns None on a 404 instead of dying: a batch just
    created is not visible to GET for a moment (OpenRouter answered 404 on
    the first poll of a batch it had accepted seconds earlier)."""
    h = {"Authorization": "Bearer %s" % key}
    h.update(headers or {})
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    delay = 3
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            detail = e.read()[:300]
            if e.code == 404 and not_found_ok:
                return None
            if e.code in (429, 500, 502, 503, 504) and attempt < 5:
                time.sleep(delay)
                delay = min(delay * 2, 60)
                continue
            die("HTTP %d on %s %s: %r" % (e.code, method, url, detail), 1)
        except (urllib.error.URLError, OSError) as e:
            if attempt < 5:
                time.sleep(delay)
                delay = min(delay * 2, 60)
                continue
            die("connection: %s" % e, 1)
    die("retries exhausted", 1)


def upload(base, key, jsonl_bytes):
    boundary = "----areev" + uuid.uuid4().hex
    body = (("--%s\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n" % boundary).encode()
            + ("--%s\r\nContent-Disposition: form-data; name=\"file\"; filename=\"requests.jsonl\"\r\n"
               "Content-Type: application/jsonl\r\n\r\n" % boundary).encode()
            + jsonl_bytes + ("\r\n--%s--\r\n" % boundary).encode())
    resp = json.loads(http("POST", base + "/files", key, body, {"Content-Type": "multipart/form-data; boundary=" + boundary}))
    return resp["id"]


def normalize(body):
    msg = body["choices"][0]["message"]
    usage = body.get("usage") or {}
    return {"message": {"role": msg.get("role", "assistant"), "content": msg.get("content")},
            "usage": {"prompt_tokens": int(usage.get("prompt_tokens") or 0),
                      "completion_tokens": int(usage.get("completion_tokens") or 0)},
            "meta": {"model": body.get("model")}}


def meter(log_path, model, provider, usage, discount, batch_id, usd_reported=None):
    """One usage row per reply. `discount` is the tier's cut off list price for
    cost.py; `usd_reported` is what the provider itself said the reply cost
    (OpenRouter reports the batch's cost, apportioned here by tokens), which
    cost.py prefers over any list price when present."""
    if not log_path:
        return
    row = {"ts": int(time.time() * 1000), "script": "batch_toolcall.py", "model": model,
           "provider": provider, "op": "batch", "prompt_tokens": usage["prompt_tokens"],
           "completion_tokens": usage["completion_tokens"], "discount": discount, "batch_id": batch_id}
    if usd_reported is not None:
        row["usd_reported"] = round(usd_reported, 8)
    try:
        with open(log_path, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(row) + "\n")
    except OSError:
        pass


def collect_items(items, model, provider, discount, batch_id, batch_cost=None):
    """Normalize a list of per-request results (both APIs share the item shape)."""
    out = {}
    tot = sum(int(((it.get("response") or {}).get("body") or {}).get("usage", {}).get("prompt_tokens") or 0)
              + int(((it.get("response") or {}).get("body") or {}).get("usage", {}).get("completion_tokens") or 0) for it in items) or 1
    for item in items:
        cid = item.get("custom_id")
        resp = item.get("response") or {}
        if item.get("error") or resp.get("status_code", 200) != 200:
            out[cid] = {"custom_id": cid, "error": item.get("error") or resp.get("body"), "message": None, "usage": {}}
            continue
        try:
            n = normalize(resp["body"])
        except (KeyError, IndexError, TypeError) as e:
            out[cid] = {"custom_id": cid, "error": "unexpected response shape (%s)" % e, "message": None, "usage": {}}
            continue
        n["custom_id"] = cid
        n["error"] = None
        n["meta"]["batch_id"] = batch_id
        out[cid] = n
        share = None
        if batch_cost is not None:
            share = float(batch_cost) * (n["usage"]["prompt_tokens"] + n["usage"]["completion_tokens"]) / tot
        meter(os.environ.get("AREEV_USAGE_LOG"), model, provider, n["usage"], discount, batch_id, share)
    return out


def run_openrouter(base, key, model, requests, poll, timeout_hours, discount, batch_id=None, state_path=None, quiet=False):
    """OpenRouter's inline shape. `base` is https://openrouter.ai/api/beta."""
    base = base.rstrip("/")
    provider = "openrouter"
    if batch_id is None:
        body = {"endpoint": "/v1/chat/completions", "model": model,
                "requests": [{"custom_id": r["custom_id"],
                              "body": {"model": model, "messages": r["messages"], "temperature": r.get("temperature", 0)}}
                             for r in requests]}
        created = json.loads(http("POST", base + "/batches", key, json.dumps(body).encode(), {"Content-Type": "application/json"}))
        created = created.get("data") or created
        batch_id = created["id"]
        if state_path:
            json.dump({"batch_id": batch_id, "api": "openrouter", "base_url": base, "model": model,
                       "requests": len(requests), "submitted_ms": int(time.time() * 1000)}, open(state_path, "w"), indent=1)
        if not quiet:
            print("batch_toolcall: submitted %d request(s) as %s" % (len(requests), batch_id), flush=True)
    deadline = time.time() + timeout_hours * 3600
    st, status, grace = {}, None, time.time() + 300
    while time.time() < deadline:
        raw = http("GET", base + "/batches/" + batch_id, key, not_found_ok=time.time() < grace)
        if raw is None:
            time.sleep(min(poll, 5))
            continue
        st = json.loads(raw)
        st = st.get("data") or st
        status = st.get("status")
        if status in TERMINAL:
            break
        time.sleep(poll)
    if status != "completed":
        die("batch %s ended %s" % (batch_id, status or "still running at the deadline"), 1)
    cost = (st.get("usage") or {}).get("cost")
    if state_path and os.path.exists(state_path):
        try:
            sp = json.load(open(state_path)); sp["usage_reported"] = st.get("usage"); sp["request_counts"] = st.get("request_counts")
            json.dump(sp, open(state_path, "w"), indent=1)
        except (OSError, ValueError):
            pass
    return collect_items(st.get("results") or [], model, provider, discount, batch_id, cost)


def run(base, key, model, requests, poll, timeout_hours, discount, batch_id=None, state_path=None, quiet=False):
    """Submit (unless batch_id), wait, collect. Returns {custom_id: reply}."""
    base = base.rstrip("/")
    provider = base.split("//", 1)[-1].split("/", 1)[0]
    if batch_id is None:
        lines = []
        for r in requests:
            lines.append(json.dumps({"custom_id": r["custom_id"], "method": "POST", "url": "/v1/chat/completions",
                                     "body": {"model": model, "messages": r["messages"],
                                              "temperature": r.get("temperature", 0)}}))
        fid = upload(base, key, ("\n".join(lines) + "\n").encode("utf-8"))
        created = json.loads(http("POST", base + "/batches", key,
                                  json.dumps({"input_file_id": fid, "endpoint": "/v1/chat/completions",
                                              "completion_window": "24h"}).encode(),
                                  {"Content-Type": "application/json"}))
        batch_id = created["id"]
        if state_path:
            json.dump({"batch_id": batch_id, "input_file_id": fid, "base_url": base, "model": model,
                       "requests": len(requests), "submitted_ms": int(time.time() * 1000)}, open(state_path, "w"), indent=1)
        if not quiet:
            print("batch_toolcall: submitted %d request(s) as %s" % (len(requests), batch_id), flush=True)
    deadline = time.time() + timeout_hours * 3600
    status, st, grace = None, {}, time.time() + 300
    while time.time() < deadline:
        raw = http("GET", base + "/batches/" + batch_id, key, not_found_ok=time.time() < grace)
        if raw is None:
            time.sleep(min(poll, 5))
            continue
        st = json.loads(raw)
        status = st.get("status")
        if status in TERMINAL:
            break
        time.sleep(poll)
    if status != "completed":
        die("batch %s ended %s" % (batch_id, status or "still running at the deadline"), 1)
    items = []
    for name in ("output_file_id", "error_file_id"):
        fid = st.get(name)
        if not fid:
            continue
        for line in http("GET", base + "/files/" + fid + "/content", key).decode("utf-8").splitlines():
            if line.strip():
                items.append(json.loads(line))
    return collect_items(items, model, provider, discount, batch_id)


def selfcheck(api="openai"):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import mock_batch_server
    srv, base = mock_batch_server.serve(polls_until_done=2)
    if api == "openrouter":
        base = base.rsplit("/v1", 1)[0] + "/api/beta"
    reqs = [{"custom_id": "seq-%d" % i, "op": "chat", "temperature": 0,
             "messages": [{"role": "system", "content": "capture"}, {"role": "user", "content": "DOC %d" % i}]} for i in range(3)]
    log = os.path.join(os.getcwd(), ".batch_selfcheck_usage.jsonl")
    os.environ["AREEV_USAGE_LOG"] = log
    try:
        fn = run_openrouter if api == "openrouter" else run
        out = fn(base, "test-key", "mock-model", reqs, poll=0.2, timeout_hours=0.01, discount=0.5, quiet=True)
        assert sorted(out) == ["seq-0", "seq-1", "seq-2"], out
        assert all(o["error"] is None and "mock batch reply" in o["message"]["content"] for o in out.values())
        rows = [json.loads(l) for l in open(log)]
        assert len(rows) == 3 and all(r["op"] == "batch" and r["discount"] == 0.5 for r in rows)
        if api == "openrouter":
            assert abs(sum(r["usd_reported"] for r in rows) - 0.000123) < 1e-9, "the batch's reported cost is apportioned over its replies"
        print("selfcheck OK (%s shape): 3 requests submitted, polled to completion, collected and metered against a mock batch API" % api)
    finally:
        srv.shutdown()
        try:
            os.remove(log)
        except OSError:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url")
    ap.add_argument("--model")
    ap.add_argument("--in", dest="inp")
    ap.add_argument("--out")
    ap.add_argument("--key-env", default="OPENAI_API_KEY")
    ap.add_argument("--poll", type=float, default=30.0)
    ap.add_argument("--timeout-hours", type=float, default=24.0)
    ap.add_argument("--discount", type=float, default=0.5, help="the batch tier's discount off list price, for cost.py")
    ap.add_argument("--batch-id", default=None, help="collect an already submitted batch instead of submitting")
    ap.add_argument("--api", choices=("openai", "openrouter"), default=None,
                    help="the batch API's shape; default: openrouter when --base-url names openrouter.ai, else openai")
    ap.add_argument("--selfcheck", action="store_true")
    a = ap.parse_args()
    if a.selfcheck:
        return selfcheck(a.api or "openai")
    if not (a.base_url and a.model and a.inp and a.out):
        die("--base-url, --model, --in and --out are required (or --selfcheck)")
    key = os.environ.get(a.key_env) or die("%s unset" % a.key_env)
    requests = [json.loads(l) for l in open(a.inp, encoding="utf-8") if l.strip()]
    for r in requests:
        if r.get("op", "chat") != "chat" or "custom_id" not in r or not isinstance(r.get("messages"), list):
            die("each request needs custom_id, op chat and messages")
    state = a.out + ".batch.json"
    api = a.api or ("openrouter" if "openrouter.ai" in a.base_url else "openai")
    fn = run_openrouter if api == "openrouter" else run
    out = fn(a.base_url, key, a.model, requests, a.poll, a.timeout_hours, a.discount, a.batch_id, state)
    with open(a.out, "w", encoding="utf-8") as fh:
        for r in requests:
            fh.write(json.dumps(out.get(r["custom_id"], {"custom_id": r["custom_id"], "error": "no result returned", "message": None, "usage": {}}),
                                ensure_ascii=False) + "\n")
    missing = sum(1 for r in requests if r["custom_id"] not in out)
    errors = sum(1 for o in out.values() if o.get("error"))
    print("batch_toolcall: %d reply(ies), %d error(s), %d missing -> %s" % (len(out), errors, missing, a.out))


if __name__ == "__main__":
    main()
