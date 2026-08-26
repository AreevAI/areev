#!/usr/bin/env python3
"""Live Gmail / Google Workspace connector for an Areev polling trigger.

Contract (docs/triggers.md): JSON on stdin, JSON on stdout, one process per
invocation -- the same seam as `--tool-cmd`, which is why the keyless mock
(`agent.py connector` in each language stack) is a drop-in stand-in for this
file. Wire it with:

    areev trigger run --connector-cmd "python3 connectors/gmail.py" ...
    # or, embedded: db.trigger_run(connector_cmd="python3 connectors/gmail.py", ...)

  stdin : {trigger, connector, scope, cursor?, max_items, config?}
  stdout: {items: [{id, payload, blobs}], cursor?, more}

Rules that matter and are easy to get wrong:
  * an ABSENT `cursor` in the response means "leave it where it is" --
    emitting null would rewind the source and replay the mailbox;
  * `more: true` means there is a backlog, and Areev re-polls immediately
    instead of waiting out the interval;
  * the first poll seeds the cursor and returns nothing, so declaring a
    mailbox trigger never replays history.

`id` is the RFC822 Message-ID -- the trigger's `--dedup-key /message_id` --
so Areev derives a deterministic run id and a re-delivered message becomes
one recorded skip rather than a second run. Attachments ride back as
`blobs` with `"@N"` references: the EVALUATOR (the party holding the
writer) stores each one in the CAS and rewrites the reference to a
`cas://sha256:...` address, so tools read them with `areev blob get`.

Payload shape (what the real tools receive as `item`):

    {"message_id": ..., "thread": ..., "mailbox": ...,
     "email": {"from", "to", "subject", "date", "body"},
     "attachments": [{"filename", "mime", "blob": "@0"}]}

Auth (see ../../docs/email-providers.md for the one-time setup):
  GMAIL_TOKEN         a ready OAuth bearer token (CI / brokered setups), or
  gcloud ADC          `gcloud auth application-default login` with the
                      gmail.readonly (or gmail.modify) scope
  GMAIL_QUOTA_PROJECT the X-Goog-User-Project quota project (else 403)
  AP_MAILBOX          the group/alias address the desk polls (falls back to
                      the trigger scope's `mailbox:<address>`)

Never run by CI: the keyless floor uses the fixture connector instead.
"""

import base64
import email
import json
import os
import re
import subprocess
import sys
import urllib.parse
import urllib.request

GMAIL = "https://gmail.googleapis.com/gmail/v1/users/me"


def token():
    tok = os.environ.get("GMAIL_TOKEN")
    if tok:
        return tok
    p = subprocess.run(
        ["gcloud", "auth", "application-default", "print-access-token"],
        capture_output=True,
        text=True,
    )
    if p.returncode != 0:
        raise RuntimeError(f"ADC unavailable: {p.stderr.strip()[:160]}")
    return p.stdout.strip()


def gapi(path, tok, **params):
    url = f"{GMAIL}/{path}"
    if params:
        url += "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url)
    req.add_header("Authorization", f"Bearer {tok}")
    quota = os.environ.get("GMAIL_QUOTA_PROJECT")
    if quota:
        req.add_header("X-Goog-User-Project", quota)
    with urllib.request.urlopen(req) as r:
        return json.load(r)


def flatten(raw_bytes, mailbox):
    """One RFC 5322 message -> the workflow's input shape + its blobs."""
    msg = email.message_from_bytes(raw_bytes)
    body, attachments, blobs = "", [], []
    for part in msg.walk() if msg.is_multipart() else [msg]:
        if part.get_content_maintype() == "multipart":
            continue
        filename = part.get_filename()
        payload = part.get_payload(decode=True) or b""
        if filename:
            attachments.append({"filename": filename, "mime": part.get_content_type(),
                                "blob": f"@{len(blobs)}"})
            blobs.append({"filename": filename, "mime": part.get_content_type(),
                          "b64": base64.b64encode(payload).decode()})
        elif not body and part.get_content_type() == "text/plain":
            body = payload.decode("utf-8", "replace")
    mid = (msg.get("Message-ID") or "").strip("<> \t\r\n")
    # The thread key: the first References entry, else the message itself.
    # This is what the trigger's context query binds ($session = /thread).
    thread = (msg.get("References", "").split() or [f"<{mid}>"])[0].strip("<>")
    return mid, blobs, {
        "message_id": mid,
        "thread": thread,
        "mailbox": mailbox,
        "email": {
            "from": msg.get("From", ""),
            "to": msg.get("To", ""),
            "subject": msg.get("Subject", ""),
            "date": msg.get("Date", ""),
            "body": body,
        },
        "attachments": attachments,
    }


# ── mail that must never become work ──────────────────────────────────────
# Two kinds, both found running this example against a live tenant:
#
#   * our own asks coming back. A reply carries the run marker; the reply
#     path owns it. Letting it through starts a second run that can only
#     ever conclude "nothing could be extracted".
#   * postmaster mail. A bounce carries neither the marker (Exchange
#     rewrites the subject and drops the body) nor an invoice, so it parks a
#     run asking a human about a delivery failure — and that ask can bounce
#     too, which starts the loop again.
#
# Duplicated in both connectors on purpose: each is a standalone stdio
# script with no imports beyond the stdlib, and that is worth more than
# saving fifteen lines.
MARKER_RE = re.compile(r"\[areev:[a-z]{2}/[0-9a-f]{12}\]")
SYSTEM_SUBJECT_RE = re.compile(
    r"^\s*(undeliverable|undelivered mail|delivery status notification|"
    r"mail delivery (failed|subsystem)|returned mail|automatic reply|"
    r"out of office|auto(matic)?[- ]?reply)\b",
    re.I,
)
SYSTEM_SENDER_RE = re.compile(
    r"^(microsoftexchange[0-9a-f]*@|postmaster@|mailer-daemon@|no-?reply@)", re.I
)


def not_new_work(payload):
    """True for mail that must not start a run."""
    email = payload.get("email") or {}
    subject = email.get("subject") or ""
    sender = email.get("from") or ""
    body = (email.get("body") or "")[:4000]
    if MARKER_RE.search("%s %s" % (subject, body)):
        return True
    return bool(SYSTEM_SENDER_RE.match(sender)) or bool(SYSTEM_SUBJECT_RE.match(subject))


def poll(req):
    mailbox = os.environ.get("AP_MAILBOX") or (req.get("scope") or "").removeprefix("mailbox:")
    tok = token()
    cursor = req.get("cursor")
    # A cursor this connector cannot parse (e.g. left behind by a different
    # connector on the same trigger) would fail every poll forever. Treat it
    # as unseeded: fire nothing, record a fresh cursor, say so loudly.
    if cursor is not None:
        try:
            int(cursor)
        except (TypeError, ValueError):
            print(f"gmail connector: unparseable cursor {cursor!r}; reseeding", file=sys.stderr)
            cursor = None
    # `deliveredto:` catches group mail; the `to:` leg catches your own Sent
    # copy, which Gmail never re-delivers to the group.
    q = f"{{deliveredto:{mailbox} to:{mailbox}}}"
    if cursor:
        q += f" after:{cursor}"
    listing = gapi("messages", tok, q=q, maxResults=req.get("max_items", 100))
    ids = [m["id"] for m in listing.get("messages", [])]
    if not cursor:
        newest = 0
        if ids:
            newest = int(gapi(f"messages/{ids[0]}", tok, format="minimal")["internalDate"]) // 1000
        return {"items": [], "cursor": str(newest or 1), "more": False}
    items, newest = [], int(cursor)
    for gid in reversed(ids):
        full = gapi(f"messages/{gid}", tok, format="raw")
        mid, blobs, payload = flatten(base64.urlsafe_b64decode(full["raw"]), mailbox)
        newest = max(newest, int(full["internalDate"]) // 1000)
        if not mid or not_new_work(payload):
            continue
        payload["gmail_id"] = gid
        items.append({"id": mid, "payload": payload, "blobs": blobs})
    out = {"items": items, "more": False}
    # Advance on anything LOOKED AT, not only on what was returned. A batch
    # that is entirely bounces yields no items, and cursoring on `items`
    # leaves it parked — re-fetching the same mail on every poll, forever.
    if ids:
        out["cursor"] = str(newest + 1)
    return out


def main():
    req = json.load(sys.stdin)
    json.dump(poll(req), sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
