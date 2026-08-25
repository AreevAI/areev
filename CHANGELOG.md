# Changelog

All notable changes to Areev are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.6.3] — 2026-08-25

### Added

- **Native TLS for the Postgres backend** (`postgres-tls` cargo feature) —
  the DSN's `sslmode` (libpq's full five-rung ladder, including `verify-ca`
  and `verify-full`, which the driver does not understand on its own) and
  `sslrootcert` are honored, so a managed Postgres that requires encryption
  on the wire — Azure Flexible Server's `require_secure_transport`, RDS's
  `rds.force_ssl`, Cloud SQL — connects without inserting a TLS-wrapping
  proxy inside the trust boundary. rustls with compiled-in webpki roots, no
  OpenSSL. On in the container image and in both bindings; off in the stock
  `areev` binary, where an encrypting DSN is now **refused by name
  (`STO-E003`) rather than downgraded to plaintext**. `sslmode=disable` and
  the `prefer` default are unchanged, so no existing deployment moves.
  Note that `require` follows libpq and encrypts *without* validating the
  certificate — use `verify-full`, with `sslrootcert` where the provider
  signs with a private root ([#117](https://github.com/AreevAI/areev/issues/117)).

### Fixed

- Postgres connect-time failures now carry their cause. These never reach a
  server, so they have no SQLSTATE, and `pg_err` reported only the driver's
  `Display` — "error connecting to server". A TLS rejection lands in exactly
  that class, where "invalid peer certificate: UnknownIssuer" is the whole
  diagnosis.
- **`outlook_graph.py` (invoice-to-accounting example): two live-only bugs.**
  The attachment listing's `$select` named `contentBytes`, which is declared
  on `microsoft.graph.fileAttachment` and not on the base `attachment` type
  the collection is typed as — Graph answered `400 BadRequest` and every
  message with an attachment failed the poll. And the poll read `/messages`,
  which spans **all** folders, so the desk's own approval mail came back out
  of Sent Items as a fresh candidate invoice on the next tick (with a new
  message id, so `/message_id` dedup could not catch it); it now reads
  `…/mailFolders/inbox/messages`. Neither reproduces under the keyless CI
  floor, which uses the fixture connector by design
  ([#118](https://github.com/AreevAI/areev/issues/118)).

## [1.6.2] — 2026-08-24

### Removed

- **`areev hub` (the "areevd" sync daemon) and the `/api/segment*` endpoints.**
  Areev no longer runs a networked sync service. Replication is what it already
  was underneath — `areev stream` writes generations of `.mgb` segments into a
  directory, `areev follow` applies them — and moving that directory is now
  always the deployment's job (rsync, object storage, a shared volume). Gone with it: `UiServer::into_hub`, `POST /api/segment`,
  `GET /api/segment`, `GET /api/segments`, the hub `--retain` archive sweeper,
  and the console's "Sync across apps" settings tab (`#settings/sync` now lands
  on the Agent tab).

  The forcing argument is that a hub token is one shared secret over an entire
  memory — anyone holding it could pull every segment, which is the whole file
  — and a bundle push is an op-log replay that never crosses the facade's verb
  checks, so the per-principal credential map governing every other write path
  had no purchase on it. A surface that can only be all-or-nothing cannot
  participate in the authorization model the rest of the system is built on.
  Rationale in full: ARCHITECTURE.md §10, "Sync is file-to-file; Areev runs no
  networked sync service".

  **Migrating:** a fleet that pushed segments to a hub replaces the HTTP hop
  with a directory the peers share (`areev stream --to DIR --retain 30d` on the
  writer, `areev follow --from DIR` on each reader). A deployment that needed
  concurrent writers against one memory belongs on the **Postgres backend**
  (`feature = "postgres"`, one memory = one schema), which is the supported
  answer and always was.

### Added

- **Brokered credentials can be minted per call instead of read once** (#113).
  `--credential` accepted only an environment variable, which made every
  brokered secret static for the life of the process — the wrong shape for the
  credentials capability tools actually use. A Google access token expires
  roughly hourly, so an unattended heartbeat needed a refresh step outside
  Areev that it could silently get wrong, and a run parked on a human gate for
  a day resumed with yesterday's token. Vault and secret-manager users had it
  sharper: their whole model is short TTLs and central revocation, and an
  environment variable defeats both.

  Two more sources resolve **inside the broker, at call time**:

  ```bash
  --credential 'sheets=cmd:gcloud auth print-access-token'
  --credential 'sheets=vault:secret/data/google#access_token' --resolver-env VAULT_ADDR,VAULT_TOKEN
  ```

  `cmd:` takes the command's trimmed stdout through the same subprocess seam
  `--tool-cmd` and `--embed-cmd` use, so it covers `vault`, `gcloud`, `aws` and
  `az` with no vendor client in the dependency graph; `vault:` reads a
  Vault/OpenBao KV secret natively (v1 and v2) so a container needs no `vault`
  binary. What the guest sees is unchanged — it names a label and holds
  nothing.

  Values are cached for `--credential-ttl` (default 300s) and minted again
  after, so a revocation upstream takes effect without a restart. A resolver
  that errors, times out, or returns nothing **refuses the call** rather than
  sending it unauthenticated, and the error names which credential failed
  without ever repeating what the resolver printed. A minted value is
  validated as an HTTP header value, because one containing CR/LF would forge
  a second header on every request it rides. A 401 on a minted credential
  always invalidates the cached value and re-issues the request exactly once —
  but only for `GET`/`HEAD`: a write that 401'd may already have been applied
  upstream, and the broker does not get to guess.

  `--resolver-env VAR,…` names the variables a resolver needs for its **own**
  authentication. They are registered as secrets (withheld from every
  subprocess seam) and re-admitted only for resolver spawns, which run under
  `EnvPolicy::ClearExcept`. This is load-bearing rather than tidy: a
  `VAULT_TOKEN` left ambient is readable by every `--tool-cmd` child and can
  fetch *every* secret, not just the one it was for — #100's leak, one level
  up. Bind a principal on the name side for these sources (`--credential
  'sheets@user:alice=cmd:…'`), because a command may itself contain `@`.
  `Credential`'s `Debug` is now redacted. Available on `areev run`, `areev
  trigger run`, and both bindings' `credentials_json`.
  Setup per platform: `docs/cookbook.md` §19. Rationale: ARCHITECTURE.md §10,
  "A brokered credential's source is a seam, resolved in the broker".

### Security

- **A brokered credential is now bound to a host, not only to a caller**
  (#112). `capabilities.http` carried `hosts` and `credentials` as independent
  lists and the permit check tested them independently, so **any declared
  credential could be attached to any declared host** — and a second `http`
  entry was refused outright, so a tool talking to two services had no way to
  say which secret belonged to which. A tool that reads a mailbox and writes a
  sheet could therefore send the mailbox token to the sheets API: the
  confused-deputy case the broker exists to prevent, reachable by an ordinary
  bug (one wrong label in the guest) as easily as by malice.

  Both halves of `declared ∩ host-granted` can now express the pairing, and
  both are checked. `capabilities` accepts **repeated `http` blocks**, and a
  call must be admitted by ONE block as a whole tuple `(host, path, method,
  credential, headers)`. The host-side grant gained the same:
  `--tool-egress 'sync:gmail@gmail.googleapis.com:POST'` pairs a credential
  with the bare hostname it may reach (`*.example.com` works; scheme and port
  stay with `--allow-host`, because the spec is colon-delimited and a URL
  would tear apart in it). A refusal that names both halves reads apart from
  an undeclared credential — different bugs, different fixes.

  **Compatible in both directions.** A single-block declaration behaves exactly
  as before, an unpaired grant still means any host the rest of the chain
  permits, and N blocks admit the union of N tuples rather than the
  cross-product their merger produced — so the change can only narrow.
  `CapabilityDenied` gains a `CredentialHost` variant and `CallerGrant`'s
  `credentials` field is now private behind `credential()` /
  `credential_for()`; `Broker::start` takes `CredentialSource` values
  (`Credential` converts with `.into()`).
  Rationale: ARCHITECTURE.md §10, "A brokered credential is bound to a host,
  not only to a caller".

  **Scope:** this covers the `areev run` path — brokered tools and capability
  tools. A *trigger connector* still holds every credential the trigger
  configured for any host in its `allowed_outbound_hosts`: one connector runs
  per evaluation pass, so its grant is derived from the credential list rather
  than written, and it carries no declaration. Unchanged from previous
  releases, now noted in `docs/triggers.md`; give a trigger only the
  credentials its connector needs.

## [1.6.1] — 2026-08-23

### Added

- **Capability tools can set non-credential request headers** (#105). A
  brokered `areev::fetch` request takes an optional `headers` map, and a Tool
  grain declares which names it may use as `capabilities.http.headers`. This
  was the last thing between capability tools and the APIs they are pitched at:
  every Google API called with user credentials requires `X-Goog-User-Project`
  or answers `403 … requires a quota project`, and that header is not a
  credential, so neither the broker nor the guest could set it. The same gap
  blocked `anthropic-version`, `x-ms-version`, and every tenant header.

  The credential channel stays closed. `Authorization`, `Proxy-Authorization`,
  `Cookie`, `Host` — and any header a configured `Credential::Header` rides in,
  which is known only after resolution — are refused at any casing: declaring
  one is refused at **write** time, sending one at call time. That refusal is
  deliberately free rather than costing a call from the budget: the
  spend-before-checking rule exists so a module cannot probe *per-caller*
  policy for nothing, and "may I write the Authorization header?" has one
  answer for everyone. Malformed names and values carrying CR/LF are a `400`,
  because header injection is malformed rather than merely denied, and it must
  die at the parse instead of at the socket where it would split one request
  into two.

  Declared headers are deny-by-default like credentials, matched
  case-insensitively, checked on every redirect hop, and travel exactly as far
  as the credential does — a cross-origin hop drops both, since a quota project
  or tenant id was meant for the host the caller named and not for one an
  intermediary chose. They are journaled on the `egress_call` Observation
  **with their values**, the deliberate asymmetry against the credential's
  name-only record: the caller supplied them, so recording them discloses
  nothing it did not already hold, and turns "it was allowed to reach Google"
  into "it billed this quota project on these four requests".

  The sandbox needed no change — it forwards the guest's JSON verbatim, so the
  guest ABI is the broker ABI.

- **Capability tools can read CAS blobs** (#106). A Tool grain declares
  `{"blob": {"read": true}}` and its module gains `areev::blob_get`, reading
  one stored blob by content address. This closes the gap that left a whole
  class of tool stuck outside Tier C: a trigger's connector already files email
  attachments as CAS blobs, and the tool that parses them is the one that most
  wants sandboxing — its input is untrusted by construction — yet it was the
  one tool that structurally could not be, because `wasm32-areev-io` could
  reach the network but not the bytes the memory already held.

  Read-only, and by address only: no enumeration, no write, no namespace
  access, so a module fetches bytes it was handed a `cas://` reference to and
  cannot browse the memory. The two capabilities are independent — a module
  that parses attachments and calls nothing declares only `blob` — and gated
  asymmetrically on purpose: `--allow-fetch` derives from the pinned runtime,
  because a host-side grant narrows it afterwards, while `--allow-blob` derives
  from the pinned *declaration*, because nothing narrows a blob read after the
  fact.

  **The read goes through the broker, not the sandbox**, and that is the
  design's substance rather than a detail. Reading the `.blobs` sidecar
  directly from the subprocess looks free — the read is lock-free, which is
  what already lets `areev blob get` work mid-run — but the subprocess is
  handed no memory path, cannot take `areev-store` without giving up the
  five-dependency standalone posture that makes it a credible boundary, reads
  nothing on the Postgres backend, and has **no channel back to the driver**:
  stdout is the guest's result and the stderr fuel line is prose. A read
  performed there could not be journaled, putting the hole in the evidence
  exactly where the untrusted bytes are. Through the broker, every read lands
  as a `blob_read` Observation naming the address and byte count, drained on
  the same superstep boundary as `egress_call` — and the guarantee becomes one
  sentence: **the guest gets neither a socket nor a file descriptor.**

  Success answers raw bytes rather than JSON, since a blob is binary and
  base64 would tax every guest with a decoder to read its own attachment;
  errors stay JSON and are told apart by status, never by sniffing a payload
  that may legitimately begin with `{`. Ceiling is `runtime_limits.max_blob_bytes`
  (default 8 MiB, the payload cap). Embedded backend only — on PostgreSQL a
  blob lives in-schema, so the call returns a `501` naming the limitation
  rather than reporting the attachment as missing.

### Changed

- **The framework adapters moved out of this repo.** `areev-langgraph` and
  `areev-crewai` now live in `AreevAI/areev-adapters` and are developed
  against the published PyPI `areev`, so they version against their upstream
  frameworks instead of against the core (`ARCHITECTURE.md` §10, "Framework
  adapters live outside the repo"). Nothing changes for anyone installing
  them: both stay on PyPI at 1.0.0 and work against current Areev. They are
  **parked** — no new releases planned until someone asks — so this repo's
  CI no longer runs their suites, and until the adapters repo is un-parked
  an areev release is not gated on them.
- The Hermes provider smoke, which rode the removed `adapters` CI job for
  its maturin-built venv, now runs as the last step of the `python` job (and
  so on macOS as well as Linux).
- **The console's memory graph draws entities, not values.** Every fact's
  right-hand side used to become a node, so `amount: 4400.00`,
  `currency: USD` and `payment_terms: net_45` were drawn as peers of the
  people and vendors they describe — the demo memory rendered as 91 nodes and
  126 edges for the ~35 entities it actually holds, which is a hairball
  rather than a graph. An object is now kept only when the memory also knows
  something *about* it (it is a subject somewhere too) or it arrived through a
  relation that points at an entity (`vendor`, `owner`, `reports_to`,
  `headquartered_in`, …), with a literal-shaped veto so an unfamiliar schema
  cannot smuggle a scalar back in. The same file now draws as 35 nodes and 36
  edges with no orphans. A memory whose relations are not recognised could be
  filtered down to nothing, so a graph left with fewer than three linked
  entities falls back to unfiltered rather than showing an empty canvas.
- **The graph is legible and reproducible.** Labels are placed by priority —
  the focused node, then the relations coming off it, then names, near before
  far — each trying several positions before it is dropped, with the node
  circles treated as obstacles, so names no longer stack on each other or
  print across a node. Start positions are seeded from the node name instead
  of `Math.random()`, so a file lays out the same way on every reload and a
  re-shot screenshot is byte-identical. A rebuild that places nothing new
  keeps the positions it has, which stops the rewind scrubber re-converging
  the whole layout under the cursor on every drag.
- The graph legend said "Things they like", which read as a personal-assistant
  memory and misnamed every invoice and process in a business one; it is now
  "Everything else", alongside "Projects & processes".
- The console rail carries the full v2 lockup (the A, `reev`, and the
  improvement loop) rather than the A beside a text "Areev" — two bitmaps,
  because `BRAND.md` wants the white `reev` as its own artwork rather than a
  CSS recolor. All ten README screenshots were re-shot against the real
  console, as a console change requires.

## [1.6.0] — 2026-08-23

### Security

- **The outbound allowlist now governs every redirect hop, not just the first**
  (#99). The broker's HTTP agent followed up to ten redirects on its own, while
  `policy.permits` was checked exactly once — on the caller-supplied URL,
  before dispatch. So an allowed host answering `302 Location:
  http://169.254.169.254/latest/meta-data/` had its follow-up performed and the
  cloud metadata service's body handed back to the tool: host allowlisting is
  this subsystem's core control, and a redirect walked straight through it. It
  affected every brokered tool and connector, not a hypothetical. Auto-follow
  is now off (`max_redirects(0)`) and the broker follows by hand, re-checking
  the allowlist on every hop and re-checking the grant whenever a `303`
  changes the method. The invariant is now enforced rather than intended: **no
  byte is sent to, and no body is returned from, a host the allowlist does not
  permit.** A blocked redirect journals a refusal (`RUN-E022` / `TRG-E009`)
  worded apart from an aimed-at one — "it tried to reach there" and "it was
  redirected there" are different stories for whoever reads the record — and
  chains are bounded at ten hops with the bound itself auditable.

  The mirror image is fixed in the same change. ureq's `redirect_auth_headers`
  defaults to `Never`, so the brokered `Authorization` was dropped on *every*
  redirect, including the same-origin ones Google and Microsoft APIs use
  routinely; the follow-up arrived unauthenticated, 401'd, and nothing in the
  journal said why. The credential now re-attaches exactly when scheme, host
  and port are unchanged, and is dropped otherwise. A `Location` that is
  relative resolves against its base; one that is not a resolvable `http(s)`
  URL, or that carries a control character, is refused rather than guessed at.

- **`--credential NAME=ENV_VAR` no longer leaks the raw secret into every child
  process** (#100). The withhold list was three flags long
  (`--passphrase-env`, `--token-env`, `--anon-key-env`) and `--credential` was
  not on it, so the credential value stayed in the inherited environment of
  every tool, connector and sandbox subprocess — readable from
  `/proc/self/environ`, a core dump, or an `env`-printing bug. A tool never
  needed to call the broker at all, which is the exact opposite of what
  brokering is for. Reading a credential is now what registers its variable as
  a secret: `Credential::bearer_from_env` calls `deny_env_var`. Placing it
  there rather than at a flag-parsing site is the point — **four** hosts read
  credentials this way (`areev run`, `areev trigger run`, and the Python and
  Node bindings), so a fix at any one of them would have left the other three
  open. Children still receive `AREEV_EGRESS_URL` + `AREEV_EGRESS_TOKEN`,
  which are applied after the environment policy.

  The existing test did not catch this because it removed the variable from
  the *parent* before spawning, validating the broker's request path rather
  than the deployment where an operator exports a token and leaves it
  exported. The regression test keeps it exported.

- **The sandbox seam spawns under `EnvPolicy::ClearExcept`.** A wasm host has
  no claim on the operator's whole environment, and it is now also the process
  holding a broker token. Native code blobs keep `InheritExcept` — they are
  ordinary programs and may legitimately read an ambient variable.

- **A credential reflected in a response body is scrubbed.** Response headers
  never cross the broker, so the body was the only channel by which an echo or
  verbose-error endpoint could bounce the injected `Authorization` back to the
  caller and into the audit trail.

- **The private-space deny recognized only canonical IP literals.** Under an
  unrestricted egress policy, `is_private_destination` is the sole control
  stopping a synced capability tool from reaching loopback, link-local, or
  metadata address space — but it parsed the host with `Ipv4Addr::from_str`,
  which accepts only dotted-quad. A libc resolver (and therefore ureq) still
  maps the historical `inet_aton` forms to the same address, so a Tool grain
  declaring `hosts: ["http://2852039166"]` — decimal for `169.254.169.254`,
  the cloud metadata service — sailed straight through it: the exact case the
  check exists to close. It now canonicalizes decimal, hex, octal, and short
  (`127.1`) forms, and covers the RFC 6598 shared/CGNAT range
  (`100.64.0.0/10`), which `Ipv4Addr::is_private` does not.

- **A credential could return after the redirect chain left its origin.** The
  same-origin check compared each hop against the URL the caller *started*
  at, so a chain `A(cred) → 302 B (cross-origin, cred dropped) → 302 back to
  A/<path B chose>` re-attached the credential on the final hop — more
  permissive than browsers or `curl --location`, which drop it for good once
  the chain leaves the origin. A hop to a different origin now retires the
  credential for the rest of the chain, and the success audit records the
  credential name only when it actually rode the final request — not
  whichever name the caller asked for.

- **A shared broker re-journaled an earlier run's egress calls as its own.**
  `areev trigger run` reuses one broker across the runs it fires in sequence,
  and `Broker::calls()` accumulates for the broker's whole life without
  draining — so a run's journaling cursor starting at 0 re-wrote a prior
  run's already-journaled calls into the immutable store a second time, under
  the new run's id, principal, and clock. The cursor now seeds from what the
  broker already holds at drive entry.

- **A handful of fail-open edges closed during review, before anything
  shipped**: `--credential NAME=VAR@` with an empty principal (a typo, or an
  unset shell variable in the owner position) now refuses rather than
  silently binding an unbound credential; a poisoned mutex on the
  per-principal owner map now fails closed instead of skipping the owner
  check; a `wasm32-areev-io` tool with no `capabilities` is refused at write
  time, matching the check the manifest already made at run start; and
  `max_response_bytes` clamps rather than truncates on a 32-bit target.

### Added

- **Capability tools: an I/O tool can be a grain** (#101). Tier C was correct
  for pure compute and, for two releases, that made it half a promise — it is
  the **only** tier producing a persistable, content-addressed tool, and it
  forbade all I/O, so the tools every real agent needs (poll a mailbox, append
  a sheet, call a model) could not be grains. The options for an I/O tool were
  a native blob (persisted, but *not sandboxed — it runs as you*, and
  platform-specific) or a host `--tool-cmd` script (sandboxed by nothing, and
  outside the memory entirely).

  The tier now has two runtimes, because there are two determinism stories:

  | Runtime | Import set | Determinism |
  |---|---|---|
  | `wasm32-areev` | `areev::emit` | pure — re-execution-provable (unchanged) |
  | `wasm32-areev-io` | `+ areev::fetch` | deterministic *modulo journaled effects* |

  **The guest still never gets a socket.** It gets one unforgeable capability
  to *ask the host*; the sandbox binary's trusted Rust half forwards over
  loopback to the credential broker, holding a revocable broker token and
  never a credential. This needed no new IPC — the engine already injected the
  broker's address and token into that process for uniformity, inert only
  because the *guest* could not reach them. The isolation claim is
  strengthened, not weakened: no socket, no credential, no clock, no
  environment, and the host enforces policy and records everything.

  A new `capabilities` field on the Tool grain declares what a module may
  reach — hosts, methods, path prefixes, credential names. It **declares; it
  never grants**: the effective set is `declared ∩ host-granted`, checked on
  every call, so a declaration can only narrow what `--allow-host` /
  `--credential` / `--tool-egress` already permitted. That is the same split
  `--allow-executor` makes for the code itself — the declaration replicates
  with the bundle, the authority does not. What it buys is audit (a synced
  memory says what a tool may reach without reading anyone's command line) and
  a **tighter** bound than the host grant can express: `--allow-host`
  allowlists hosts only, while a capability may pin `path_prefixes`, closing
  the exfiltration case a host-only grant structurally cannot — a malicious
  tool POSTing stolen context to an *allowed* host's upload endpoint.

  Deny by default throughout, and enforced at five heights: CAL refuses a
  malformed declaration at **write** time; the manifest refuses a bad
  runtime/declaration pairing at **start** and freezes the declaration beside
  the pinned runtime, so a mid-run supersession cannot widen reach; dispatch
  refuses a capability module whose host wired no broker, naming the missing
  flag; the sandbox refuses a module importing `areev::fetch` without
  `--allow-fetch` at **instantiation**, by name, before one instruction runs;
  and the broker checks declaration, grant, allowlist, method, call budget and
  response ceiling on **every call — and every redirect hop**, so a `302` on a
  declared host cannot walk a module off its declared paths. The
  `path_prefixes` match refuses evasive shapes (`..` segments, `%2e`/`%2f`/
  `%5c`, backslashes) outright rather than normalizing them, the response
  ceiling bounds what the broker *reads* rather than measuring after
  buffering, and `areev::fetch` is non-reentrant by mechanism — a guest whose
  `alloc` calls `fetch` again gets `-1`, not a recursion. Two further gates make the runtime safe for
  a process serving more than one user: a capability declaration cannot reach
  loopback/private/metadata address space by itself (that takes an explicit
  `--allow-host` entry, and the rule binds every redirect hop), and
  `--credential name=VAR@principal` binds a credential to its owning run
  principal so a run executing as anyone else — or as none — is refused it. The
  driver binds the run principal automatically. The host-prefix grammar has one parser,
  in `areev-core` beside the grain field, shared by the write path and the
  broker — two would be how a tool becomes writable and then unrunnable.

  Ceilings are `runtime_limits` keys (`max_calls`, `max_response_bytes`, next
  to `fuel` and `max_pages`), and an overrun is a typed error, never a
  truncation.

- **Successful brokered calls are journaled, not only refusals.** A new
  `egress_call` Observation in `agent:harness` records caller, method, final
  URL, status, redirect count, request and response **digests**, response size
  and the credential **name**. "It was allowed to reach Gmail" is a policy
  statement; "it sent these four requests" is the evidence, and only the first
  was in the memory before. Bodies are digests because a grain is immutable
  and replicates; the credential is a name because that is all the broker ever
  received. Refusals dedup on `(caller, destination, reason)` and calls do
  not — a refusal is a policy fact and forty retries are one of them, but a
  call is an effect and forty are forty. Neither is a journal entry, so
  `verify` stays byte-identical whether or not a broker was configured.

### Changed

- **A non-2xx from an upstream reaches the caller as a status, not a broker
  error.** `http_status_as_error` had to be turned off for the broker to read
  a redirect's `Location` at all, and it fixes a smaller wrong on the way: a
  404 or a 429 used to arrive as `502 {"error": "upstream: …"}`,
  indistinguishable from the connection having failed. The broker's contract
  is to answer with the response, so it now does.

- **The Node binding publishes as `@areev/areev` again, not `areev`.** npm's
  similarity filter still 403s the unscoped name against `argv` — the
  1.5.2 release attempted it twice (once before, once after an unrelated SBOM
  fix) and both times the four platform packages published while the main
  package failed, leaving a broken partial release on the registry. A
  support ticket for the unscoped name is open; until it resolves, `npm
  install @areev/areev` is correct and `npm install areev` is not. crates.io
  and PyPI are unaffected.

### Not in this release

Deliberately out of #101's first phase: verify-by-re-execution against the
recorded call log, connectors resolved as capability tools by content address,
concurrency, streaming, raw sockets, and guest-visible clock or RNG — the last
of those permanently, because it is the determinism boundary.

## [1.5.2] — 2026-08-22

### Fixed

- **A trigger builds the same runner `run start` builds** (#90). The trigger
  path constructed a deliberately reduced runtime — a bare `CommandExecutor`
  with no model — so a plan with a **code-carrying (Tier C) node refused at
  start with `RUN-E018`** and one with an **abstract node with `RUN-E006`**,
  no matter which flags the operator passed; the same plan ran happily from
  `run start`. `--context-query` (#92) and `runtime` (#86) shipped in the same
  release and were meant to compose; used together the run refused, so an
  agent could have declared context **or** sandboxed tools on the trigger
  path, not both. `trigger run` and `trigger deliver` now take
  `--allow-executor`, `--executor-cache`, `--sandbox-cmd`, `--model` /
  `--base-url` / `--key-env`, the egress trio, and the observers — from one
  shared builder rather than a second copy, so a stack that grows a component
  cannot grow it on only one path. Both bindings gain `allow_executor`,
  `executor_cache` and `sandbox_cmd` on `trigger_run`/`trigger_deliver`.
  Every setting also reads its `$AREEV_RUN_*` variable (flag wins), because a
  heartbeat is a cron line, not an interactive command. A firing now also
  starts runs with **no** `--tool-cmd` at all — a plan whose nodes are all
  pinned code, or all abstract, needs no subprocess, and gating on one was the
  same reduction one level down. The pin is still the authorization and still
  comes from the host; what changed is where the host may state it, not who
  may. `RUN-E018`/the runtime refusal now name `areev trigger` among the
  surfaces to pin on — the old message named three, none of them the one the
  operator was using.
- **A trigger-started run carries the budgets it was given.** `run start` has
  taken `--max-tokens`/`--max-usd`/`--max-wall-ms`/`--ask-ttl` on every
  surface since the runtime shipped; the trigger path took none of them and
  built `RunOptions::default()`, in the CLI and both bindings alike. Moving a
  workflow behind a trigger therefore dropped every ceiling silently — on the
  one path that fires unattended, where an unbounded run has nobody watching
  it and an ask with no TTL parks forever.
- **`areev trigger --credential NAME=VAR` refuses an unset variable** instead
  of dropping it. `run start` and both bindings already did; the trigger path
  dropped it silently, which does not stay silent — it surfaces downstream as
  an unexplained 401 from someone else's API, hours later, on a heartbeat
  nobody is watching.
- **A `sha256:`-prefixed workflow reference fires** (#73). Both spellings were
  accepted at declaration and only the bare one worked at evaluation, which
  hex-decoded the whole string: the trigger validated, listed, reported
  `waiting` forever, then died at fire time on `FMT-E001: invalid hex hash:
  Odd number of digits`. References are now read through a known scheme
  prefix (`sha256:`, `grain:sha256:`) everywhere and **normalized to the bare
  form on write**, so `trigger_list` returns what was declared and a
  round-trip comparison matches. A reference that is not an address at all is
  refused at declaration and reported as `unusable` by `trigger status`
  (`TRG-E002`) rather than sitting in `waiting`.
- **`name` is returned by every trigger read surface** (#73). It was accepted
  on write and read back by nothing, so identity fell onto the workflow hash
  — which is stable only until the plan is re-declared. `areev trigger
  list`/`status`/`show`, `trigger_list()` and `trigger_status()` now carry it,
  and `areev trigger add --name` sets it. A blank name is treated as absent.
- **`trigger add` says at declaration time what the plan will need at fire
  time** (#73). Pointing a trigger at a plan whose nodes do not resolve used
  to fail at the *first firing*, on the operator's mailbox rather than at
  their keyboard, with `trigger status` reporting `waiting` in between. It now
  warns when the workflow is not in the memory, and when nodes are abstract
  (naming them, and the model configuration they will need). A warning rather
  than a refusal: a plan can arrive by sync afterwards, a Definition can be
  added later, and abstract nodes are legitimate with a model configured.
- **The Python docs-example guard no longer asserts a block the README does
  not have.** The 1.5.1 revamp made the README visual-first and removed its
  Python proof block; the guard kept asserting it, so the `python` CI job went
  red on a docs change — the guard outliving the thing it guarded. It now
  covers the two docs that do carry a block.

### Added

- **SSO proxy secrets rotate without a zero-overlap cutover** (#79).
  `areev ui --sso-secret-env-next VAR` opens a window in which **either**
  secret proves the proxy, so a fleet moves over one node at a time and the
  old value is retired once nothing presents it — TLS key rotation's shape.
  The secret is impersonation-grade (it can assert any identity, including
  approval-capable principals), and rotating one atomically across a proxy
  fleet is not achievable in practice, so the honest choices were an outage or
  a gap — and an operator facing either under suspected-compromise pressure
  defers the rotation, which is the outcome that actually costs. Two secrets
  at a time, deliberately; both compared in constant time with no
  short-circuit, so timing cannot reveal which matched; rotating to the same
  value is refused; and the console warns on **every** start while the window
  is open, because a rotation left half-finished is an extra live credential.
  New runbook: [`docs/runbooks/sso-secret-rotation.md`](docs/runbooks/sso-secret-rotation.md),
  covering the planned rotation **and** the suspected-compromise case, where
  the answer is a hard cutover and *not* a window.
- **Release artifacts carry provenance and an SBOM** (#81). All three release
  workflows now attach a Sigstore-backed build provenance attestation
  (`actions/attest-build-provenance`, keyless — no key for this project to
  lose) and publish a CycloneDX SBOM alongside the artifact; npm packages also
  publish with `--provenance`, which is what `npm audit signatures` checks.
  The bindings get **two** SBOMs each, because a wheel or a `.node` addon
  links a Rust tree `pip`/`npm` cannot see, so a package-manager-only bill
  would truthfully describe almost nothing. Verification is one command
  (`gh attestation verify …`), documented in
  [`docs/security-model.md`](docs/security-model.md). This complements
  `cargo-deny` rather than replacing it: `cargo-deny` says the dependencies
  are acceptable, the SBOM says which ones shipped, the attestation says who
  built them.

### Documentation

- `docs/triggers.md` gains **the runner a firing gets** (the full flag /
  binding / environment table), **the plan has to resolve before the trigger
  fires**, **re-declaring a plan mints a NEW plan**, **naming the workflow**,
  and **what the run receives** — a trigger wraps the item as
  `{trigger, connector, scope, item}` while `run start` passes its input
  through unchanged, so one plan started both ways sees two shapes, and a tool
  reading a top-level key fails on the trigger path only while the pass still
  reports `runs_started: 1` with no errors.
- **Re-adding a Workflow is not free, and the docs now say so** (#73). Grains
  are content-addressed over the whole `.mg` blob and the header carries
  `created_at`, so two identical `add("workflow", …)` calls return different
  hashes: an idempotent-declare loop mints a new plan every boot while the
  trigger still points at the old one, silently. Excluding `created_at` from
  the address is not an option — canonical serialization is frozen, and
  moving it would change every content address ever computed and break OMS
  conformance. `docs/triggers.md` documents the recall-first declare pattern
  instead.
- `docs/run.md` records **why the embedded backend has no read-only open**
  (#85) — the exclusive lock lives inside a pinned `turso` whose facade
  exposes none, and today's open path writes regardless (DDL replay, the
  telemetry sidecar, heal passes, the anon-vault write-behind), so it is a
  store-level project gated on a re-audited engine bump, not a patch.
- `docs/security-model.md` documents trusted-header SSO and its rotation
  window under data-in-transit, and adds **release artifacts: provenance and
  bill of materials**.

## [1.5.1] — 2026-08-22

### Fixed

- **CAL `WHERE` fails closed** (#91). A filter is now pushed down, evaluated
  per grain, or refused — never dropped. Before, a common field outside a
  grain type's queryable set (`status`, `priority`, `epistemic_status`, …)
  passed validation, fell out of push-down, and returned **everything** with
  only a stderr `CAL-W010` — so `RECALL tools WHERE status = "failed"`
  returned the successes, in the right shape and order. Now a field the
  target type cannot carry refuses with `CAL-E060` before the scan; `NOT`
  and `OR` are honoured with real boolean semantics by the one authoritative
  per-grain evaluator (`NOT tool_name = "x"` returned precisely the set the
  author asked to exclude; `a OR b` pushed only `a`); previously-dropped
  comparators (`confidence < x`, `subject != y`, `IS NULL`) now filter; and
  engine-level fields (`query`, `time`, `entity`, `contradicted`, `scope`,
  `tags`) refuse with the new **`CAL-E061`** where they cannot be honoured
  (under `NOT`/`OR`, unsupported comparator) instead of widening. `EXISTS`
  (which answered `true` if *any* grain of the type existed), `HISTORY …
  WHERE`, and ASSEMBLE's post-filter share the contract. Leniency was
  deliberately not kept behind an opt-in: the safe direction is the default
  direction. `DESCRIBE FIELDS <type>` now lists exactly the registry's
  filterable set for that type.
- **`kind` and `status` are queryable on `tools`** (#91). Definitions and
  execution records are one grain type split by `kind`; without it every
  host invented a child-namespace workaround to keep definitions from
  outranking real results. Both fields are stored omit-default and the
  filter materializes the default (`kind = "execution"`, `status =
  "completed"` match grains that never wrote the field). The phantom
  `tool_phase` field — advertised, parsed, and never written — is removed
  from the queryable set.

### Added

- **`--context-query` can see the firing item** (#92). The declaration may
  bind saved-query parameters from the item's payload with the JSON
  pointers `--dedup-key` already understands: `--context-query
  'triage_ctx($session = /session)'`. The evaluator resolves each pointer
  at fire time and runs the query with those bindings via the parsed-AST
  `RUN` path (no CAL text splicing, so payload values cannot inject CAL).
  Fail-closed with `--dedup-key`'s precedent: an unresolvable pointer or a
  non-scalar value refuses the firing. The whole spelling is stored
  verbatim on the trigger grain (same field, same compact key; the plain
  name form is byte-identical to 1.5.0), so the binding replicates and
  audits with the declaration. Malformed spellings refuse at `trigger add`.
- **Polling connectors can persist CAS blobs** (#93). An item may return a
  `blobs` array (`{filename, mime, b64}`); the **evaluator** — the party
  already holding the writer — stores each entry (`put_blob`, idempotent on
  content), rewrites `"blob": "@N"` payload references to the resulting
  `cas://sha256:…` address, and attaches matching `content_refs`
  (uri/mime_type/size_bytes/checksum, filename in metadata) to the Event it
  writes. Attachments ingested by trigger and by host are now
  indistinguishable: `blob get` works mid-run, dedup is content-addressed,
  and erasure's sole-reference reclamation needs no special case. Budgets
  are enforced on decoded size (16 MiB/item, 48 MiB/response, evaluator
  options) and any contract violation — over budget, undecodable base64, a
  dangling `"@N"` — is the new **`TRG-E011`**: the whole poll refuses with
  the cursor unmoved, because a silently dropped attachment is an invoice
  posting without evidence and a lost item is worse. The RFC 4648 base64
  decoder moved to `areev_core::b64` (one implementation, shared with the
  server's HTTP Basic path).

## [1.5.0] — 2026-08-22

### Added

- **Trigger-started runs carry declared context** (#85). A Trigger grain may
  name a saved query (`--context-query NAME`, field `context_query`, compact
  key `tcq`, omit-default — every existing trigger keeps its content
  address): at fire time the **evaluator** runs it read-only against the
  memory it already holds and places the result into the run input as
  `context`. This is the embedded backend's answer to its own exclusive
  lock — a tool inside a run cannot open the memory its run holds, but the
  evaluator can, and the declaration replicates with the trigger, so what a
  fired run sees is auditable rather than host-local. Fail closed: a trigger
  that declared context never fires without it. The backend divergence is
  now documented (`docs/run.md`): on the PostgreSQL tier reads never block,
  so tools *can* query the memory mid-run — the read-only embedded open
  (#85 proposal A) stays tracked, gated on a deliberate turso bump.
- **Tool Definitions declare their runtime, and the engine dispatches to the
  sandbox** (#86). `runtime: "wasm32-areev"` (+ optional `runtime_limits:
  {fuel, max_pages}`; both omit-default, compact keys `axr`/`axl`) routes a
  pinned `cas://` blob to **areev-sandbox** instead of native exec — the
  engine constructs the sandbox argv itself (`--module <cached blob>
  --fuel N --max-pages N`), so provenance (`--allow-executor`) and isolation
  become independent knobs. The runtime is frozen into the run manifest with
  the address; an unknown runtime refuses at resolve rather than running
  foreign bytes natively; a declared runtime on a host with no sandbox
  refuses at start, naming the missing config. The sandbox runner is host
  config on every surface: `--sandbox-cmd` (CLI), `sandbox_cmd`
  (Python/Node `run_start`/`run_resume`), `$AREEV_RUN_SANDBOX_CMD`
  (`areev serve`). Also: the sandbox's `ForbiddenImport` message and module
  docs no longer claim `areev::alloc` is importable (it is a guest
  **export**; the frozen import set is `areev::emit` alone — regression-
  pinned), and `areev-sandbox`'s version stamp is no longer stale.
- **The executor pin reaches every surface that starts runs** (#87).
  `allow_executor`/`executor_cache` on Python and Node
  `run_start`/`run_resume` (the CLI's comma list), and
  `$AREEV_RUN_ALLOW_EXECUTOR`/`$AREEV_RUN_EXECUTOR_CACHE` set at `areev
  serve` start for MCP — server-bound like `$AREEV_RUN_TOOL_CMD`, because
  the pin IS the authorization. Previously a plan naming a code-carrying
  Definition was unrunnable from every non-CLI surface (RUN-E018 with no
  recourse); the refusal now names the pin mechanism per surface. The
  console's HTTP surface deliberately does not start runs, so it carries no
  pin.

### Fixed

- **Code-carrying tools reach the credential broker** (#87).
  `CodeExecutor::execute_code` now injects
  `AREEV_EGRESS_URL`/`AREEV_EGRESS_TOKEN` on the same terms as
  `CommandExecutor` — granted tools only — and the CLI hands the broker to
  the code executor even without `--tool-cmd`. Previously the pinned blob,
  the authoring style whose provenance the host can actually prove, was the
  one that could NOT use brokered credentials.
- **Two doc corrections** (#87): `docs/run.md` no longer claims the run
  journal lives in `agent:harness` (intents/results/checkpoints live in the
  run's session `--ns`; the manifest and administrative records are the
  `agent:harness` residents — an operator following the old text could
  leave a journal outside every declared policy), and the 1.3.0 changelog
  bullet claiming webhook/manual/composite triggers "fire in a later
  release" is corrected in place — all eight kinds fire since 1.3.0.

- **The tuning seam** — the last mile of the corpus path, closing the slow
  learning loop under the same governance as the fast one. `areev tune --cmd
  'TRAINER'` hands a governed corpus to a **host-supplied** trainer (JSON on
  stdio, stderr inherited, no timeout by default — Areev still never trains
  and takes no training dependency) and registers the returned adapter as an
  `mg:adapter` Fact in `agent:harness`: base model + adapter + quantization
  pinned as one tuple, `derived_from` naming the corpus export manifest, the
  Rule E1 evalset pin embedded. Integrated (`--select … --out`) and
  bring-your-own (`--corpus … --manifest`) corpus modes; lineage cannot be
  asserted from the command line — the manifest must be a recorded export.
- **`adapter_revision` — a new eval-gated recommendation class** mirroring
  `code_revision`: the new builtin `adapter_intake` analyzer (14 builtins now)
  proposes the newest unpromoted candidate per served model; apply is refused
  without a recorded clean run of the pinned evalset and writes an immutable
  `(model:<name>, mg:adapter_promotion)` Fact — the host contract: serve what
  a live promotion names, stop when it is retracted (rollback's inverse).
  One candidate per served model by design; auto-apply is impossible three
  independent ways. When a baseline eval run exists the recommendation
  carries an `evalset:<pin>:failed` metric, so a post-promotion regression
  makes `outcome_review` propose the revert.
- **`areev eval run --model provider:name`** — grade an evalset against a
  model behind the ToolCallLlm seam instead of a host command: how a tuned
  adapter served by vLLM/SGLang (`openai-compat:<served-name>`) or Ollama is
  gated, with `--base-url`/`--key-env`/`--llm-max-tokens`, fail-closed case
  prevalidation, the same scorer as `--tool-cmd`, and the graded model
  recorded in the `mg:eval_run` summary. (`--base-url`/`--key-env` also
  joined `areev run start`'s USAGE, where they existed undocumented.)
- **Gated apply reaches every surface** — the loop's documented full-lifecycle
  parity now includes the gating edge. One shared loader
  (`Engine::gating_evidence`) serves the CLI's `--gating-run`, Python/Node
  `apply_recommendation(..., gating_run=…)` / `applyRecommendation(...,
  gatingRun)`, MCP `areev_recommendations` `gating_run`, and
  `POST /api/loop/apply` `gating_run` — on every surface the stats are read
  back from the journaled `mg:eval_run` Fact, never taken from the caller.
  The console's review queue asks for the gate run id on gated
  recommendations (their rows now carry `evalset_hash`). Fused
  approve-and-apply callers are refused **before** the approval lands when
  the gating run is missing or unknown (`preflight_apply` gained
  `has_gating`; `ensure_executable` now knows a gated revision's Data
  payload is executable — both latent classification gaps exposed by the
  first production producer of gated recommendations).
- **The record family grows two members in the bindings**:
  `record_corpus_export` / `recordCorpusExport` (the immutable export
  manifest, for hosts that select and serialize in-process — the CLI verb
  stays the paved road) and `record_adapter` / `recordAdapter` (the adapter
  registration `areev tune` performs, for hosts that train in-process).
  `record_adapter` now also verifies its lineage anchor **is** a corpus
  export manifest on every surface, not just the CLI.
- **Erasure reaches the seam**: the stale-export notice on
  `forget-subject`/`purge-older-than`/`retention sweep` (and the CAL erasure
  audit) now walks one provenance hop further and names the **adapters**
  derived from a stale corpus — `stale_adapters` beside `stale_corpora` in
  the Tier-2 audit record and `areev audit export`. Still auditable
  suppression and re-derivation, never an unlearning claim.

### Changed

- **The position on weight tuning is stated on the record, and the tuning seam
  is named as roadmap.** Areev's boundary is unchanged and now explicit as a
  named decision in `ARCHITECTURE.md` §10: it emits a governed corpus
  (`areev corpus`) and grades the result (`areev run shadow`, `areev eval`), and
  it never trains — no trainer, no training dependency. What is announced as
  *not yet built* is the seam itself (`areev tune --cmd`, an adapter registry
  grain, promotion as a gated apply); the design of record is
  `docs/areev-adaptive-agents-proposal.md` §5. The SEAL rows in
  `docs/loop-explainer.md` §14 and `docs/loop-reflection.md` are reframed from
  "avoid weight updates" to "order them last, behind a governed corpus and a
  replay harness" — a published competitive argument should not be reversed
  quietly. No accuracy or context-savings claim accompanies any of this until
  the replay harness has measured one.

## [1.4.0] — 2026-08-21

### Added

- **Line coverage is measured, published and gated per crate.**
  `scripts/coverage.py` turns the `coverage` job's LCOV trace into
  `docs/coverage.json`, which the README chart renders alongside the line
  counts. It scores source lines only — `tests/`, `benches/` and
  `#[cfg(test)]` blocks are excluded, because a test body is executed by
  definition — and excludes what that job structurally cannot run (`areev-py`,
  which pytest drives; the benchmark harnesses; the Postgres backend, which
  needs a live server), each exclusion carrying its reason in the JSON.
  Enforcement is **per crate plus a global floor**, not one workspace target:
  a single number lets a regression in one crate hide behind a gain in
  another, and these crates do not carry the same risk. The per-crate floors
  are the tight gate — regression ratchets a couple of points under each
  crate's measurement — with a looser aggregate floor under the whole set,
  deliberately given headroom because a gate that fails on cross-platform
  noise gets lowered, and a lowered floor protects nothing.
- **Tests for the CLI and MCP surfaces that had none** — the `areev trigger`
  read and lifecycle verbs (`show`, `status`, `pause`/`resume`, `render`,
  `deliver`), the `areev hold` and `areev retention floor` guards over
  age-based destruction, and the three MCP tools `mcp_smoke.rs` never called
  (`areev_supersede`, `areev_runs_touching`, `areev_recommendations`,
  including the recommendation lifecycle and every argument refusal). Plus a
  CAL error-contract test that pins the leading-token rule (every `Display`
  begins with its `DOMAIN-Ennn` code), keeps `DELETE`/`ERASE`/`TRUNCATE`/
  `DROP TABLE` rejected at the lexer as repros rather than as a claim, and
  checks that every one of the 78 emitted `CAL-Ennn` codes falls inside a
  range `ERROR_CODES.md` documents. Together these lifted `areev-cli`
  62.1% → 72.3%, `areev-mcp` 65.2% → 73.0%, and the workspace to 80.1%.
- **A real demo memory, committed to the repo** — `data/demo.db` (~800 KB,
  466 grains) holds one coherent story end to end: an accounts-payable
  agent's vendor knowledge and category rules, nine governed runs (six
  posted, one a person refused, one waiting on a person, one honest
  failure), a real open fork
  from two channels editing offline, a declared polling trigger, saved CAL
  queries, and thirteen recommendations that `areev loop run` actually
  computed from that history. Nothing in it is hand-written to look
  convincing; `scripts/build_demo.sh` regenerates the whole artifact from
  `crates/areev-store/examples/seed_accounting_demo.rs`, and
  `scripts/shoot_console.mjs` re-shoots the README's screenshots against it.
- **`examples/agents/invoice-to-accounting/` is runnable**, replacing the
  placeholder README. `./smoke.sh` imports the plan from a portable bundle,
  runs three fixtures through it — one auto-posted, one parked for a human,
  one photographed page that fails rather than posting a blank row — and
  asserts the outcomes, including that the principal who *started* a run is
  refused when it tries to approve it. Keyless: no credential, no network,
  no model key — and CI now runs it (`agent-example`), so the keyless floor
  is enforced rather than claimed.
- **`areev_search` and `areev_nearest` join the MCP tool surface (23 → 25
  tools), and `serve --mcp` gained `--profile memory|full`.** Both bindings
  and the CLI have had hybrid free-text recall (`search`) and the
  embedding-similarity novelty check (`nearest`) since early on, but MCP —
  the surface an LLM agent actually calls — only ever got structural
  `areev_recall`, which needs the caller to already know the exact
  `(subject, relation)` pair. The natural agent query ("what do we know
  about the Johnson account") is free text, and without `nearest` an agent
  had no cheap way to check "do I already know something like this" before
  `areev_add`, so long-lived sessions tended to accumulate near-duplicate
  facts reworded slightly across turns. Both fail loudly (never a silent
  empty list) when their prerequisite is missing — `areev_search` needs a
  text index or an embedder, `areev_nearest` needs an embedder — naming the
  MCP-specific remedy (`--index-text true` + `reindex`, or `--embed-cmd`),
  not a bindings-only one that doesn't apply to this surface. Separately,
  `--profile memory` narrows both `tools/list` and `tools/call` to the
  twelve read/write/query tools, dropping the thirteen-tool workflow-runtime
  family (`areev_run_*`, `areev_loop`, `areev_recommendations`,
  `areev_tool_provenance`, `areev_record_tool_call`, `areev_run_manifest`) —
  a host that only wants Areev as chat memory no longer hands its agent a
  dozen governed-run tools it will never call; `--profile full` (the
  default) is unaffected. See [`docs/mcp-reference.md`](docs/mcp-reference.md).

- **The console draws a workflow's whole picture on one canvas.** A `Trigger`
  grain names the plan it starts (`trigger.workflow`), and the binding points
  trigger → plan and never the reverse — a plan that grew a list of triggers
  would change content address every time one was added and orphan its own run
  history. That direction is precisely why a flat list is the wrong surface:
  it cannot show you that two triggers start the same plan. Triggers now render
  in a "STARTED BY" lane on the workflow canvas, dashed-bordered and
  dash-arrowed into the plan's entry steps, with the full declaration in the
  inspector — including a `memory` trigger's serialized `Condition` tree said
  out loud (`subject = "globex" AND relation = "open_incidents"`) rather than
  dumped as JSON. They are read-only, and not by preference: CAL has no
  `ADD trigger` and `ADD workflow`'s `ON "..."` clause was removed in 1.3, so a
  console that writes only through `/api/cal` has nothing to write; the panel
  offers the exact CLI command instead of an input it could not honour. Trigger
  nodes are held in their own arrays, never in `WF_DRAFT`, so the Save path is
  structurally incapable of serializing the lane into a plan grain.
- **A run overlay on the canvas.** Selecting a run tints each step by what it
  did in that run — a client-side join over the journal's own Tool grains via
  `mg:step_action:<node>`, with no new endpoint. The journal's Pending-then-
  supersede shape does the work: `isCurrent()` alone leaves exactly one row per
  `(run, node)`, which IS that step's current state. A step still Pending in an
  *open* run is waiting on a person; the same row in a canceled or failed run is
  simply where it stopped, and is drawn grey rather than orange so the UI never
  invites an approval that can never arrive.
- **A Tools page.** Tool definitions and executions are one grain type split by
  `kind`, so they are two tabs of one page: the catalog (each entry opening its
  full configuration — executor kind, input schema as a property table, locked
  params, annotations, and the plans that bind it) and every execution grain,
  grouped by run, with calls made outside any run given their own group rather
  than filtered out of existence. Built entirely on `/api/browse`.

### Changed

- **README is visual-first**: real console screenshots (light and dark, via
  `<picture>`) instead of design exports, an architecture diagram, a
  sixty-second runnable path, and the problem stated as a table before any
  of the mechanism. The stale `dejadb`-branded assets are gone.

- **Console navigation follows the order you meet things in**: Workflows →
  Runs → Tools. The standalone Triggers tab is gone (folded into the canvas
  above); `#triggers` redirects to Workflows rather than dead-ending a
  bookmark, plan cards carry what starts them and how they last ran, and a
  trigger whose plan is not in the current namespace gets an explicit callout
  under the list — a standing rule must never silently vanish from the console.
- **The Runs page groups by what it wants from you** — *Waiting on you* /
  *In flight* / *Finished* — instead of one flat grid that buried an ask under
  finished history. Each card resolves its plan's name, and carries the same
  per-step strip the canvas draws as a rail. The Approve/Refuse buttons now
  disable when the session cannot use them and say which credential is missing:
  `run.respond` refuses a shared console token even when that token can write
  everything else. The Runs page and the canvas overlay read ONE shared run
  index, so the two surfaces cannot drift apart.

### Removed

- **`README.zh-CN.md`.** A translation that lags the README is worse than no
  translation — it was still describing the pre-Console-v2 shape and pointing
  at screenshots that no longer exist.
- **`seed_support_demo.rs` / `seed_workflow_demo.rs`.** Both seeded a
  different fictional company than anything the README now shows.
  `seed_accounting_demo.rs` replaces them, and it is the single source for
  both `data/demo.db` and the example's `plan.mgb`.

### Fixed

- **`areev_recommendations` silently ignored `status: "all"`** over MCP,
  returning only the pending queue. `docs/mcp-reference.md` documents `all` as
  one of the four accepted filters, so an agent asking for every
  recommendation was told — with no error — that nothing had ever been
  approved or applied. The cause was a filter chain that could not tell "the
  caller said `all`" from "the caller said nothing", since both arrive as
  `None`; a dropped filter fails **open**, and the wrong answer goes straight
  into a model's context. Now pinned by `mcp_smoke.rs`, which asserts `all` is
  a superset of `pending`.
- **Run checkpoints read as "A state with no readable text" in the console's
  memory browser.** A checkpoint's body is the scheduler's serialized state,
  which has no sentence in it, so every one of them fell through to the
  type-name fallback — on any file with governed runs in it, the browser's
  default page was a wall of identical unreadable rows. They now say which
  run and which step they belong to. (What remains is a design question, not
  a bug: whether runtime bookkeeping belongs in the plain memory browser at
  all.)

- **The console's Triggers tab rendered into a pane that never became
  visible.** Every page section ships `hidden` in the markup and is revealed
  only by the one array in `render()` that clears the attribute; `'triggers'`
  was missing from it. The hash routed, the nav item highlighted and
  `renderTriggers()` filled its container on every render — while the section
  stayed hidden along with all eight others, so the tab showed an empty page.
  Nothing in the file could catch it, because the defect is a *missing* string
  rather than a wrong one: a test now parses `console.html` and asserts that
  the set of `id="page-X"` sections, the sidebar's `data-page` values and that
  array agree.

- **A refused egress-broker call could reset the caller's own connection
  instead of delivering its 401/403 JSON body.** `serve_one` read the
  request's token, decided to refuse it (unknown token, or a caller with no
  grant), wrote the response and dropped the connection — all without
  reading the request body the caller had already started sending. Closing a
  socket with unread data queued sends an RST rather than a clean FIN, so
  under enough scheduling delay the caller's own `write` could fail with a
  raw `ConnectionReset` and never see the refusal at all — a security-
  relevant "why was I denied" path degrading to an opaque I/O error under
  load. Found as a one-off `ConnectionReset` in the test suite during the
  1.3.1 release, confirmed as a real, reproducible defect (not test
  flakiness) by isolating it: 5/60 failures on the pre-fix code under
  verified CPU load, 0/60 after. The two refusal paths whose bodies are
  always small and legitimate (bad token, no grant) now drain the request
  body before responding; the "body too large" refusal deliberately does
  not, since draining an oversized claimed body is the resource-exhaustion
  risk that refusal exists to avoid. A regression test forces the same race
  deterministically, without needing artificial system load, by making the
  body large enough to force real TCP backpressure rather than fit entirely
  inside OS socket buffers — verified to fail on the very first run against
  the pre-fix code.

## [1.3.1] — 2026-08-20

### Added

- **Triggers reach the Python and Node bindings.** 1.3.0 shipped the trigger
  evaluator to the CLI only — `areev-trigger` was a dependency of `areev-cli`
  and nothing else, and there is no MCP tool either — so a binding host could
  *declare* a standing rule (the `Trigger` grain has always been authorable
  through `add("trigger", …)` and queryable through `RECALL triggers`) but had
  no way to **fire** one. It had to shell out to the `areev` binary: a second
  artifact to ship, pin and sign per deployment, for a rule the process was
  already holding the memory for. All nine subcommands are now methods —
  `trigger_add`/`list`/`show`/`status`/`run`/`deliver`/`pause`/`resume`/`render`
  (camelCase on Node) — returning the same `EvalReport`/`TriggerStatus` JSON the
  CLI prints under `--format json`. Two deliberate differences from the CLI:
  `trigger_add` also runs the schedule validation `add("trigger", …)`
  structurally cannot (cron parsing, the UTC-only refusal, a composite's gate
  against its own members — that check lives in `areev-trigger`, above the CAL
  grain builder), and an unset `--credential` variable is refused rather than
  silently dropped, because a host wiring this up programmatically has no
  console on which to notice, and the omission would otherwise surface as an
  unexplained 401 from someone else's API. Still no daemon: `trigger_run` is a
  call the host makes on its own heartbeat.
- **`anon_key` is reachable outside Rust.** The host-supplied anonymization
  root added in 1.3.0 (#46) was settable only through `AreevOptions` — not from
  the CLI, and not from either binding — so the feature whose whole purpose is
  making the mapping vault and value-derived tokens work on **Postgres** (which
  refuses `encryption_key`, a page-cipher capability) and on plaintext files
  was unreachable from the two surfaces those deployments actually use. Now
  `--anon-key-env VAR` on any CLI command and `anon_key=`/`anonKey` on both
  constructors, as 64 hex characters. The CLI takes the variable *name*, never
  the key, so it stays out of shell history and `ps`, and that variable joins
  `--passphrase-env`/`--token-env` in the deny-list every subprocess seam
  scrubs. A malformed key is refused at open rather than deriving a different
  token space — the failure mode that looks like working software right up
  until a rehydrate comes back empty.
- **Abstract nodes can run from a binding.** `run_start`/`run_resume` (and
  their camelCase twins) take `model`, `base_url`, `key_env` and
  `llm_max_tokens`. Both bindings hard-coded `llm: None` when building the
  `Runner`, so a plan with an abstract node refused at load with `RUN-E006` and
  there was no argument that could have prevented it — all of #45's provider
  and credential work (Vertex under workload identity, the feature-gated
  providers) was unreachable from the Python or Node agent service the bindings
  exist for. The spec is resolved *before* the run is journaled, so a bad
  provider or a missing key fails without leaving behind a run that can never
  advance. `trigger_run`/`trigger_deliver` take the same arguments, so a
  trigger may start a plan with abstract nodes.

### Fixed

- **A trigger that could never fire was stored, and then looked healthy**
  (#67). `areev trigger add` validated a declaration and refused a bad one, but
  `add("trigger", …)` — the path a host authoring programmatically actually
  reaches for — performed no equivalent check. The evaluator then counted the
  result under `not due`, which is indistinguishable from a healthy trigger
  waiting its turn, so the symptom was work silently not happening on whatever
  schedule was supposed to be running, with a green `trigger status`. Both
  binding write paths now run the schedule check (cron parse, the UTC-only
  refusal, a composite's gate against its own members). Because authoring-time
  validation cannot be the only defence — a declaration can arrive by bundle
  import from an implementation that validated differently, or predate the
  check — the evaluator also reports one rather than assuming it was caught: a
  new `unusable` counter on the run report, counted **apart from**
  `skipped_not_due`, an `unusable` reason on `trigger_status()`, and an
  `unusable` state in `areev trigger status` instead of `waiting`. Such a
  trigger is never reported as `due`.
- **A top-level `timezone` on a JSON trigger declaration was silently
  discarded** (found while reproducing #67). The evaluator reads
  `config["int:timezone"]`, which is where the CLI's `--timezone` writes, but a
  hand-written declaration naturally spells it `"timezone"` at top level — and
  that landed in `extra_fields`, where nothing reads it. The trigger was
  stored, reported healthy, and fired in UTC while its author believed it was
  on local time: silence, on a schedule. It now maps to the config key, and a
  declaration that sets both to *different* values is refused rather than
  resolved by a precedence rule nobody would remember.
- **`trigger render --target k8s-cronjob` emitted the authoring host's local
  binary path into a container spec** (#69). The manifest paired
  `image: areev:latest` with `command[0]` set to `std::env::current_exe()` —
  an absolute path from the machine that ran the render, guaranteed wrong
  inside the container, and sitting next to a right-looking `image:` line so it
  was not obvious which half the operator was meant to fix. Container targets
  now use the name on `PATH` in the image (`areev`); the host targets
  (`cron`, `launchd`, `systemd`) keep the absolute path, which is correct for
  them because they run on the machine that produced the render. The rendered
  `--db` path carries a comment saying it must resolve inside the container.
  The regression survived because the render test's context already used
  `exe: "areev"` — the same string the fix produces — so a render that spliced
  in `current_exe()` looked identical to one that did not; the new test uses a
  path that could only have come from the authoring machine.

## [1.3.0] — 2026-08-20

### Added

- **Repository quality metrics, generated and gated** — `scripts/repo_stats.py`
  measures the tree (source vs test lines, test count, error codes, per-crate
  breakdown) and emits five artifacts: a light and dark SVG for the README, a
  GitHub-renderable `docs/repo-stats.md`, a standalone `docs/repo-stats.html`
  report, and `docs/repo-stats.json`. Test code is counted **per block, not per
  file**, so a source file with a `#[cfg(test)]` module contributes its
  implementation to source and only the module body to tests — file-granularity
  counting inflates the ratio roughly 4x. A new `stats` CI job runs `--check`
  and fails the build when the published figures drift more than 2% from the
  tree, so the README's numbers cannot go quietly stale.
- **`scripts/check_versions.py`** — asserts that all five version sites agree
  (`[workspace.package]`, `areev-py/pyproject.toml`, `areev-js/package.json`,
  `areev-js/Cargo.toml`, and the ~54 literals baked into the generated
  `areev-js/index.js`), optionally pinned to the release tag. Run as a
  `versions` job on every CI run and as a `preflight` gate in the PyPI and npm
  release workflows. Both drift modes it catches have shipped before: a
  workspace-only bump makes the publish workflows skip-existing over the
  released version (a green run that ships nothing), and a `package.json` bump
  without regenerating `index.js` breaks `require()` for anyone with
  `NAPI_RS_ENFORCE_VERSION_CHECK` set.

- **`ASSEMBLE` literal sections and pinning** (#42). `label: LITERAL "…"`
  renders host-supplied text at its authored position; `label: PIN …` marks a
  source non-degradable — costed off the top and never trimmed, with
  **`CAL-E122`** when the pins alone exceed `BUDGET`. A compliance-mandated
  instruction can now live in the statement instead of as a mutable grain, and
  cannot be summarised away by a long conversation. Render order is documented
  as FROM-clause order, explicitly independent of `PRIORITY`, with a test.
  **Out-of-order `ASSEMBLE` clauses are now a parse error** rather than
  silently detaching. New CAL syntax ahead of the OMS spec — recorded as a
  named decision in `ARCHITECTURE.md` §10.
- **A host-supplied anonymization key** (#46). `AreevOptions::anon_key` is the
  HKDF root for the session/memory/vault subkeys when given, else the page key
  as before. The mapping vault and deterministic value-derived tokens now work
  on **Postgres** — which refuses `encryption_key` because it is a page-cipher
  capability — and on plaintext files. Never persisted; rotating it is a
  crypto-erasure of the mapping table. Conformance case on both backends.
- **Healthcare / national-ID detectors and CI-testable fixtures** (#47).
  Singapore NRIC/FIN (weighted mod-11 with era offsets) and UAE Emirates ID
  (`784` prefix + Luhn) are checksum-gated; MRNs are cue-gated on a nearby
  `MRN`/`medical record number` rather than matching bare digit runs.
  `co_occurrence` rules express "redact A when B is within N characters" — a
  name beside a condition is health data, which no per-category action can
  say — and `term_sets` name the categories they compare. `areev anonymize
  test --fixtures F` asserts must-redact / must-not-redact and exits non-zero
  on any miss or false positive.
- **Pluggable LLM credentials and feature-gated providers** (#45).
  `areev_llm::cred::Credential` mints the auth value per request instead of
  reading a `String` once, so Application Default Credentials work: a
  `vertex:<model>` provider reaches the **regional** `aiplatform` endpoint under
  workload identity with no key on disk (the region is never defaulted and
  `global` is refused). Service-account key JSON is refused by name — signing
  its JWT needs an RSA dependency this tree does not carry. Providers are
  individually feature-gated; **OpenRouter is off by default**, so a regulated
  build can state that its artifact cannot reach a third-party router.
- **A parsed-statement cache, and `calPrepare`** (#44). The executor caches
  parsed statements by exact text, so a real-time turn stops re-lexing and
  re-parsing on every turn — and it serves every surface, not just one. The
  bindings built a fresh executor per `cal()` call and so could never hit a
  cache; one executor now lives on the handle. `calPrepare`/`cal_prepare`
  validates and warms a statement at startup. `RESULTS.md` §1b adds measured
  binding-level p50/p95/p99 for `RECALL`, a three-source `ASSEMBLE`, and
  `thread_tail`, on both backends.
- **Executable, undoable definition rewrites in the loop** (#28). A proposal
  may rewrite a saved query or template — where a self-improving agent's
  prompt-assembly actually lives. `OmsSubstrate::definition_inverse` records
  the statement that restores the previous definition (or a `DROP`), so
  `ROLLBACK` really undoes it; a substrate that cannot produce one refuses the
  apply rather than applying something rollback could not reverse. Definition
  targets are excluded from auto-apply by name, like `code` and `evalset`.

- **Triggers** (#36): a standing rule that starts a workflow, declared as a
  `Trigger` grain (type `0x0D`) and evaluated by `areev trigger run` — a
  one-shot idempotent command safe to invoke concurrently. There is still no
  daemon and no scheduler; what changes is that the cadence is data in the
  memory instead of a fact buried in someone's crontab.
  - Eight kinds over four primitives: `interval`/`schedule`/`once` (Time),
    `polling` (Time + Poll), `memory` (state predicate), `webhook`/`manual`
    (Push), and `composite`. All eight fire: webhook and manual through
    `trigger deliver`, composites settled in the same evaluator pass as
    their members. (This bullet originally claimed the last three would
    "fire in a later release" — stale before it shipped; `docs/triggers.md`
    was always right. Corrected 2026-08-22, #87.)
  - Idempotency by construction: the run id is derived from
    `(trigger, connector, dedup value)`, so a re-delivered item is one run and
    one recorded skip. Correctness does not rest on the lease — the lease only
    prevents duplicate connector calls.
  - The first poll seeds the cursor and fires nothing, so declaring a mailbox
    trigger does not replay history.
  - `--catchup last|none|all` and `--concurrency forbid|allow|replace` for
    missed occurrences and overrun.
  - Connectors reuse the `--tool-cmd` seam, so there is one subprocess contract
    and they inherit its timeout, output cap and secret scrub.
  - Cron is **UTC only**; a non-UTC timezone is refused with `TRG-E006` rather
    than mishandled across a DST boundary.
  - **Outbound allowlisting** (`int:allowed_outbound_hosts`, Fermyon Spin
    semantics) and **credential brokering**: `--credential NAME=ENV_VAR` gives
    the connector `AREEV_EGRESS_URL` instead of a token, and a loopback broker
    checks the destination and attaches the credential on the way out. A
    destination outside the allowlist is refused with `TRG-E009` before any
    request is made.
  - `areev trigger render --target cron|launchd|systemd|k8s-cronjob` emits
    heartbeat config for infrastructure you already run and creates nothing. The
    rendered interval is the GCD of declared intervals floored at 60s, not the
    shortest one — the memory owns the cadence.
  - `areev trigger deliver` ingests a webhook or manual payload. Areev never
    opens a port: the host owns the listener and hands the payload over.
  - A read-only Triggers tab in the console, on the existing `/api/browse`
    surface with no new server route.
  - CAL: `RECALL triggers WHERE kind = "polling" AND enabled = true` — the
    grain-type plural set grows to 13, which is what typed queryable fields buy.
  - New docs: [`docs/triggers.md`](docs/triggers.md).

- **Run leases** (`RUN-E021`): a run is leased while a driver advances it, taken
  at start/resume, renewed at each superstep boundary, and released when the run
  finishes **or parks**. Two drivers on one run previously last-write-wins in
  the journal, silently — `journal::ingest` overwrites a second result for the
  same key and the owner-nonce check is a documented gap, so the `Tainted` doc
  comment's claim that forked tips were detected was not true of the shipped
  code. This prevents the case rather than noticing it afterwards. An expired
  lease is reclaimable, so a crashed driver does not park its run forever.

- **`areev-sandbox` (Tier C)**: a standalone package that runs a pure `wasm32`
  module with no WASI, a frozen one-function import set (`areev::emit`; `alloc` is a guest export), fuel, a memory ceiling,
  and a module-size cap applied before decode. Deliberately outside the
  workspace so `wasmi`'s tree and MSRV never reach workspace `cargo deny`, MSRV
  checks or test time; it has its own CI job. Protects the host from the tool —
  explicitly not credential protection, which is what the egress allowlist and
  broker are for.

- **`read_blob_offline` in the Python and Node bindings.** The lock-free CAS
  read added in 1.2.1 reached only the CLI, so a `--tool-cmd` subprocess
  written in Python or Node — the common case for a binding host — still had
  no way to fetch an attachment while its own run held the memory. It had to
  shell out to the `areev` binary (a second artifact to ship, pin and sign per
  deployment) or hand-roll the read and risk skipping the content-address
  verification. Same contract as the Rust and CLI paths: no database open, no
  lock, hash re-verified on read, `None`/`null` for a sealed blob.
- **`run_inspect`/`run_oversight_report` in the Python and Node bindings**
  (#34): the two read-only run reports — the frozen manifest, budgets,
  phase, spend, pending asks, and fork lineage; and the EU AI Act Article
  14 answers, measured from the journal — were CLI-only. Both are now
  thin `Runner` methods (`Runner::inspect`, `Runner::oversight_report`)
  the CLI's `areev run inspect`/`areev run oversight-report` call too, so
  a tenant-deployed Python/Node agent service renders them in-process
  instead of shelling out to the CLI binary for two read-only reports.
  `GET /api/run/inspect` on the hub/console now returns the same full
  report instead of a smaller, independently hand-rolled subset.

### Changed

- **README repositioned around adaptive agents.** The pitch led with "embedded
  memory engine" and carried a migration section comparing Areev to other memory
  stores; being another memory player is not the position. It now leads with the
  substrate for agents whose behaviour changes on evidence, under human
  authority, in steps that can be inspected, undone and re-measured — and
  explains the three systems that make that possible (graph engineering, context
  engineering, governance) plus the loop that closes them. Competitor comparisons
  are gone from the README, the package READMEs, and `README.zh-CN.md`;
  `areev migrate` remains documented in `docs/migrate.md` as a capability rather
  than a positioning. Added an Examples section linking the runnable material in
  `examples/`.

  Claim discipline follows the strategy docs' own rules: "self-improving" is
  scoped to the agent's **memory**, never to model outputs; `verify` is named by
  the tier that actually ships (**journal-consistent**) rather than the two that
  do not; `runs_touching` is stated with its limit (a run that merely *read* a
  grain leaves no grain, so nothing can attest to it); erasure reach is stated
  with the archive window it does not cover; and nothing anywhere claims to be
  "compliant".

- **`workflow_dispatch` is now a safe dry run on all three release workflows.**
  `release-npm` and `release-pypi` published to the registries for real on a
  manual dispatch from any branch; their publish jobs are now guarded on
  `github.event_name == 'release'`, matching the guard `release-cli` already
  had.
- **Release builds are `--locked`.** The maturin and napi builds resolved a
  fresh dependency graph at release time, so published wheels and native addons
  could contain a dependency set no test run had ever seen. Both now build from
  the committed lockfile, and `npm ci` replaces `npm install` where a
  `package-lock.json` is committed.
- **The release runbook publishes the GitHub Release *before* crates.io.** The
  PyPI, npm and CLI workflows build from local `path` dependencies and never
  read crates.io, so they had no reason to wait behind the twelve-crate publish
  chain — they now start immediately and run concurrently with it.
  `cargo publish --workspace` replaces the hand-maintained bottom-up tier list
  (which went stale twice and failed mid-publish), with
  `cargo publish --workspace --dry-run` moved into pre-flight.
- **Release workflows carry `concurrency` groups** keyed on the tag, so a
  re-run cannot race a manual dispatch.
- **README**: added a Quality section with the generated metrics chart; removed
  the legacy rename notice and the placeholder overview video; the status line
  no longer restates a version number that goes stale (it points at this file).
  `README.zh-CN.md` kept in sync.

- **One bounded spawn path for every host command seam** (`areev_core::proc`,
  mirrored privately in `areev-loop`, which may not depend on an areev-*
  sibling; `proc_contract.rs` pins the two together). Five hand-rolled copies
  across six seams are gone, and with them three real defects:
  - **No wall-clock ceiling.** A tool that never exited held its run-pool worker
    and then the driver itself, forever. Now 300s by default, then killed —
    surfacing as a retryable `Timeout` for tool effects rather than a hang.
    `CommandExecutor::with_timeout(None)` restores the old behaviour.
  - **No output cap.** stdout was read to EOF into memory unbounded. Now 64 MiB
    per stream, drained past the cap so the child never blocks on a full pipe.
  - **A stdin deadlock.** Every seam wrote its whole payload before reading a
    byte of output, so a child that filled the pipe buffer while still reading
    its input hung, and so did we. stdin now writes on its own thread.

### Removed

- **`Workflow.trigger`** (breaking). A free-text "activation condition" that
  nothing ever read — neither `areev-run-core` nor `areev-run` — so it described
  an activation that could not activate anything, while the console offered to
  set it. A trigger is now a `Trigger` grain that points *at* a plan, which is
  the only direction that works: a Workflow is content-addressed and a run's
  manifest pins its hash, so a plan carrying a list of triggers would change
  address every time one was added.
  - CAL's `ADD workflow "n" ON "..."` clause is removed and **refused by name**,
    with a message pointing at `areev trigger add`. Silently ignoring it would
    leave an author believing they had scheduled something.
  - Old blobs still deserialize: an unknown field is preserved and ignored, so
    this costs a vestigial key in grains already written and nothing else.
  - The console's plan subtitle becomes a read-only shape summary.

### Fixed

- **`crates/areev-js/Cargo.lock` had drifted, and nothing would have caught it
  until a release failed.** areev-js is a detached cargo workspace, so a
  dependency added to a crate it depends on never reaches its lockfile —
  `areev-run` gained `getrandom` and `ureq` for the egress broker and this
  lockfile did not follow. Dependabot's `cargo` entry for `/` does not cover it
  either. Because `release-npm.yml` now builds `--locked`, that drift would have
  surfaced as a failed **release** rather than a failed build. Lockfile
  regenerated, plus two guards so it cannot recur: the `node` CI job asserts
  `cargo metadata --locked` and now builds with the same `npm ci` /
  `--locked` flags the release uses, and `dependabot.yml` gains a `cargo` entry
  for `/crates/areev-js`.

Nine findings from an external evaluation of 1.2.2 as the context assembler
and memory for a regulated healthcare voice + chat agent (#42–#50), plus the
loop's definition-rewrite gap (#28). Every one was reproduced against the code
before it was fixed.

- **`ORDER BY` ranked a truncated window, and vanished on `ASSEMBLE`** (#43).
  A pipeline stage runs over what the statement already returned — a
  `default_limit` page — so `ORDER BY priority DESC | LIMIT 5` returned the
  top 5 *of the newest 50* and looked exactly like a correct answer.
  `CONTRADICTIONS` already widened its scan for this reason; that fix is now
  generalized to every stage with the same shape (`ORDER BY`, type-specific
  `WHERE` post-filters, `COUNT`), with the caller's bound re-applied
  afterwards and **`CAL-W015`** when even the widened scan fills. `ORDER BY
  created_at` is pushed into the scan and is exact at any size — it is the one
  sort key the `grains` table carries as a column; the rest live inside the
  content-addressed blob. `ORDER BY` on a multi-source `ASSEMBLE` now emits
  **`CAL-W016`** instead of being silently discarded. `WITH recency_weight(w)`
  is **implemented** — it was parsed, stored, and read by nothing since 1.0,
  while ten built-in saved queries passed it.
- **`session_id` was a post-filter over a 50-row page** (#49). It is now pushed
  into `idx_thread(ns, session, seq)`, so `RECALL events WHERE session_id = …`
  is bounded by turns of *that conversation* rather than rows of the namespace
  — on a busy namespace the tail of a conversation could be entirely outside
  the window and the query answered "nothing". No new CAL syntax: the existing
  `WHERE session_id` spelling now pushes down. `thread_tail` is exposed on the
  Node and Python bindings.
- **A Postgres handle never recovered from a database outage** (#48). One
  `tokio_postgres` client with no reconnect meant a routine managed-database
  restart (`57P01`) permanently poisoned a long-lived handle. The session is
  now replaced in place, clearing the prepared-statement and BM25-stats caches
  that belonged to it; **reads replay, writes do not** (a write may have
  committed before the connection died), and nothing replays inside a
  transaction. `docs/deployment-profile.md` gains the connection contract —
  connections per handle, open cost, pooling guidance — and its stale
  "advisory-locked single writer" claim is corrected to multi-writer.
- **Windows `require()` failed on a package npm had refused** (#50). The
  Windows leg built fine; npm's spam filter rejected the *name*
  `areev-win32-x64-msvc`, and the release shipped a manifest promising it
  anyway. Scoping the package makes napi derive `@areev/areev-<platform>`
  names, which the filter does not reject — Windows works rather than being
  dropped. `prepare-npm.mjs` now hard-fails a release when a declared target
  produced no artifact. Three stale proposal headers corrected.
- **The CLI aborted with no message on Windows.** Windows gives a process's
  main thread 1 MiB where Linux and macOS give 8, and the deepest paths —
  `areev loop apply` threading the argument dispatcher through the engine, the
  substrate adapter, the CAL facade and the store — sat just over it, so the
  command died with `STATUS_STACK_OVERFLOW` and no output. `main` now runs the
  CLI on a thread whose stack size it chooses, making headroom identical on
  every platform instead of depending on a number the platform picks.
- **`WITH recency_weight(0)` returned more grains than the statement asked
  for.** The re-ranking widens its candidate scan and truncates back to the
  caller's bound afterwards; the widening tested "is the option present" and
  the truncation "is the weight above zero", so a weight of exactly zero — the
  same answer as no option at all — widened and never came back, and
  `RECENT 3` answered with twelve. Both now read one predicate; zero, negative
  and NaN weights all take the unwidened path.

- **Known-identity propagation now reaches `scan_text`/`anonymize_text`**
  (#32): these free-text APIs read the store's known-identity table for the
  facade's default namespace — the same propagation table grain-egress
  reads already build — so a subject interned by an intake step (e.g. a
  `subject` written under the namespace) is now detected/pseudonymized in
  prose passed to these APIs too, not only in `recall`/CAL results.
  `AnonPolicy` grows a `known: [{value, category}]` field so a caller can
  also inject identities it holds but never interned as a grain subject
  (an email's From header, a CRM row, a project codename), each with its
  own detection category. Both APIs' signatures are unchanged; the
  bindings pick this up with no code changes.
- **A cycle's back-edge can now close on any node, not only the plan's
  entry** (#33): a bounded cycle whose re-entry point was a mid-graph node
  (e.g. `analyze -> notify -> gate -> converse -> gate`, the back-edge
  targeting `gate`) validated cleanly and then stalled the run at the
  entry on superstep 1, because the scheduler's AND-join gate required
  that not-yet-resolvable back-edge before the node could ever go Ready —
  a rule only the entry node's unconditional bootstrap sidestepped.
  `PlanGraph` now classifies every edge as a DFS back-edge or not (from
  the same entry-rooted Tarjan traversal that already computes `scc_of`),
  and a node's first activation only gates on edges that could possibly
  have resolved by then. `run oversight-report`'s stall diagnosis also no
  longer blames the entry node when its own edge fired correctly.

### Security

- **Host command seams no longer inherit named secrets.** No subprocess seam
  called `env_clear`/`env_remove`, so `--passphrase-env` (the memory's
  encryption passphrase) and `--token-env` were inherited by every child of
  `--tool-cmd`, `--embed-cmd`, `--anonymize-cmd`, `--llm-cmd`, `--analyzer-cmd`
  and `areev eval`. The CLI wrapped its own copy in `Zeroizing` and then handed
  the raw variable to every child. Both flags name a *variable*, so the names
  are now registered at argument-parse time and withheld from every spawn. The
  rest of the environment is still inherited — an `--llm-cmd` that reads its own
  API key from the environment keeps working.
- **A plan's `tool_name` is validated before it reaches a child.** It arrives as
  `$AREEV_TOOL_NAME` and can come from an imported bundle (import verifies
  content integrity, not authorship). Names outside `[A-Za-z0-9_.-]{1,64}` are
  refused at `run start` rather than mid-superstep.

## [1.2.2] — 2026-08-18

### Added

- **A Workflows tab in the console** (#37): lists Workflow grains as cards
  and opens one into an editable node/edge graph — a deterministic
  left-to-right layered layout on canvas, add/rename/delete a step, rebind
  it to any Tool definition, drag a step's connector dot to wire it to
  another step, set/clear an edge's `WHEN` condition. Saving always writes
  a new `ADD workflow` grain, since plans are content-addressed and
  immutable and "editing" one means authoring a new version; a plan with a
  bounded-cycle edge or a per-node retry count opens **view-only**, because
  `ADD`/`SUPERSEDE workflow` has no surface syntax yet to author either
  (`* N` populates `retries`, not `max_cycles`). No new server routes —
  built entirely on the existing `/api/browse` and `/api/cal` surface.
  `crates/areev-store/examples/seed_workflow_demo.rs` seeds three demo
  plans into the "Northwind Support" corpus.
- **An Analytics tab in the console**: a grain-type census across all 12
  types, a namespace breakdown, a 14-day growth trend, and recall-leg
  status — generalizes the Query page's "WHAT'S IN THIS MEMORY" on-ramp
  (now removed from Query in favor of it) to cover every grain type
  instead of 4, and every namespace instead of just the bound one.

### Fixed

- **Workflow edge arrowheads were never visible, and edge selection didn't
  line up with what was drawn.** The graph stroked each edge along a
  border-adjusted bezier curve but evaluated the arrowhead position and
  click hit-testing on a different curve through the raw node centers, so
  the arrowhead landed inside the destination node (painted over by its
  opaque fill) and a click near an edge sampled a curve offset from the
  one on screen. Both now read off the exact curve that gets stroked.
- **A node bound to another plan ("subgraph") showed as "unbound" in the
  editor's "Runs as" picker**, contradicting the "Subgraph" badge shown
  directly above it — the option list was built from Tool definitions
  only, with no entry for a Workflow-grain target.
- **A crafted `BIND` binding could inject arbitrary CAL into a plan's save
  statement.** Every other value the Workflows editor writes into
  `ADD workflow` (node names, `WHEN`, the trigger, the reason) is quoted;
  the bound hash was spliced in bare. A plan opened in the console can
  have been authored outside it (the Rust/Python/Node API, or a synced
  bundle), so a binding value crafted to look like a hash followed by more
  CAL could append clauses — rebinding other steps or overriding the
  reason — the moment someone reopened and resaved that plan through the
  UI. The hash is now validated against the content-address format before
  it reaches the statement.
- **Drawing a cycle in the workflow editor saved silently and only failed
  later, at run time.** Every edge the console can author is
  unconditionally unbounded (`ADD workflow` has no syntax to re-emit a
  bound on save), so any cycle drawn through the editor was guaranteed to
  fail at run-load with `RUN-E002`. Connecting an edge that would close
  one is now refused up front.
- The sidebar's Workflows nav item didn't reset an open draft or selection
  the way navigating to a bare `#workflows` hash already did, so clicking
  it while mid-edit just re-rendered the same editor instead of returning
  to the plan list.
- Query's "start from a question" examples wrote hardcoded placeholder
  subjects (`"john"`, `"acme-corp"`) that almost never match a real
  memory's own data, so the first thing a new user tried reliably came
  back empty. They now pull an actual subject and value from the file's
  own Facts, falling back to filter-free forms only when the file has none
  yet.
- Console-wide: one shared namespace-picker component ("Namespace  value
  ⌄") replaced three different layouts across Activity, Workflows, and
  Analytics, each with its own alignment quirks; every native `<select>`
  in the console (the "Runs as" picker above, the anonymization policy
  picker) now matches the rest of the UI instead of the browser's default
  box; the "Areev" brand mark is clickable (home) and aligned with the nav
  icons below it; the breadcrumb home icon's optical alignment against its
  trail text.

## [1.2.1] — 2026-08-17

### Fixed

- **A grain carrying a `subject` without a relation or object reached no
  index at all** (#23). Structural indexing required all three positions, so
  an Event *about* a message id or a person was invisible to
  `recall(ns, subject, …)` — a silent empty result on a filter every surface
  accepts. The same root cause was the serious one: `forget_subject` and
  `subject_report` select through those indexes, so the identity's own grain
  survived erasure and went **undisclosed in a DSAR**, while the erasure
  reported success. Such grains now get a subject-anchored row (relation and
  object NULL, because the grain asserts neither — which also keeps the row
  inert to every relation-bound query). Never written to `heads`/
  `entity_latest`: a log entry about a subject has no "current value". Existing
  files are healed on open by a `link_index` stamp bump; the rebuild replays
  the rows and reconstructs `cur` from supersession state, so a reindex neither
  duplicates a grain nor resurrects a superseded one. Pinned on both backends
  (`subject_without_relation_is_indexed`).
- **`DEFINE QUERY` stored bodies that could never `RUN`** (#24). Define-time
  validation skipped parsing entirely whenever the body contained `$` — the
  shape most saved queries have — and fell back to a keyword blocklist, so any
  syntax error was stored and first surfaced when a caller ran it, typically an
  unattended agent long after the author had moved on. The body is now parsed
  at `DEFINE`. Bodies whose parameters sit in positions demanding a literal
  (`RECENT $limit`) are still accepted: the check re-parses with the parameters
  standing in, so only a body malformed *however* it is bound is refused
  (`CAL-E059`). The read-only and destructive guards are unchanged.
- **A Skill's `instructions` could not be reached through any rendered path**
  (#25). The field that *is* the skill was absent from the grain type's
  queryable fields (`PROJECT name, instructions` → `CAL-E060`) and no format
  emitted it, leaving raw JSON recall — which defeats budgeted assembly — as
  the only way to read it. `instructions` and `when_to_use` are now projectable
  and render at full disclosure.

### Added

- **`WITH progressive_disclosure(summary|headlines|full)` now executes**
  (#25). It was documented in `docs/cal-reference.md` but parsed and discarded,
  warning `CAL-W004`. It is the *body* axis, orthogonal to metadata: `summary`
  and `headlines` clip free-text bodies (40/80 chars, the same ladder budgeted
  template renders already use), and `full` leaves them whole **and** adds the
  long-form definition bodies no other tier carries — a Skill's `when_to_use`
  and `instructions`, so they reach a budgeted `ASSEMBLE` instead of being
  injected around it. Omitting the option renders exactly as before, byte for
  byte.
- **The CAS blob store reaches the CLI and both bindings** (#27):
  `areev blob put <FILE>|--stdin` prints the `cas://` URI (idempotent by
  construction), `areev blob get <cas-uri>` writes hash-verified bytes to
  stdout, and `put_blob`/`get_blob` ship in Python and Node — bytes in, bytes
  out, the one documented exception to the scalars-in/JSON-out convention.
  `blob get` deliberately **does not open the memory**: the embedded backend's
  file lock is exclusive, so while a run holds a memory even a reader is
  refused, which put an attachment out of reach of the very `--tool-cmd`
  subprocess the run spawned to process it. Reading the sidecar needs no lock
  and answers no consistency question — a blob is immutable and its address is
  its checksum, re-verified on read. Encrypted memories still open, since
  decryption needs the derived key. No MCP tool, deliberately: blob bytes would
  have to be base64'd into a tool result and land whole in the model's context.
- **Evalset-backed outcome metrics** (#29). A recommendation may carry
  `metric = "evalset:<EVALSET_HASH>:<field>"`, resolved by `areev loop outcomes`
  from the summaries `areev eval run` journals — `passed`, `failed`, `total`
  and `error_rate` work against any evalset, and any other field is read from
  the summary the harness wrote. This moves the honesty boundary legitimately
  rather than breaking it: an evalset run is itself an internal, bounded,
  attributable measurement. Two safeguards are load-bearing. A run journaled
  **before** the apply is never evidence (no run since → not yet measurable,
  and the checkpoint stays due; scoring the baseline against itself would
  report `held` forever, a fabricated receipt). And `MetricSnapshot.higher_is_better`
  states the direction, because the built-in metrics are recurrence counts
  where lower is better while an accuracy is the opposite — read the wrong way,
  the Verify gate would propose reverting the rules that worked. The regression
  comparison now lives in one function both the engine and `outcome_review`
  call. The apply gate (`--gating-run`) and the outcome edge read those
  summaries through one shared reader, so a rule cannot be admitted on one
  reading of an evalset and judged on another.

## [1.2.0] — 2026-08-17

### Added

- **Namespace prefix scoping (`"org.*"`)** — one convention on every read
  surface (CAL `WHERE namespace` / `namespace IN (…)`, the MCP `namespace`
  argument, `areev recall --ns`, ASSEMBLE sources, both bindings): a
  namespace value ending in `*` selects the base namespace **plus its
  descendants through the separator you wrote** (`"org.*"` = `org`,
  `org.sales`, `org.sales.emea` — never `organization`, never `org:x`).
  Malformed patterns (`org*`, bare `*`, mid-string `*`) refuse with
  `VAL-E001` instead of silently matching nothing. Backed by a
  count-maintained namespace registry (`ns_reg`, self-healed on open for
  older files) and a namespace-set recall path through all three hybrid
  legs; the single-exact-namespace hot path is untouched. Scopes widen
  **reads only**: `*` is now reserved in namespace names (writes refuse it;
  replication of pre-existing files still imports), and destruction,
  grants, policy, and point reads keep taking exact namespaces. Under a
  bound principal a prefix expansion **fails closed** — every covered
  namespace must be granted, and the refusal names the pattern, never a
  discovered namespace.

### Fixed

- `WHERE namespace IN (…)` now queries **every** member of the set (union,
  deduped, newest-first across the set); previously only the first member
  was consulted and the rest were silently dropped (#19). A
  `namespace_override`-pinned session now also clears caller-supplied `IN`
  sets, closing the corresponding pin-escape.

## [1.1.0] — 2026-08-16

### Added

- **Anonymization: prompt-safe pseudonymization** (`areev anonymize`,
  cookbook recipe 16). Declare one `anon:<ns>` policy — a file-truth that
  replicates write-if-absent and fails reads closed when unreadable — and
  every model-facing read (recall/search/CAL/MCP/graph reads) returns typed
  placeholders (`[PERSON_1]`) instead of identities:
  - **Detection** is layered: built-in Tier-0 (structural known-identity
    propagation, regex + Luhn/mod-97 validators, secrets, keyword cues,
    dictionaries), a pluggable NER command seam (`--anonymize-cmd`), and a
    grounded LLM detector (`--anonymize-llm-cmd`) — a policy demanding an
    uninstalled detector fails closed. Actions: `pseudonym`, `mask`,
    `redact`, `generalize:month|year|decade`, `allow`.
  - **The round trip**: mappings stay in process custody
    (`anon_mappings()`, `rehydrate_text()`; payloads carry an `anonymized`
    report with mapping *ids* only). `PseudonymizingBackend` wraps any
    `LlmBackend` so extraction requests leave pseudonymized and responses
    return rehydrated.
  - **Ingress mode + `memory` scope** (encrypted memories): value-derived
    tokens transform *before* the content address commits; `FORGET
    SUBJECT`/`REPORT SUBJECT` recompute the stored pseudonym from the real
    identity, so pseudonymized-at-rest never means erasure-proof.
  - **The sealed vault** (`vault:` rows under an HKDF subkey of the page
    key; never replicated; erased with the subject; TTL-swept): tokens
    continue across processes, and `areev anonymize reveal` /
    `reveal_tokens()` is admin-gated and Tier-2 audited by fingerprint.
  - Surfaces: CLI verb family + `--anonymize-egress` host floor, Python and
    Node methods in lockstep, the console's Anonymization card + per-grain
    "Model view" (`GET /api/anon/preview`, `POST /api/anon/config`),
    `/api/config` observability, conformance cases on both backends
    (Postgres: egress/audit work; value-derived features refuse loudly —
    no page cipher there).
  - Explicit text APIs ship too: `scan_text` / `anonymize_text` /
    `rehydrate_text` and the store-free `areev anonymize scan`.
  - Honest scope, by design: this is **pseudonymization** of the egress
    channel, not anonymity — see `docs/security-model.md` and
    `ARCHITECTURE.md` §10 for the threat model and named decision.
- **`min_reader_version` stamping on anonymization policies** so older
  builds warn loudly at open; `anon:` joins the replicable meta prefixes,
  `vault:` is reserved and never replicates.

### Changed

- **One rendering stack.** Per-grain
  rendering now has a single implementation — `areev_cal::render` — shared
  by CAL's `FORMAT` arms and `areev-context`, with byte parity pinned by a
  cross-surface golden. Output changes that follow:
  - `FORMAT sml` emits semantic per-type elements
    (`<fact confidence="0.95" date="2026-01-13">john prefers window
    seat</fact>`) instead of generic `<grain type=…>` field dumps; event
    elements carry the speaker as `role="…"`.
  - `FORMAT markdown` gains dedicated arms for state / workflow / reasoning /
    consensus / consent / recommendation grains (topology and labels instead
    of a raw field-pair dump); fact/event/tool lines are byte-identical to
    before.
  - `recall --render` (markdown/json/toon/plain) converges on the CAL
    shapes: markdown carries the documented `- ` bullet and the
    confidence-below-1.0 rule, json is the `{hash, grain_type, fields}`
    envelope, toon rows come from the registry columns.
  - `FORMAT toon`'s `state` rows read the OMS §8.3 `context` key (previously
    `context_data`, which never matched — rows always fell back to
    `state,state`).
  - One `chars/4` token estimator (`render::estimate_tokens`) serves
    `ASSEMBLE … BUDGET` and the areev-context allocators, so a budget means
    the same thing on every path.
- **Progressive disclosure is real.** The context allocators emit
  Full→Summary→Omit (70%/95% thresholds); budgeted `FORMAT TEMPLATE` renders
  pick their disclosure tier from tokens-per-grain, so `ELEMENT_SUMMARY`
  fires under pressure and `ELEMENT_OMIT` accounts for dropped grains —
  behavior the reference already promised. JSON and TOON stay whole-entry
  (a prose summary inside a structured dump would corrupt it).
- **The registry replicates.** Bundles/segments carry saved queries,
  templates and retention policies in a v2 `MGB2` meta segment (emitted only
  when the file has registry rows — registry-free bundles stay MGB1 and
  readable by older builds; older builds refuse an MGB2 bundle loudly).
  Import merges latest-wins on `updated_at`; `last_run_at` never replicates
  and survives locally; retention rows apply only when locally absent; a
  point-in-time restore skips the segment. New conformance cases cover both
  backends; `ImportStats` gains `meta_applied`/`meta_skipped`.

### Removed

- The six whole-result builtin templates (`triples`, `progressive`,
  `llm_system_prompt`, `llm_chat`, `weekly_standup`, `toon`) — unused, and
  `toon`/`triples` shadowed the same-named `FORMAT` arms with different
  output. Builtins are now exactly the three §10.1 sectioned presets
  (`structured`/`readable`/`compact`), and a builtin can never take a
  `FORMAT` arm name. `FORMAT TEMPLATE toon` now returns `TemplateNotFound`
  — use `FORMAT toon`.
- The never-wired `CalExecutorConfig::max_cal_queries`/`max_cal_templates`
  caps (no host set them, and their `Some(-1)` = unlimited convention was
  implemented backwards). The registry-level limits (100 queries/namespace,
  50 templates, body-size caps) remain the enforcement.
- Dead `areev-context` dependency declarations in `areev-py`, `areev-js`,
  and `areev-server`.

### Docs

- Saved queries and templates are now discoverable where agents look:
  the `cal-for-llms.md` grammar card gains a SAVED block, the MCP reference
  documents the `DESCRIBE QUERIES` → `RUN` pattern under `areev_cal`, and
  cookbook recipe 15 walks the ship-assembly-logic-in-the-file pattern
  (the Hermes provider's override). `llms.txt`'s MCP tool count corrected
  (14 → 23); `docs/facts/context-assembly.md` re-verified.

## [1.0.2] - 2026-08-16

### Fixed

- **`verify` on canceled runs, at every cancel phase.** Replay fed
  `CancelSeen` at a superstep's open whenever the coming checkpoint
  carried the cancel — a phase the live driver only produces when the
  marker predates the run, so a cancel landing during the first
  superstep (before any checkpoint) failed verify on slow machines.
  Replay now places the cancel by the journal's own evidence: with the
  wave's resolutions when the closing checkpoint shows they ran, or by
  rewinding the boundary and feeding it first when the journal shows
  the live driver canceled before dispatching. A new phase-sweep test
  exercises every placement on every machine.
- **Windows `--tool-cmd` quoting.** The 1.0.1 `cmd /C` fix still routed
  the command through `Command::arg`, whose MSVC quoting `cmd.exe` does
  not parse; the command string now goes through `raw_arg`.
- **RUSTSEC-2025-0134.** Replaced the unmaintained `rustls-pemfile`
  with `rustls-pki-types`' PEM support (already in the tree via
  rustls); the `tls` feature's surface is unchanged.

## [1.0.1] - 2026-08-16

### Fixed

- **`areev run` wave determinism.** The driver fed effect completions to
  the pure scheduler in racy arrival batches, each with its own clock
  reading — scheduler state depended on thread timing (an unjournaled
  decision), so two identical runs could checkpoint differently and
  `areev run verify` could diverge from a live run under load. The driver
  now drains every dispatch wave fully and feeds one close reading plus
  all resolutions in dispatch order — exactly the cadence `verify`
  replays. Journal-answered replays join the same wave rather than
  resolving early.
- **Windows `--tool-cmd`.** `/bin/sh` was hardcoded in the host tool
  executor and the eval seam; both now use the platform shell
  (`cmd /C` on Windows).
- **`areev-run-core` purity gate.** Dropped the workspace's only `chrono`
  use (a `created_at` fallback in canonical serialization, now
  `std::time`), so the CI gate that keeps clock/rand/IO out of the pure
  scheduler's dependency tree actually passes.

## [1.0.0] - 2026-08-16

The first Areev release — the complete memory engine, plus the
governed-agents program: the `areev run` runtime, agent-grade capture, the
ecosystem adapters, and the enterprise plane.

### The memory engine

- **Immutable, content-addressed grains** in the `.mg` format — 12 grain
  types, canonical serialization (NFC, sorted keys, omit-defaults), SHA-256
  content addressing. Every edit is a supersession, every removal a
  tombstone or crypto-erasure; nothing ever rewrites a stored blob.
- **One memory = one isolation unit** — a single file on the embedded Turso
  backend, a schema on the PostgreSQL backend (`feature = "postgres"`,
  advisory-locked writers, pgvector) — the unit of erasure, sync,
  portability, and write parallelism. Files are self-describing: saved
  queries, templates, and index declarations travel with the file.
- **Hybrid recall in microseconds** — dictionary-encoded triples, an owned
  BM25 inverted index, optional vector recall via a pluggable embedder
  (`--embed-cmd`), graph/time reads (`related`, `entity-at`,
  `step-actions`), heads/forks with explicit merges, bundles, encrypted
  incremental sync, and CAS blob storage (encrypted under an HKDF-derived
  subkey when the memory is).
- **CAL — the Context Assembly Language** — lexer/parser/executor,
  `ASSEMBLE` with facade mounts for cross-memory queries, and budget-aware
  SML/TOON/Markdown/JSON rendering for model-ready context.

### Governance

- **Authorization in the file** (CAL 1.3): grants ride as `mg:permits`
  Facts; destruction (`FORGET <hash>`, `FORGET SUBJECT`, `PURGE OLDER
  THAN`) is authorization-gated with mandatory `BECAUSE` and a Tier-2 audit
  Observation on every execution; `REPORT SUBJECT` shares one selector with
  erasure so a DSAR discloses exactly what an erasure removes.
- **GDPR compliance pack** — [`docs/gdpr.md`](docs/gdpr.md) article→
  capability map, DSAR `subject-report` on every surface, `audit export`,
  declarative `retention:<ns>` policies, and erasure that names its
  subject by fingerprint, never by identity.

### Areev Loop — governed self-improvement

- Substrate-agnostic engine: 13 deterministic analyzers, four gates, a
  recommendation lifecycle with pinned evalsets, the DISCOVER→GROUND→VERIFY
  LLM verifier, outcome measurement across horizons, and out-of-box LLM
  backends (OpenAI-compatible / Anthropic / Ollama). Trajectory capture,
  `analyze_only` replay against the immutable past, and `areev corpus`
  export with erasure-aware provenance.

### `areev run` — the governed runtime

- A pure sans-IO scheduler (`areev-run-core`: `step(env, state, events) →
  (commands, state)`, frozen condition grammar, plan validation, `RUN-Ennn`
  errors, no clock/rand/IO in its dependency tree — CI-enforced) under a
  journaling driver (`areev-run`): intent-before-effect journal grains,
  checkpoints, crash-safe resume with same-key redelivery, HITL respond
  with separation of duties, budgets, cancel, and journal-consistent
  `verify`.

### Surfaces

- **`areev`** — the CLI (~29 verbs), including `migrate` importers from
  other memory systems, `hub` (the areevd sync daemon), `ui` (the embedded
  web console: memory browser, interactive graph, loop review queue, runs
  tab), and `hook claude-code` session capture.
- **MCP** — 23 tools over newline-delimited JSON-RPC 2.0 on stdio,
  protocol rev `2025-06-18`.
- **Bindings** — Python (`pip install areev`, abi3, sync + async) and
  Node (`npm install @areev/areev`, napi native addon; the unscoped `areev` name is pending an npm similarity-filter exception), same facade, scalars in /
  JSON out.
- **Adapters** — `areev-langgraph` (checkpointer, store, memory saver) and
  `areev-crewai` (storage backend, knowledge source, audit listener) on
  PyPI.

### Benchmarks

- Reproducible latency, honesty, and LoCoMo-accuracy harnesses in
  `crates/areev-bench` (`RESULTS.md` has the numbers), with perf gates
  (`bench`, `voice_loop`) run as examples.

[Unreleased]: https://github.com/AreevAI/areev/compare/v1.6.3...HEAD
[1.6.3]: https://github.com/AreevAI/areev/compare/v1.6.2...v1.6.3
[1.6.2]: https://github.com/AreevAI/areev/compare/v1.6.1...v1.6.2
[1.6.1]: https://github.com/AreevAI/areev/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/AreevAI/areev/compare/v1.5.2...v1.6.0
[1.5.2]: https://github.com/AreevAI/areev/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/AreevAI/areev/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/AreevAI/areev/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/AreevAI/areev/compare/v1.3.1...v1.4.0
[1.3.1]: https://github.com/AreevAI/areev/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/AreevAI/areev/compare/v1.2.2...v1.3.0
[1.2.2]: https://github.com/AreevAI/areev/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/AreevAI/areev/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/AreevAI/areev/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/AreevAI/areev/compare/v1.0.2...v1.1.0
[1.0.2]: https://github.com/AreevAI/areev/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/areev/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/areev/releases/tag/v1.0.0
