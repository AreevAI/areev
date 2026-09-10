#!/usr/bin/env python3
"""A stand-in vendor API on loopback: one route, and it checks its auth.

Not a mock of Areev — a mock of the *upstream*. It exists so this example can
prove the whole path end to end with no credential of anyone's and no network:
the guest names a credential, the broker attaches it, and this server sees the
header the guest never held.
"""
import http.server
import json
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7788
SEEN = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - stdlib naming
        auth = self.headers.get("Authorization", "")
        SEEN.append((self.path, auth, self.headers.get("X-Api-Version", "")))
        if not self.path.startswith("/v1/invoices/"):
            self.send_error(404, "no such route")
            return
        if auth != "Bearer demo-vendor-token":
            # The point of the whole boundary: a request that arrives without
            # the broker's credential does not get the data.
            self.send_response(401)
            self.end_headers()
            self.wfile.write(b'{"error":"unauthenticated"}')
            return
        body = json.dumps({
            "id": self.path.rsplit("/", 1)[-1],
            "vendor": "Acme Freight",
            "amount_usd": 1240.00,
            "status": "approved",
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass  # quiet: the example's own output is the story


if __name__ == "__main__":
    http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
