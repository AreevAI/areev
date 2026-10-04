"""Stands in for areev-sandbox in the #374 test: a `wasm32-areev-io` module
that calls an upstream through the broker under the `vendor` credential, once
plainly and once trying to set that credential's own header itself. It reports
both answers plus every value in its environment, so the test can assert the
key never reached the module by any route."""
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
    "admitted": ask({"url": upstream + "/ok", "method": "POST", "credential": "vendor", "body": "{}"}),
    "collision": ask({"url": upstream + "/ok", "method": "POST", "credential": "vendor",
                      "headers": {"x-api-key": "guest-chosen"}, "body": "{}"}),
    "env": "\n".join(os.environ.values()),
}
sys.stdout.write(json.dumps(out))
