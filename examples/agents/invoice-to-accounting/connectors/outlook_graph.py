#!/usr/bin/env python3
"""Live Microsoft 365 / Outlook connector for an Areev polling trigger.

Same contract and payload shape as gmail.py (read that header first); this
one speaks Microsoft Graph. Wire it with:

    areev trigger run --connector-cmd "python3 connectors/outlook_graph.py" ...
    # or, embedded: db.trigger_run(connector_cmd="python3 connectors/outlook_graph.py", ...)

One-time setup (details in ../../docs/email-providers.md):

  1. App registration in Entra ID: public client, delegated permissions
     Mail.Read (+ Mail.Send if your reply tool uses Graph too).
  2. `python3 connectors/outlook_graph.py login` -- device-code sign-in;
     the refresh token lands in $OUTLOOK_TOKEN_CACHE (chmod 600).

Environment:
  MS_CLIENT_ID        the app registration's client id            (required)
  MS_TENANT           tenant id or domain (default "organizations")
  OUTLOOK_TOKEN_CACHE token cache path (default ~/.config/areev/outlook-token.json)
  OUTLOOK_TOKEN       a ready bearer token -- skips the cache (CI/brokered)
  AP_MAILBOX          poll a shared mailbox via /users/<address> instead of
                      /me (needs Mail.Read.Shared or application consent)

Cursor is the newest message's receivedDateTime (ISO 8601, UTC). The filter
is `receivedDateTime gt <cursor>`, so two invoices landing in the same
second can leave one behind until the next mail arrives -- and any overlap
re-fetched after a crash is swallowed by the trigger's `/message_id` dedup,
which is the guarantee that actually matters.

Never run by CI: the keyless floor uses the fixture connector instead.
"""

import base64
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

GRAPH = "https://graph.microsoft.com/v1.0"
TENANT = os.environ.get("MS_TENANT", "organizations")
CACHE = os.environ.get(
    "OUTLOOK_TOKEN_CACHE",
    os.path.expanduser("~/.config/areev/outlook-token.json"),
)
SCOPE = "https://graph.microsoft.com/Mail.Read offline_access"


def http(url, data=None, headers=None):
    req = urllib.request.Request(url, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{url.split('?')[0]}: HTTP {e.code}: {e.read()[:200]}") from e


def oauth(grant):
    return http(
        f"https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/token",
        data=urllib.parse.urlencode(grant).encode(),
    )


def login():
    """Device-code sign-in; stores the refresh token in $OUTLOOK_TOKEN_CACHE."""
    client = os.environ["MS_CLIENT_ID"]
    dc = http(
        f"https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/devicecode",
        data=urllib.parse.urlencode({"client_id": client, "scope": SCOPE}).encode(),
    )
    print(dc["message"], file=sys.stderr)  # "go to ... and enter code ..."
    while True:
        time.sleep(dc.get("interval", 5))
        try:
            tok = oauth({"grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                         "client_id": client, "device_code": dc["device_code"]})
        except RuntimeError as e:
            if "authorization_pending" in str(e):
                continue
            raise
        save_cache(tok)
        print(f"signed in; token cache at {CACHE}", file=sys.stderr)
        return 0


def save_cache(tok):
    os.makedirs(os.path.dirname(CACHE), exist_ok=True)
    tok["expires_at"] = int(time.time()) + int(tok.get("expires_in", 0)) - 60
    fd = os.open(CACHE, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        json.dump(tok, fh)


def token():
    tok = os.environ.get("OUTLOOK_TOKEN")
    if tok:
        return tok
    try:
        with open(CACHE) as fh:
            cache = json.load(fh)
    except OSError as e:
        raise RuntimeError(f"no token cache -- run `outlook_graph.py login` first: {e}") from e
    if int(time.time()) < cache.get("expires_at", 0):
        return cache["access_token"]
    fresh = oauth({"grant_type": "refresh_token", "client_id": os.environ["MS_CLIENT_ID"],
                   "refresh_token": cache["refresh_token"], "scope": SCOPE})
    fresh.setdefault("refresh_token", cache["refresh_token"])
    save_cache(fresh)
    return fresh["access_token"]


def graph(path, tok, **params):
    url = f"{GRAPH}/{path}"
    if params:
        url += "?" + urllib.parse.urlencode(params)
    return http(url, headers={
        "Authorization": f"Bearer {tok}",
        # Text bodies, not HTML -- this is model input, not a mail client.
        "Prefer": 'outlook.body-content-type="text"',
    })


def base_path():
    shared = os.environ.get("AP_MAILBOX")
    return f"users/{shared}" if shared else "me"


def flatten(msg, tok, mailbox):
    """One Graph message -> the workflow's input shape + its blobs."""
    attachments, blobs = [], []
    if msg.get("hasAttachments"):
        listing = graph(f"{base_path()}/messages/{msg['id']}/attachments", tok,
                        **{"$select": "name,contentType,contentBytes"})
        for a in listing.get("value", []):
            if not a.get("contentBytes"):
                continue  # itemAttachment / referenceAttachment: not file bytes
            attachments.append({"filename": a["name"], "mime": a.get("contentType", ""),
                                "blob": f"@{len(blobs)}"})
            blobs.append({"filename": a["name"], "mime": a.get("contentType", ""),
                          "b64": a["contentBytes"]})
    mid = (msg.get("internetMessageId") or "").strip("<>")
    return mid, blobs, {
        "message_id": mid,
        # conversationId is Graph's thread key -- what the trigger's context
        # query binds ($session = /thread).
        "thread": msg.get("conversationId", mid),
        "mailbox": mailbox,
        "email": {
            "from": (msg.get("from") or {}).get("emailAddress", {}).get("address", ""),
            "to": ", ".join(r["emailAddress"]["address"] for r in msg.get("toRecipients", [])),
            "subject": msg.get("subject", ""),
            "date": msg.get("receivedDateTime", ""),
            "body": (msg.get("body") or {}).get("content", ""),
        },
        "attachments": attachments,
    }


SELECT = ("id,internetMessageId,conversationId,subject,from,toRecipients,"
          "receivedDateTime,hasAttachments,body")


def poll(req):
    mailbox = os.environ.get("AP_MAILBOX") or (req.get("scope") or "").removeprefix("mailbox:")
    tok = token()
    cursor = req.get("cursor")
    if cursor is None:
        # Seed: record where the mailbox is now, fire nothing.
        newest = graph(f"{base_path()}/messages", tok, **{
            "$orderby": "receivedDateTime desc", "$top": 1,
            "$select": "receivedDateTime"})
        rows = newest.get("value", [])
        seed = rows[0]["receivedDateTime"] if rows else "1970-01-01T00:00:00Z"
        return {"items": [], "cursor": seed, "more": False}
    listing = graph(f"{base_path()}/messages", tok, **{
        "$filter": f"receivedDateTime gt {cursor}",
        "$orderby": "receivedDateTime asc",
        "$top": req.get("max_items", 100),
        "$select": SELECT})
    items, newest = [], cursor
    for msg in listing.get("value", []):
        mid, blobs, payload = flatten(msg, tok, mailbox)
        newest = max(newest, msg.get("receivedDateTime", newest))
        if mid:
            payload["graph_id"] = msg["id"]
            items.append({"id": mid, "payload": payload, "blobs": blobs})
    out = {"items": items, "more": "@odata.nextLink" in listing}
    if items:
        out["cursor"] = newest
    return out


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "login":
        return login()
    req = json.load(sys.stdin)
    json.dump(poll(req), sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
