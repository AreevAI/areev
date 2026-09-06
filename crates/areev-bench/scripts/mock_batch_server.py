#!/usr/bin/env python3
"""A local stand-in for two batch APIs, for testing the harness's batch path
without spending: the OpenAI files shape (upload a file, create a batch,
poll it, download its output) and OpenRouter's inline shape (POST
/api/beta/batches with the requests in the body; the completed batch object
carries the results). Every chat request gets a fixed park reply that carries
token counts, so plumbing, scoring, journaling and metering can be checked
end to end. Nothing here resembles a model.

    mock_batch_server.py [--port 0] [--polls-until-done 2]
"""
import argparse
import json
import re
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"files": {}, "batches": {}, "polls_until_done": 2}
REPLY = {"fields": {}, "park": True, "reason": "mock batch reply"}


def _multipart_file(body, ctype):
    m = re.search(r'boundary="?([^";]+)"?', ctype or "")
    if not m:
        return body
    boundary = ("--" + m.group(1)).encode()
    for part in body.split(boundary):
        if b'name="file"' in part:
            head, _, data = part.partition(b"\r\n\r\n")
            return data.rsplit(b"\r\n", 1)[0]
    return body


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def _json(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _answer(self, req):
        text = json.dumps(req.get("body", {}).get("messages", []))
        return {"id": "batch_req_" + uuid.uuid4().hex[:8], "custom_id": req["custom_id"],
                "response": {"status_code": 200, "request_id": "r", "body": {
                    "id": "chatcmpl-mock", "model": req.get("body", {}).get("model", "mock"),
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": json.dumps(REPLY)}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": max(len(text) // 4, 1), "completion_tokens": 12}}},
                "error": None}

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n)
        if self.path.endswith("/beta/batches"):
            # OpenRouter's shape: requests inline, results inline on the batch object
            req = json.loads(body or b"{}")
            bid = "batch-" + uuid.uuid4().hex[:12]
            STATE["batches"][bid] = {"inline": req.get("requests", []), "polls": 0, "model": req.get("model")}
            return self._json(202, {"id": bid, "object": "batch", "endpoint": req.get("endpoint"), "model": req.get("model"),
                                    "status": "validating", "request_counts": {"total": len(req.get("requests", [])), "completed": 0, "failed": 0},
                                    "usage": None, "results": None, "error": None})
        if self.path.endswith("/files"):
            fid = "file-" + uuid.uuid4().hex[:12]
            STATE["files"][fid] = _multipart_file(body, self.headers.get("Content-Type"))
            return self._json(200, {"id": fid, "object": "file", "purpose": "batch"})
        if self.path.endswith("/batches"):
            req = json.loads(body or b"{}")
            bid = "batch_" + uuid.uuid4().hex[:12]
            STATE["batches"][bid] = {"input": req.get("input_file_id"), "polls": 0, "output": None}
            return self._json(200, {"id": bid, "object": "batch", "status": "validating"})
        self._json(404, {"error": "no such route"})

    def do_GET(self):
        m = re.search(r"/batches/([^/]+)$", self.path)
        if m:
            b = STATE["batches"].get(m.group(1))
            if not b:
                return self._json(404, {"error": "no such batch"})
            b["polls"] += 1
            if b["polls"] < STATE["polls_until_done"]:
                return self._json(200, {"id": m.group(1), "status": "in_progress", "results": None})
            if "inline" in b:
                results = [self._answer(r) for r in b["inline"]]
                pt = sum(r["response"]["body"]["usage"]["prompt_tokens"] for r in results)
                ct = sum(r["response"]["body"]["usage"]["completion_tokens"] for r in results)
                return self._json(200, {"id": m.group(1), "object": "batch", "status": "completed", "model": b["model"],
                                        "request_counts": {"total": len(results), "completed": len(results), "failed": 0},
                                        "usage": {"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct, "cost": 0.000123},
                                        "results": results, "error": None})
            if b["output"] is None:
                lines = []
                for line in STATE["files"][b["input"]].decode("utf-8").splitlines():
                    if not line.strip():
                        continue
                    lines.append(json.dumps(self._answer(json.loads(line))))
                oid = "file-" + uuid.uuid4().hex[:12]
                STATE["files"][oid] = ("\n".join(lines) + "\n").encode()
                b["output"] = oid
            return self._json(200, {"id": m.group(1), "status": "completed", "output_file_id": b["output"], "error_file_id": None,
                                    "request_counts": {"total": 0, "completed": 0, "failed": 0}})
        m = re.search(r"/files/([^/]+)/content$", self.path)
        if m and m.group(1) in STATE["files"]:
            data = STATE["files"][m.group(1)]
            self.send_response(200)
            self.send_header("Content-Type", "application/jsonl")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        self._json(404, {"error": "no such route"})


def serve(port=0, polls_until_done=2):
    """Start in a thread; returns (server, base_url)."""
    STATE["polls_until_done"] = polls_until_done
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, "http://127.0.0.1:%d/v1" % srv.server_address[1]


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--polls-until-done", type=int, default=2)
    a = ap.parse_args()
    srv, base = serve(a.port, a.polls_until_done)
    print("mock batch API at", base, flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        sys.exit(0)
