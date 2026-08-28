---
name: areev-server-console
description: Playbook for areev-server — the hand-rolled std-only HTTP/1.1 server, its two auth modes (loopback-unauthenticated ui / with_auth token) plus the per-principal credential map, the drive-by/body-cap/Origin security invariants, and the embedded console.html (whose design source is the Paper file "Areev"). Use before editing crates/areev-server/src/{lib.rs,console.html} or adding an endpoint, and always re-read docs/security-model.md when touching auth, bind, or the request surface.
---

# The server & console

`areev-server` is a **hand-rolled, std-only** HTTP/1.1 server —
`std::net::TcpListener`, **one request per connection**, no framework, no async
runtime (invariant 6: dependency-light). It serves the web console — and
nothing else; there is no networked sync surface. The console is a single
embedded file:
`const CONSOLE_HTML = include_str!("console.html")` (`lib.rs:17`).

## The two modes (auth is the load-bearing distinction)

- **`ui` (default)** — binds **loopback** and is **unauthenticated**. Fine for a
  local console; never expose it off-host.
- **`with_auth(token)`** (`lib.rs:103`; CLI `areev ui --token-env VAR`) — requires
  the token on **every** request. Browsers via the native HTTP Basic prompt (any
  username, password = token); scripts via `Authorization: Bearer`. A 401 must
  carry `WWW-Authenticate: Basic` so browsers prompt. Base64 for Basic is
  **hand-rolled** (no dep) — keep it correct.

A shared token is all-or-nothing over the whole memory. `with_credentials`
(CLI `areev ui --auth FILE`) is the per-principal alternative — and the only
one `run.respond` accepts unconditionally, since the approver's identity is
the audit record. Credentials carry an `id` (revoke one without disturbing the
principal's others), an optional `expires_at` (refused indistinguishably from
unknown when past), and are minted by `areev auth mint` — 256-bit, prefixed
`areev_pat_`, digest-only at rest.

- **Trusted-header SSO** (`--sso-header` + `--sso-secret-env`) — a proxy does
  OIDC/SAML and forwards identity, honored only with the proxy secret. The
  identity is **validated** (`sanitize_sso_identity`: no control chars, no
  whitespace, ≤128 bytes, no reserved names) and a rejected one is treated as
  *absent*, never as an error. `--sso-groups-header` maps IdP groups to
  principals via the credential map's `groups` table; identity outranks group.
- **Native OIDC** (`oidc` feature, `src/oidc.rs`) — the console runs
  auth-code+PKCE itself and issues an `HttpOnly`/`SameSite=Strict` session
  cookie. `/auth/login`, `/auth/callback`, `/auth/logout` bypass `route_full`
  because they set headers of their own.

**The approval ladder — the load-bearing rule.** `POST /api/run/respond`
accepts an identity only in proportion to how it was proven:

| Identity source | May approve? |
|---|---|
| `credential` (per-principal token) | yes |
| `oidc` (signature verified against the issuer's JWKS) | yes |
| `sso` (proxy header + shared secret) | only with `--sso-approvals allow` |
| `sso-group` (a role, not a person) | **never**, no flag exists |
| shared `--token-env`, anonymous | never |

`GET /api/whoami` reports `identity_source` and `may_approve` so the console
can say this before an approver tries. If you add an identity source, place it
on this ladder deliberately — the question is not "is it authenticated" but
"is the proof strong enough to be an audit record".

**Removed, do not re-add:** `into_hub` / `areev hub` / `/api/segment*` were
deleted on 2026-08-24. A networked write surface that takes one shared secret
over an entire memory cannot participate in the authorization model — see
ARCHITECTURE.md §10, "Sync is file-to-file; Areev runs no networked sync
service". Replication is `areev stream`/`follow` over a directory.

## Security invariants — do not regress

- **Body cap 1 MiB** — reject larger bodies before buffering.
- **Origin check** — cross-origin **POSTs** are rejected (drive-by protection);
  a browser on another site must not be able to mutate a loopback console.
- **Read-only `GET /api/config`** reports effective config + file-vs-host
  reconciliation warnings — keep it read-only.
- One request per connection; parse defensively (this is an untrusted-input
  surface — see the fuzz/robustness posture in [[areev-invariants]]).
- Any change to auth, bind address, or the request parser → **re-read
  `docs/security-model.md`** and keep it accurate.

## Adding an endpoint

1. Route it in `handle_request` (`lib.rs:175`) — match method + path.
2. Enforce the mode's auth **first** (a new POST needs the token when one is
   set; a mutation that carries an identity needs the credential map, not the
   shared token).
3. Apply the body cap + Origin check for any POST.
4. Return proper status + headers (401 carries `WWW-Authenticate: Basic`).
5. If the endpoint reflects store state as JSON, follow the store contract; a
   new *operation* (not just a read) fans out via [[areev-add-operation]].

## The console (console.html)

One embedded HTML file, **vanilla JS**, no build step, no external assets
(dependency-light). Pages behind hash routes — `#memory` (sentence list),
`#graph` (canvas force-graph, focus + depth + rewind scrubber), `#activity`,
`#query`, `#workflows[/<plan>]`, `#runs`, `#tools/{catalog,executions}`,
`#suggestions`, `#settings/{agent,sync,general}` — plus a slide-in memory panel.
Light + dark, token for token.

`render()` un-hides exactly one `<section class="page" id="page-X">`, and the
list of names it iterates is the ONLY thing that makes a page visible. A page
missing from that array renders its content into a pane that stays `hidden` —
which is a blank tab, not an error. `every_console_page_is_in_the_visibility_list`
(in `lib.rs`) pins section ids, nav `data-page` values and that array together;
keep them in step.

Triggers deliberately have no page: they render on the workflow canvas in their
own lane (`trgNodes`/`trgEdges`, kept OUT of `WF_DRAFT` so a save cannot
serialize them) and are read-only because CAL has no `ADD trigger` at all. The
canvas run overlay and the Runs page share one index (`loadRuns()` → `RUNIDX`);
do not add a second fetch of `/api/run/*` for a new surface. **Design source of truth is the Paper file
"Areev", page "Console v2 — Redesign"** — reproduce visual changes there (or
read exact values from it via the Paper tools) rather than eyeballing; keep the
embedded file and the Paper design in sync.

Two rules the UI depends on:

- **Speak in sentences, not triples.** A grain renders as
  "Prefers green tea", never `fact | john | prefers | green tea`. Content
  addresses, grain types and CAL live behind **Developer mode** (`?dev=1` or
  the rail toggle, persisted in `localStorage`) — never in the default view.
- **`/api/browse` does not resolve heads.** A plain re-`ADD` of the same
  `(namespace, subject, relation)` replaces the old value *without* logging a
  SUPERSEDE, so the feed still marks the stale row as live. `markHeads()`
  recomputes this client-side (newest `op_seq` per key wins); the graph does
  the same *as of the rewind cut*. Delete that and the console starts showing
  values the store has already replaced.

Testing it: hash routes + `?dev=1` + `?theme=dark` make every page reachable
headlessly — `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
--headless=new --virtual-time-budget=5000 --dump-dom "http://127.0.0.1:PORT/#graph"`
renders the real JS and is enough to catch a blank list or a thrown boot.

## The gate — before you commit

```bash
cargo test -p areev-server
```
- `tests/http_smoke.rs` — the request/response surface + auth behavior.

Then run the [[areev-invariants]] gate. If you touched the request parser or
auth, treat it as a security-sensitive change and review accordingly.
