# Email providers for agent examples

Every mail-driven agent in this directory reaches its mailbox through ONE
seam: an Areev **polling-trigger connector** — JSON on stdin, JSON on
stdout, one process per invocation, the same contract as `--tool-cmd`
([`docs/triggers.md` §"The connector contract"](../../../docs/triggers.md)).
That is why each agent's keyless mock connector and the live connectors
here are interchangeable, and why none of this adds a vendor SDK to the
repo: the provider glue is a standalone script you own.

The two live connectors ship with
[`invoice-to-accounting/connectors/`](../invoice-to-accounting/connectors/)
and are written to be lifted into any other mail-driven agent unchanged:

| Provider | Script | Auth |
|---|---|---|
| **Microsoft 365 / Outlook** (the default — most desks run Exchange Online) | `outlook_graph.py` | Entra ID app registration + device-code sign-in, or a brokered `OUTLOOK_TOKEN` |
| **Google Workspace / Gmail** (the pattern proven in a production deployment) | `gmail.py` | gcloud Application Default Credentials, or a brokered `GMAIL_TOKEN` |

Both are Python-stdlib-only on purpose: a connector is a dumb pipe (fetch,
normalize, cursor) and runs as a subprocess of whichever language your agent
is written in — a Rust or TypeScript agent uses these scripts as-is, or you
rewrite ~150 lines in your language against the same contract.

## The payload contract (what your tools receive as `item`)

```json
{"message_id": "<...>",            ← the dedup key: --dedup-key /message_id
 "thread": "<...>",                ← what the context query binds ($session = /thread)
 "mailbox": "ap@yourdesk.example",
 "email": {"from": "...", "to": "...", "subject": "...", "date": "...", "body": "..."},
 "attachments": [{"filename": "inv.pdf", "mime": "application/pdf", "blob": "@0"}]}
```

Attachments ride back as `blobs` (`{filename, mime, b64}`) referenced by
`"@N"`. The **evaluator** — the party already holding the memory's writer —
stores each blob in the CAS and rewrites the reference to a
`cas://sha256:…` address, so your parse tool reads bytes with
`areev blob get` (the one lock-free door into the file). Budgets: 16 MiB
per item, 48 MiB per response, enforced on decoded size; a violation
refuses the whole poll with the cursor unmoved (`TRG-E011`).

## Cursor rules (the ones that eat mailboxes when gotten wrong)

- **Absent cursor in the request** = first poll: record where the mailbox
  is now, return `items: []`. Declaring a trigger must never replay
  history.
- **Absent cursor in the response** = "leave it where it is". Emitting
  `null` rewinds the source.
- **`more: true`** = backlog: Areev re-polls immediately instead of waiting
  out the interval, so a cold start drains without hammering.
- An unparseable cursor (another connector's leftovers on the same trigger)
  should reseed loudly, not fail every poll forever.
- Overlap is safe, gaps are not: the trigger's `/message_id` dedup makes a
  re-fetched message one recorded skip, so when in doubt fetch too much.

## Microsoft 365 setup (once)

1. **App registration** (Entra ID → App registrations → New): public
   client. Delegated permissions: `Mail.Read` (+ `Mail.Send` if your reply
   tool sends through Graph; `Mail.Read.Shared` for a shared mailbox).
   Enable "Allow public client flows" for device code.
2. Export `MS_CLIENT_ID` (and `MS_TENANT` if not `organizations`), then
   `python3 connectors/outlook_graph.py login` — sign in with the mailbox
   account; the refresh token lands in `~/.config/areev/outlook-token.json`
   (chmod 600, override via `OUTLOOK_TOKEN_CACHE`).
3. A shared/desk mailbox: set `AP_MAILBOX=ap@yourdesk.example` to poll
   `/users/<address>` instead of `/me`.
4. The connector polls `…/mailFolders/inbox/messages`, **not** `/messages`.
   Graph's `/messages` spans every folder, so a desk that sends its own
   approval mail re-ingests it out of Sent Items on the next tick — with a
   new message id, which is exactly what `/message_id` dedup cannot catch.
   Attachment listings deliberately carry no `$select`: `contentBytes` is
   declared on `fileAttachment`, not on the base `attachment` type the
   collection is typed as, so selecting it 400s the whole request.

## Google Workspace setup (once)

The production-proven shape avoids a dedicated service account and
domain-wide delegation entirely: make the desk address a **Google Group**
whose member is a real mailbox, and read *that* mailbox:

1. `gcloud auth application-default login` with scopes including
   `gmail.readonly` (or `gmail.modify` if the agent labels/archives), as
   the member account.
   **Each ADC login replaces the previous grant** — re-authing with fewer
   scopes silently breaks the mail path.
2. Set `GMAIL_QUOTA_PROJECT` (the `X-Goog-User-Project`; without it Google
   answers 403) and `AP_MAILBOX` to the group address. The connector's
   query is `{deliveredto:<group> to:<group>}` — the `to:` leg catches your
   own Sent copy, which Gmail never re-delivers to the group.
3. Brokered setups can skip gcloud: `GMAIL_TOKEN` short-circuits to a ready
   bearer token.

## The reply leg (approval by email)

Sending the ask and reading the answer are **tools**, not connector work.
Hard-won guidance from the production deployment:

- **Mark every ask.** Put a stable marker in the subject —
  `[areev:ap/<sha256(message_id)[:12]>]` — because a `mailto:` button
  cannot set `In-Reply-To` (clients ignore RFC 6068's attempt), so a
  clicked reply may land outside the thread. Resolve replies by thread
  *first*, marker search second.
- **Not every message in the mailbox is work.** Both connectors drop two
  kinds before they become items, and a hand-rolled connector must too:
  *our own asks coming back* (they carry the run marker, the reply path owns
  them, and a second run can only ever conclude "nothing could be
  extracted"), and *postmaster mail* — a bounce carries neither the marker
  nor an invoice, so it parks a run asking a human about a delivery failure,
  and that ask can bounce in turn. Mind the cursor when you add such a
  filter: advance it on everything you **looked at**, not on what you
  returned, or a batch that is entirely bounces leaves it parked and
  re-fetches the same mail forever.
- **Email the approver, never the external sender.** Getting that backwards
  emails your vendor an approval link.
- **Classify deterministically first**: strip quoted history (Gmail's
  "On … wrote:", Outlook's header block), take the verb
  (`approve`/`revise`/`reject`), parse `Field: value` correction lines. Add
  an LLM interpretation leg only for what that cannot classify, and leave a
  genuine question unactioned for a person.
- **Respond as the human's principal** (`run respond --as user:<who>`) —
  the approver's identity is the audit record, and the runtime refuses the
  principal that started the run. In production, put approvals behind
  per-principal credentials (`areev ui --auth`).

## Credentials never enter the connector

On a deployment, prefer the credential broker to ambient env tokens:
`areev trigger run --credential gmail=GMAIL_TOKEN --allow-host
'https://gmail.googleapis.com'` hands the connector a broker address, not
the secret ([`docs/triggers.md`](../../../docs/triggers.md) §"Brokered
connector egress"). `cmd:`/`vault:` resolvers mint short-lived tokens per
call ([`docs/cookbook.md` §19](../../../docs/cookbook.md)).
