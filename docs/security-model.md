# Areev Security Model & Threat Model

This document describes Areev's trust boundaries, what its defenses do and do
not protect against, and how to deploy it safely. It complements
[SECURITY.md](../SECURITY.md) (which covers vulnerability reporting).

> This model is written to be **honest about current
> limitations** rather than aspirational. Where a protection is partial or
> planned, it says so.

## What we are protecting

The asset is **agent memory** — often personal, long-lived, and sensitive
(conversations, facts about people, decisions, credentials an agent was told).
The primary goals are **confidentiality** (at rest and in transit) and
**integrity** (a grain cannot be silently altered).

## Trust model at a glance

Areev is an **embedded** engine, like SQLite. Its baseline trust boundary is
**the local process and the user who runs it**. Everything below is layered on
top of that.

| Surface | Transport | Trust boundary | Auth |
|---|---|---|---|
| Library (`areev-*` crates) | in-process | the host program | n/a |
| CLI (`areev`) | local process | the invoking user | filesystem perms |
| MCP server (`serve --mcp`) | stdio | the parent process that spawned it | inherited |
| Web console (`areev ui`) | HTTP/1.1 | **loopback only by default** | none, or `--token-env` (Basic/Bearer on every request) |
| Sync hub (`areevd`) | HTTP/1.1 | networked peers | bearer token (writes + sync) |

## Data at rest

- **Encryption at rest** is optional and off by default. When enabled, the
  memory database (grains, indexes, op-log, and WAL) is encrypted with
  **AES-256-GCM** via the underlying storage engine's page cipher.
- **Key derivation.** The CLI derives the 32-byte key from a passphrase using
  **Argon2id** (OWASP-recommended parameters: 19 MiB memory, 2 iterations).
  The non-secret salt and parameters live in a `<db>.kdf` sidecar created on
  first use. Applications embedding the library may instead supply a raw
  32-byte key directly.
- **Key handling.** Passphrases and derived keys are wrapped in `Zeroizing`
  buffers and wiped from Areev's memory after use. (The passphrase is read
  from an environment variable via `--passphrase-env`, never a command-line
  argument, so it does not leak into shell history or the process table.)
- **Crypto-erasure.** Because the key is never written to the file, destroying
  the passphrase (and the derived key) renders the data unrecoverable — a fast,
  durable delete of an entire encrypted memory.

### Anonymization keys and the sealed vault

- **Subkeys derive from ONE root** via HKDF with distinct domain-separation
  strings: `areev.blobs.v1` (the CAS sidecar, above), `areev.anon.memory.v1`
  (value-derived pseudonym tokens — ingress mode and `memory` scope), and
  `areev.vault.v1` (the sealed placeholder→value vault: AES-256-GCM per row,
  the `vault:<ns>:<placeholder>` row key bound in as associated data).
- **The root is `AreevOptions::anon_key` when the host supplies one, else the
  page-cipher key.** With the page key as root, destroying it destroys all
  three subkeys — **crypto-erasure reaches the vault by construction**. With a
  host-supplied root the same property holds against *that* key instead: the
  vault rows are unreadable without it.
- **Trust model for a host-supplied `anon_key`.** It is a 32-byte secret that
  belongs in a KMS or secret manager (Cloud KMS, AWS KMS, Vault) and is
  supplied per process. **Whoever holds it can re-identify every pseudonym in
  the vault** — it is exactly as sensitive as the plaintext it protects. It is
  **never persisted**: not in `meta`, not in a bundle, not in the file, so it
  cannot leak through sync or export. **Rotating it is destructive**: existing
  vault rows become permanently unreadable and every value-derived token
  changes, so a rotation is a crypto-erasure of the mapping table and a break
  in pseudonym continuity, not a routine hygiene step. Rotate deliberately —
  to revoke re-identification — and re-derive nothing.
- **How a host supplies it.** `AreevOptions::anon_key` in Rust; `--anon-key-env
  VAR` on any CLI command; `anon_key=`/`anonKey` on the Python and Node
  constructors (64 hex characters — the scalars-in FFI convention). The CLI
  takes the **variable name**, never the key itself, so it stays out of shell
  history and `ps`, and that variable joins `--passphrase-env`/`--token-env` in
  the deny-list every subprocess seam scrubs. Supplying a key makes the open
  explicit, which re-stamps the file's declarations and is reported through
  `open_warnings()` — the same trade `--passphrase-env` has always made.
- **Why it exists.** The Postgres backend refuses `encryption_key` outright (it
  is a page-cipher capability), so when the page key was the only possible root
  the mapping vault and deterministic tokens were unavailable on exactly the
  backend built for stateless hosts — Cloud Run, ECS, Kubernetes. Separating
  the anonymization root from encryption-at-rest fixes that, and lets a file be
  encrypted under one key and pseudonymised under another, which is what
  separating the two roles is for.
- **No root, no value-derived features.** With neither an `anon_key` nor a page
  key, ingress modes, `memory` scope, and the vault refuse loudly at policy
  `set` rather than degrade to unkeyed derivation (conformance-pinned on both
  backends), and the refusal names `anon_key` as the fix that works everywhere.
  The egress session's `mapping_id` key falls back to a per-handle random key
  held only in memory.
- **The vault never replicates.** `vault:` rows are excluded from bundle
  export, and the import allowlist refuses them from a crafted bundle — the
  re-identification table must not travel.
- **Erasure reaches the vault.** `FORGET SUBJECT` decrypt-and-compares the
  vault rows and scrubs every live in-process mapping naming the erased
  identity; a failure there is an error, never best-effort.
- **Reveal is a privileged act.** Reverse lookup requires the `admin` verb
  and writes a Tier-2 audit Observation carrying value *fingerprints*,
  never identities. Pseudonym mappings otherwise stay in process custody:
  bounded in memory, returned only to in-process callers, and carried on
  MCP/server payloads as mapping **ids** only.
- **The free-text APIs read, never write, the trust boundary.**
  `scan_text`/`anonymize_text` stay pure-text-in/JSON-out (no grain writes),
  but they now read the store's known-identity table for the facade's
  default namespace — the same propagation table grain-egress reads
  already build — so a subject interned by an intake step is caught in
  prose passed to these APIs too. `AnonPolicy.known` lets a caller inject
  identities it holds but never interned as a grain subject, each with an
  explicit category; those never touch the store either.

### Known limitations at rest

- **The `.blobs` CAS sidecar is encrypted** when the memory is: AES-256-GCM
  under a key HKDF-derived from the page-cipher key (domain-separated, so a
  leaked blob key does not open the database), with the content address bound
  in as associated data. The `cas://sha256:` address stays the digest of the
  **plaintext** so addresses are stable across encrypted and plaintext stores
  — the documented cost is that a blob filename is a content-equality oracle
  (someone holding a candidate file can tell whether this memory stores it).
  ⚠️ Attachments written by a build from before this landed stay plaintext
  until migrated: run `areev blobs encrypt` and check `open_warnings()`.
- ⚠️ **The encryption feature depends on the storage engine's *experimental*
  AES-GCM implementation** (a pinned Turso dependency). Treat encryption at
  rest as **defense-in-depth**, not a replacement for full-disk encryption on
  the host.
- ⚠️ **Losing the `.kdf` sidecar** means the passphrase can no longer re-derive
  the key. Back the sidecar up alongside the database.

## Data in transit (sync & hub)

- Sync ships **bundles/segments** (`.mgb`) of immutable grains between files and
  peers. Applied grains are re-hashed on import; a grain whose content does not
  match its content address (SHA-256) is rejected.
- The **hub** (`areevd`, started with `areev hub --dir DIR --token-env VAR`)
  requires a **bearer token** on all mutating and segment endpoints — including
  `GET /api/segment*`, so listing and pulling bundles are gated too, not just
  pushes. The token is compared in **constant time**. Segment names are
  sanitized to a single path component (no directory traversal). `--token-env`
  is **mandatory** for `areev hub`: unlike the console there is no
  trusted-local-operator default, because a hub exists to be written to by other
  machines. A pushed segment is an **op-log replay**: it adds grains and
  applies tombstones — including erasure tombstones, which is how a subject's
  erasure reaches the hub's store (a tombstone deletes only the exact grain
  hash it names, and its sole-referenced CAS attachments; it can never delete
  by predicate).
- The **web console** (`areev ui`) is unauthenticated by default (loopback,
  trusted local operator). Pass `--token-env <VAR>` to require a shared secret
  on **every** request — the console page, all reads, and all writes. Browsers
  authenticate through the native HTTP **Basic** prompt (any username; password
  = the token); scripts may send `Authorization: Bearer <token>`. The token is
  compared in constant time, and a `401` carries `WWW-Authenticate: Basic` so
  browsers prompt. Naming an env var (not a flag) keeps the secret out of argv
  and shell history.
- **Multi-principal console** (`areev ui --auth areev-auth.json`): the credential
  map resolves a token to a *principal name*; the rights come from the memory
  file's own grant grains, and unauthenticated requests run as `anonymous`
  (read-only unless the file grants more). It changes **who** a request is, not
  **whether** a secret is required: pass `--token-env` alongside it and every
  request must still carry a recognized credential — either a map token (binds
  that principal) or the shared secret (the implied admin). A credential whose
  `env` variable is unset or **empty** authenticates nobody; an empty
  `Authorization: Bearer ` never resolves.
- **Trusted-header SSO** (`areev ui --sso-header NAME --sso-secret-env VAR`):
  an authenticating proxy does the OIDC/SAML handshake and forwards the
  identity in `NAME`; Areev honours that header **only** when the same request
  carries the proxy shared secret in `x-areev-proxy-secret`, compared in
  constant time. A forged identity header without the secret is *ignored* — the
  request proceeds as whatever its other credentials make it, rather than being
  rejected, so the header is never attacker-controlled input. Rights still come
  from the file's grant grains: the secret decides who may **assert**, never
  what an asserted principal may **do**. It is nevertheless an
  impersonation-grade credential, since the identities it can assert include
  approval-capable principals whose name IS the audit record on a
  human-in-the-loop decision. `--sso-secret-env-next VAR` accepts a second
  secret during a rotation window so an operator is never choosing between an
  outage and a deferred rotation; both candidates are compared every time
  (`fold`, never `any` — short-circuiting would leak *which* secret matched
  through response timing), the console warns on every start while the window
  is open, and rotating to the same value is refused. Procedure:
  [runbooks/sso-secret-rotation.md](runbooks/sso-secret-rotation.md).
- Import is **DoS-hardened**: an untrusted `.mg` blob is size-capped and its
  msgpack framing is validated iteratively before decoding, so a hostile grain
  cannot cause a stack overflow (deep nesting) or a giant pre-allocation (a
  short header claiming a huge length).
- The HTTP server bounds per-connection bytes, caps header size/count, and sets
  read/write timeouts (slowloris mitigation).

### Known limitations in transit

- ⚠️ **TLS is optional and non-default; plaintext is what you get if you don't
  ask for it.** Both `areev ui` and `areev hub` can terminate TLS natively via
  the non-default `tls` build feature (`--tls-cert`/`--tls-key`, rustls — no
  plaintext downgrade, tested), or you can front either with a
  **TLS-terminating reverse proxy**, which stays the documented default
  deployment shape (see `docs/deployment-profile.md`) — native TLS exists for
  deployments with nowhere to run one, not as a replacement for the proxy
  profile. A plain build/run of either surface is plaintext HTTP either way.
  Both refuse to bind a non-loopback address unless you pass `--allow-remote`
  (and even then warn loudly). `--token-env` authentication is **not** a
  substitute for TLS: without it (native or proxied), the token and all
  memory still cross the wire in the clear, so `--token-env` guards against
  unauthorized clients but not against a network eavesdropper.
- ⚠️ **Integrity, not authenticity.** Content addressing detects corruption and
  tampering, but does **not** verify *who* authored a grain. There is dormant
  scaffolding for COSE signing, but signature verification is not yet enforced
  on import. **Only sync with peers you trust.**
- ⚠️ **`verify` detects modification, not removal.** `areev verify` re-hashes
  every grain it can read, so an in-place edit of stored bytes is caught. But
  whole-file tampering that corrupts the WAL makes the storage engine roll the
  file back to its last consistent state — grains written since then silently
  vanish, and `verify` reports `ok` on the smaller, self-consistent survivor
  set. Truncation of a consistent store is indistinguishable from
  "never written" using the file alone; to detect it, compare against an
  **external anchor** — the op high-water mark of an `areev stream` segment
  directory, a bundle, or a hub replica.

## Input handling

- **CAL** (the query language) destroys only in **shaped** forms — by hash
  (`FORGET <hash>`), by identity (`FORGET SUBJECT "<id>"`), or by age
  (`PURGE OLDER THAN <n>d`) — **never by predicate**. Each is authorized by
  the session's `delete`/`erase` grant, requires a recorded BECAUSE (the
  bulk forms mandatorily), and writes a Tier-2 audit Observation naming a
  subject **fingerprint**, not the identity. The executor's
  `allow_destructive_ops` switch (default on; `--no-destructive-ops`) is a
  process-wide restrictive **cap** over any grant — use it for a read-only
  session, e.g. when serving untrusted input over MCP.
  `DELETE`/`ERASE`/`TRUNCATE`/… are not grammar tokens, `FORGET USER/SCOPE`
  are refused from text, and the server path requires the `admin` scope.
  `REPORT SUBJECT` — the read-only DSAR mirror — classifies as a read and is
  gated by `read`, deliberately not by the destructive cap. CAL is otherwise
  hardened against abuse (max query length, nesting depth, LET-binding and
  result-size caps, Unicode bidi-override rejection, NFC normalization).
- **Namespace prefix scopes** (`"org.*"`) widen **reads only**, and fail
  closed under a bound principal: the scope expands against the file's
  namespace registry and every covered namespace must be within the session's
  read grants, or the whole query refuses. The refusal names the *pattern the
  caller typed*, never a discovered namespace — in a multi-tenant file, a
  refusal that named a sibling tenant's namespace would itself be a
  disclosure. `*` is reserved: writes refuse wildcard namespaces, grants take
  exact names or `*` alone (never a prefix), destruction and policy
  (`FORGET SUBJECT`, `PURGE … IN`, retention/anon/holds) take exact
  namespaces, and a `namespace_override`-pinned session cannot escape its pin
  via `IN` sets or patterns (the pin clears any caller-supplied scope).
- **The console's namespace picker is a UI affordance over reads the bound
  session could already perform, not a new authorization surface.** `--ns`
  at launch is a display default, never an enforcement boundary (unlike
  `namespace_override`, above) — a bound session's `/api/cal` can already
  `RECALL` any namespace its `AuthzSet` covers. `GET /api/browse?ns=<name>`
  (an alternative to the bound default; omit `ns` for the original,
  unchanged behavior) and `GET /api/namespaces` (the picker's own namespace
  list) apply that exact same read gate explicitly, since neither goes
  through `AreevFacade::recall` — `changes_since`, `/api/browse`'s
  underlying read, has no per-namespace check of its own, so the route
  handler is the only enforcement point and has to check what `recall`
  would have. `?ns=*` ("no filter, show every namespace at once") is
  **owner-only**: a restricted principal must still enumerate namespaces
  one at a time, even one holding a wildcard `read ON *` grant that would
  pass every individual check — the firehose view is a stricter bar than
  the per-namespace one. `GET /api/namespaces` filters its list the same
  way, so a namespace's mere *name* is not disclosed to a principal with no
  read grant on it. Covered by `areev-server`'s `namespace_route_tests`.
- The store issues **parameterized SQL** exclusively; user strings are
  dictionary-encoded to integer term-ids before reaching the triple queries, so
  there is no SQL-injection surface.
- The **web console** escapes grain-controlled data before rendering it, so a
  synced grain carrying HTML/JS markup is inert in the UI.
- The console also treats grain-controlled data as untrusted on the way
  *back out* to CAL, not just on the way in to HTML: the Workflows editor's
  save path validates a plan's `BIND` hash against the content-address
  format before splicing it bare into the `ADD workflow` statement it sends.
  Every other value there (node names, `WHEN`, the trigger, the reason) goes
  through the same quoting `calEsc()` gives everything else; a hash is the
  one value CAL accepts unquoted, and `Workflow::bind()` accepts an
  arbitrary string, so a binding authored outside the console (the Rust,
  Python, or Node API, or a synced bundle) could otherwise append arbitrary
  CAL clauses to that statement the moment someone re-saves the plan through
  the UI.

## Threats in scope (please report)

- Memory-safety, panics, or resource exhaustion reachable from untrusted `.mg`
  blobs, bundles, or imported segments.
- Injection, path traversal, or auth bypass in CAL, the store, the MCP server,
  or the console/hub.
- Cryptographic weaknesses in the encryption or crypto-erasure paths.
- Secret or data leakage in error messages, logs, or `Debug` output.

## Host command seams (`--tool-cmd` and friends)

Six flags hand Areev a command to run: `--tool-cmd` (and `$AREEV_RUN_TOOL_CMD`
for the MCP server), `--embed-cmd`, `--anonymize-cmd`, `--llm-cmd`,
`--analyzer-cmd`, and `areev eval`'s case runner. Say the posture plainly:

> **A host command runs as you, with your privileges, on your machine.** Areev
> does not sandbox it. `--tool-cmd` and the eval runner go through the platform
> shell (`/bin/sh -c`, `cmd /C`); the other four are argv-split and never see a
> shell. A command you would not run at a prompt is a command you should not
> put behind these flags.

This is the same posture GitHub Actions takes with third-party actions, and it
is a deliberate v1 choice rather than an oversight: the alternative is a sandbox
that would still not stop a connector from misusing a credential it was
legitimately given.

What Areev *does* guarantee, as of 1.3:

- **Secrets you name are withheld.** `--passphrase-env VAR` and `--token-env
  VAR` pass a variable *name*, so Areev knows exactly which variables hold
  secrets and removes them from every child's environment. Before 1.3 the
  memory's encryption passphrase was inherited by every subprocess Areev
  started.
- **The rest of the environment is inherited.** Deliberately: an `--llm-cmd` or
  `--embed-cmd` that reads its own API key from the environment must keep
  working. Areev withholds what it *knows* is secret, not everything that might
  be. If a command must not see the ambient environment at all, do not rely on
  Areev for that — wrap it in `env -i` yourself.
- **A wall-clock ceiling.** 300s per invocation by default; the child is killed
  when it elapses. Note this kills the *direct* child: a shell command that
  spawned background grandchildren may leave them running, because killing a
  whole tree needs process groups.
- **An output cap.** 64 MiB per stream, drained past the cap so a capped child
  never blocks on a full pipe.
- **Tool names are validated.** A plan's `tool_name` reaches the child as
  `$AREEV_TOOL_NAME`, and a plan can arrive by bundle import — which verifies
  content integrity, not authorship. A name outside `[A-Za-z0-9_.-]{1,64}` is
  refused when the run starts.

Not provided: memory or CPU limits, filesystem confinement, network
restrictions, or privilege reduction. Do not put a command you do not trust
behind these flags.

### Tier C: sandboxed tools

`areev-sandbox` runs a `wasm32` module with no WASI, a frozen import set, a fuel
ceiling, a memory-page ceiling, and a module-size cap applied before the decoder
sees the bytes. A module cannot open a socket, touch the filesystem, read an
environment variable, see a clock, or run forever.

Be precise about what that buys. Tier C protects **the host from the tool**, and
it is real isolation for parsing, extraction, classification and scoring. It is
**not credential protection**: a connector that legitimately holds an OAuth token
and makes outbound calls is not made safer by isolation, which is why the egress
allowlist and the credential broker exist. The two mechanisms cover different
threats and neither substitutes for the other.

Since 1.6.0 the tier has **two runtimes**, because there are two determinism
stories (#101):

| Runtime | Import set | Determinism |
|---|---|---|
| `wasm32-areev` | `areev::emit` | pure — **re-execution-provable** |
| `wasm32-areev-io` | `+ areev::fetch` | deterministic **modulo journaled effects** |

`alloc` is a guest *export* the host calls, not an import, in both.

Until 1.6.0 the rule was absolute — *a Tier C module cannot make a network
call, so a connector will never be one* — and that was the reason the tier
half-delivered on its own promise. It is the only tier that produces a
persistable, content-addressed tool, and it forbade exactly the I/O every real
agent needs, so an I/O tool had to be a native blob (persisted, but *not
sandboxed — it runs as you*) or a host `--tool-cmd` script (sandboxed by
nothing, and outside the memory entirely).

**The isolation claim is strengthened by the second runtime, not weakened.**
The guest still has no socket, no credential, no clock, no environment. It gets
one more unforgeable capability — to *ask the host* — and the host enforces
policy and records everything. Three trust levels, credentials confined to the
innermost:

```text
guest wasm ──areev::fetch(req)──▶ sandbox binary (trusted Rust half)
                                       │ POST loopback, with AREEV_EGRESS_TOKEN
                                       ▼
                                engine broker (holds credentials)
                                  token→caller · host grant · DECLARATION ·
                                  allowlist · method · attach credential · perform
                                       ▼
                                  real upstream
```

The sandbox binary holds a revocable **broker token**, never a credential. This
needed no new IPC channel: the engine already injected `AREEV_EGRESS_URL` +
`AREEV_EGRESS_TOKEN` into that process for uniformity, inert only because the
*guest* could not reach them.

Four properties make it a capability system rather than a hole:

- **The gate is linked, not guarded.** `areev::fetch` exists in the guest's
  import set only when the host passed `--allow-fetch`, which the engine
  derives from the **manifest-pinned** runtime. A module that imports it
  without a capability declaration is refused at instantiation, by name,
  before one instruction runs — the same `ForbiddenImport` treatment WASI gets.
- **Effective reach is `declared ∩ host-granted`.** The Tool grain's
  `capabilities` field declares hosts, methods, path prefixes, credential
  names and request-header names; the host's `--allow-host` / `--credential` /
  `--tool-egress` grant independently. Both are checked on every call, so a
  declaration can only ever narrow. Default deny throughout: no declaration
  means no reach, no declared methods means read-only, no declared credentials
  means none, no declared headers means none.
- **The credential channel is not writable from the guest.** A module may set
  the non-credential headers enterprise APIs demand — `X-Goog-User-Project`,
  `anthropic-version`, a tenant id — and may not set `Authorization`,
  `Proxy-Authorization`, `Cookie`, `Host`, or any header a configured
  credential rides in, at any casing. Declaring one is refused at write time;
  sending one is refused before the call budget is spent, because that answer
  is identical for every caller and so leaks no policy. Values containing
  CR/LF are refused as malformed: header injection dies at the parse rather
  than at the socket. Caller-set headers travel exactly as far as the
  credential does — a cross-origin redirect drops both, since a quota project
  or tenant id was meant for the host the caller named, not for one an
  intermediary picked.
- **Path prefixes, which the host-side grant deliberately does not have.**
  `--allow-host` allowlists hosts because a path-level grant there would imply
  an authorization model it does not have. A capability tool is a different
  bargain — the code is pinned by content address — so it may pin
  `path_prefixes` too. That closes the exfiltration case a host-only grant
  structurally cannot express: a malicious tool POSTing stolen context to an
  *allowed* host's upload endpoint. The prefix match refuses evasive shapes
  outright rather than normalizing them — dot-segments (`/../`), percent-
  encoded dots and separators (`%2e`, `%2f`, `%5c`), and backslashes — because
  normalizing here would bet our resolution matches every upstream's. And the
  declaration binds **every redirect hop**, exactly as the allowlist does: a
  `302` on a declared host must not walk a module from its declared paths to
  an endpoint the host-side grant happens to tolerate.
- **Every call is journaled**, not just the refusals. See below.

Also out, permanently: guest-visible clock and RNG (that is the determinism
boundary), concurrency, streaming, raw sockets, and non-HTTP protocols. One
outstanding call at a time — completion-order nondeterminism would add an
ordering side channel to a boundary whose whole point is that it leaks nothing.

### What reaches a model

An `egress` anonymization policy now covers the run path as well as store
reads. Before, it did not: a trigger hands its payload into `run start` in
process, so an abstract node's prompt never passed a read exit, and the one
place a model was called was the one place the policy missed. An operator who
declared `egress` and ran abstract nodes had a reasonable belief that was
false, which is the worst shape a security control can take.

The boundary is the model, not the tool — a host tool must receive real values
or it writes corrupt records — and it is deliberately narrow:

- It is **not DLP**. The tool gets real data, so a compromised tool
  exfiltrates real data. Brokered egress is what constrains that, and only
  somewhat.
- It replaces **only what the detectors catch**: `email` and `phone` by
  pattern, `person` by interned known identities and the policy's
  `custom_terms`. A bare personal name the memory has never seen as a subject
  is not pseudonymized.
- Rehydration **fails closed** — an unresolvable placeholder fails the node
  rather than sending the placeholder to a vendor — but `unmatched` detection
  recognizes the default `[CATEGORY_ID]` silhouette, so a custom `placeholder`
  template weakens that check.
- It requires `scope: memory`, and therefore an encrypted memory, because only
  value-derived tokens replay identically (`RUN-E023` refuses the rest at
  start).

### Credentials a host command never holds

`areev run`'s `--credential` / `--allow-host` / `--tool-egress` and the trigger
evaluator's connector path share one credential broker. A brokered command gets
`AREEV_EGRESS_URL` and `AREEV_EGRESS_TOKEN` and nothing else; credential values
are read from host-named environment variables in the driver's process and
never enter a grain, a bundle, or the child's environment.

Six properties are worth stating because each closes a specific hole:

- **The broker authenticates its callers.** It binds loopback on an ephemeral
  port, and loopback is not an authorization — any process on the box could
  otherwise post to it and spend the credentials it holds. Each caller
  presents an unguessable per-caller capability token.
- **Scope is per caller.** The token is also what lets one port serve N pool
  workers and still tell them apart, so one tool's grant buys nothing of
  another's. A caller with no grant never receives the broker's address.
- **Writes are deny-by-default.** A grant naming no method may only `GET`/
  `HEAD`.
- **The credential variable is withheld from children** (#100, fixed in
  1.6.0). Reading `--credential NAME=ENV_VAR` is what registers `ENV_VAR` as a
  secret, so every subprocess seam scrubs it. Before 1.6.0 the withhold list
  was three flags long and `--credential` was not on it: an operator who
  exported `ZOHO_TOKEN` and left it exported handed the **raw credential** to
  every tool, connector and sandbox subprocess, which could read it out of
  `/proc/self/environ` and never call the broker at all. Registration happens
  where the variable is *read* rather than at each host's flag parsing,
  because four hosts read credentials this way (`areev run`, `areev trigger
  run`, and the Python and Node bindings) and a fix at one site would have
  left three open. The sandbox seam additionally spawns under
  `EnvPolicy::ClearExcept` — a wasm host has no claim on the operator's
  environment, and under #101 it is also the process holding a broker token.
- **The allowlist governs every hop, not just the first** (#99, fixed in
  1.6.0). The HTTP client used to follow up to ten redirects on its own while
  the allowlist was checked once, on the caller-supplied URL, before dispatch
  — so an allowed host answering `302 Location: http://169.254.169.254/…` had
  its follow-up performed and the cloud metadata service's body handed back to
  the tool. Auto-follow is now off and each hop is re-authorized: **no byte is
  sent to, and no body is returned from, a host the allowlist does not
  permit.** A blocked redirect journals a refusal (`RUN-E022` / `TRG-E009`)
  worded apart from an aimed-at one, because "it tried to reach there" and "it
  was redirected there" are different stories. The mirror image is fixed too:
  the brokered `Authorization` used to be dropped on *every* redirect
  including a same-origin one, so legitimate Google/Microsoft flows 401'd
  silently; it now rides a follow exactly when scheme+host+port are unchanged
  **and the chain has never left the starting origin** — an `A → B → A` bounce
  through another origin retires the credential for good, so an untrusted
  intermediary cannot have it re-attached to a path it chose (the rule browsers
  and `curl --location` apply). The success audit records the credential name
  only when it actually rode the final request. Chains are bounded at ten hops
  and the bound is auditable.
- **A reflected credential is scrubbed.** Response *headers* never cross the
  broker — it answers with `{status, body}` and nothing else — so the body is
  the only channel, and an echo or verbose-error endpoint that bounces the
  injected `Authorization` back in it has the value replaced before it reaches
  the caller or the audit trail.
- **The response ceiling bounds the read, not just the answer.** For a
  capability caller the broker abandons the body at `max_response_bytes`
  rather than buffering an upstream's whole answer and then measuring it, and
  the overrun is a typed refusal — never a truncated or empty body passed off
  as the upstream's response.
- **`areev::fetch` is non-reentrant, enforced.** Placing a response calls the
  guest's own `alloc`, which is guest code; a guest whose allocator called
  `fetch` again would recurse a native host frame plus a broker round trip per
  level. An in-flight flag refuses the reentrant call with `-1` before any
  broker traffic — "one outstanding call, synchronous" is a mechanism, not a
  convention.
- **A capability declaration cannot reach private space by itself.** Under an
  unrestricted egress policy a capability tool is still refused loopback,
  link-local, private-range and cloud-metadata destinations — a synced memory
  declares hosts freely, so reaching the local console, the hub, or the
  metadata service takes an explicit `--allow-host` entry. This binds every
  redirect hop, and leaves connectors and `--tool-cmd` tools (pure host config)
  untouched. The check canonicalizes the alternate IPv4 literal encodings a
  libc resolver honours — decimal (`http://2852039166/` is the metadata
  service), hex, octal and the short `127.1` forms — so classifying only the
  dotted-quad spelling cannot be used to walk past it. It stays syntactic in one
  respect only: a public hostname *resolving* to a private address (DNS
  rebinding) is the standing limitation of hostname allowlisting.
- **Credentials can be bound to a run principal.** `--credential
  name=VAR@principal` refuses the credential to any run executing as a
  different principal, and to one that bound none (fail-closed) — so a single
  process holding several users' secrets cannot let a run started for one spend
  another's. The tool grant governs which *tools* may ask; ownership governs
  which *runs* may be answered. This is the RBAC unit for the case where
  grain-stored code invoked on behalf of one user must not gain another's
  access — alongside the wasm guest having no filesystem, no store handle, and
  no environment to reach another user's data with in the first place.

Grants are host configuration, never grains, for the same reason the
code-executor allowlist is: a Definition declaring its own reach would be a
permission arriving in the same bundle as the code it authorizes. A capability
tool's `capabilities` field (#101) is not a counter-example — it *declares*,
and the effective set is its intersection with the host grant, so it can only
narrow.

### The egress audit trail

Both halves of a run's outbound behaviour land in the memory as Tier-2
Observations in `agent:harness`, written at each superstep boundary so a crash
loses at most one superstep of evidence:

| `observation_kind` | Records | Dedup |
|---|---|---|
| `egress_refusal` | caller, destination, reason | per distinct `(caller, destination, reason)` |
| `egress_call` | caller, method, final URL, status, redirect count, request/response **digests**, response size, credential **name**, caller-set request headers **with values** | none |

The dedup difference is deliberate: a refusal is a policy fact and forty
retries against one blocked host are one of them, but a successful call is an
*effect* and forty of those are forty things that happened.

Bodies are recorded as `sha256:` digests, never contents — a grain is immutable
and replicates, so an inbox body written into one cannot be taken back, and the
digest is what pins *which* request anyway. The credential appears by name
only, which is all the broker ever received: the caller sends a label and the
value is attached internally, so the record is safe by construction rather than
by scrubbing. Caller-set headers are the deliberate asymmetry: they are
recorded with their **values**, because the caller supplied them, so the record
discloses nothing the caller did not already hold — and "it billed this quota
project on these four requests" is evidence "it reached Google" is not.

Neither kind is a journal entry. Replay never sees them, so `verify` stays
byte-identical whether or not a broker was configured — they are evidence
*about* the run, not steps *of* it.

**Refusals are evidence, so they are journaled.** Each distinct `(caller,
destination, reason)` a run is refused lands as an Observation in
`agent:harness` alongside the stderr line. A refusal is an agent reaching for
somewhere it was not allowed — the event a reviewer asks about — and evidence
that lives only in a terminal is evidence until the terminal scrolls.

The limits are unchanged and worth repeating: exfiltration through an *allowed*
host still works, hostname allowlisting cannot see through DNS tricks or domain
fronting, and a brokered command cannot use a vendor SDK. This is why the
brokering exists alongside — not instead of — the sandbox discussion above: a
connector legitimately needs the network *and* the credential, so isolation
does not constrain what actually goes wrong.

### Code that arrives in a memory (`executor_uri`)

A `Tool` Definition may name its executor by content address
(`executor_uri: "cas://sha256:..."`), and the blob travels in bundles — so
**importing a peer's memory imports their connector code**. Say this posture
plainly too:

> **Areev never executes a code blob the host did not pin.** The default is
> refuse, in the `HostToolExecutor` trait itself.

The authorization deliberately does not live in the file. An operator pins
addresses with `areev run start --allow-executor <addr>`, which is host
configuration, never a grain. There is no CAL grant form for this and there
should not be: `mg:permits` Facts replicate, and a permission that arrives in
the same bundle as the code it authorizes is not a permission. This is the same
split that keeps trigger evaluation state and host config out of the file.

An unpinned address is refused at run start (`RUN-E018`) before a lease is
taken, naming the address. An `executor_uri` that is not a `cas://sha256:`
content address, or one on a `client` tool, is refused at resolve — a value
that is silently ignored is exactly the failure this refuses.

What the pin buys, and what it does not: a pinned executor **runs as you, with
your privileges**, exactly like `--tool-cmd`. The pin is a judgement about
*provenance* — that a human looked at this specific content address — not a
container. The threat it addresses is the one that actually occurred in the
January 2026 n8n community-node compromise, where code nobody had vetted ran
with credentials it was given; isolation did not stop that and would not stop
it here.

Because the path is `<cache>/<hex>` and `get_blob` verifies the digest on every
read, a poisoned cache entry cannot impersonate a different executor.

## Release artifacts: provenance and bill of materials

Every published release carries a **build provenance attestation** and a
**CycloneDX SBOM** (#81). The problem they address is the one behind every
registry compromise of the last few years: nothing about a package on
crates.io, PyPI or npm otherwise tells you it came from this repository's CI
rather than a maintainer's laptop or a stolen publish token.

Attestations are Sigstore-backed and keyless, so there is no signing key for
this project to lose — the identity being attested is the workflow, in this
repository, at that commit.

Verify a downloaded CLI archive:

```bash
gh attestation verify areev-1.5.2-aarch64-apple-darwin.tar.gz --repo AreevAI/areev
```

Verify an installed npm package (npm's own registry-visible provenance, from
`npm publish --provenance`):

```bash
npm audit signatures
```

Verify a wheel:

```bash
gh attestation verify areev-1.5.2-cp39-abi3-macosx_11_0_arm64.whl --repo AreevAI/areev
```

The SBOMs are attached to the GitHub Release as `*.cdx.json`. There is one per
ecosystem, and for the two binding packages there are **two** graphs, because
the interesting dependencies are Cargo's — a wheel or a `.node` addon links a
Rust tree that `pip`/`npm` cannot see, so an SBOM built only from the
package-manager metadata would truthfully describe almost nothing.

| Asset | What it describes |
|---|---|
| `areev-<v>-cargo.cdx.json` | the `areev` binary's Rust dependency graph |
| `areev-<v>-python.cdx.json` | the Rust graph compiled into the wheel |
| `areev-<v>-node-cargo.cdx.json` | the Rust graph compiled into the `.node` addon |
| `areev-<v>-npm.cdx.json` | the addon package's JS tree |

This complements, and does not replace, `cargo-deny` (advisories, licenses,
sources, bans) in `security.yml`: `cargo-deny` says the dependencies are
acceptable, the SBOM says which ones shipped, and the attestation says who
built them. Note this is unrelated to grain signing (#77) — that is about
authenticating *data* at the format level, which remains out of scope below.

## Threats out of scope

- An already-compromised host, physical access, or a malicious local process
  running with the same privileges as Areev.
- Whether a memory stores a *specific known* attachment: blob filenames are
  plaintext content addresses (see the sidecar note above).
- Network confidentiality without an operator-provided TLS proxy (by design).
- Forged grain provenance when syncing with an untrusted peer (integrity is
  guaranteed; authenticity is not, until signing lands).

## Areev Loop (self-improvement) trust boundary

Areev Loop lets an agent change its own memory, so its governance *is* a security
boundary. See [`loop.md`](loop.md) for the surfaces; the invariants:

- **Read-only token-less console (breaking change).** Token-less `areev ui` is
  read-only. Every write — any loop mutation, or an `ADD`/`SUPERSEDE`/
  `FORGET` CAL batch — returns 401 without `--token-env VAR`. This closes the
  bypass where a local process could execute a proposal's CAL directly and
  skip the review queue, which would void the whole governance story. The
  server classifies a POST `/api/cal` by its leading keyword and fails closed.
- **The trust floor is not configurable.** These fields do not exist in any
  file or policy schema (unknown keys are rejected at load), so a hostile or
  synced file can never arrive pre-armed: auto-apply never touches free text,
  destruction, prompts, or LLM-drafted content; analyzers execute read-only;
  no payload amplifies scopes; no file raises a host-set cap.
- **Substrate reads are namespace-grant-gated.** A principal holding
  `loop.run` reads only the namespaces its grants cover: explicit-namespace
  reads fail closed rather than returning empty, all-namespace scans filter
  per grain, and `agent:*`/loop namespaces are excluded from implicit
  analyzer input entirely (governance and harness state are not analyzer
  fodder). Regression-tested in `areev-loop-adapter/tests/adapter.rs`.
- **The laundering threat.** The deterministic path can carry attacker text:
  tool-failure clustering derives a signature from attacker-controlled tool
  output. So auto-apply is restricted to SUPERSEDE-only structural curation
  with **zero** attacker-influenced free text (an `ADD` disqualifies), and any
  recommendation introducing evidence-derived text is always approval-required
  with the untrusted prose shown as a literal, escaped diff.
- **Auto-apply is default-off and host-granted only.** It requires host opt-in
  plus a matching grant in the optional `loop-policy.json`, a built-in
  analyzer, a memory/query target, non-destructive payload, and an engine-side
  per-draft shape check. The policy file is host config, never persisted in a
  memory file, and rejects unknown keys — a stolen or committed policy file is
  inert (it cannot register an executable).
- **Separation of duties + accountable audit.** `write` grants neither
  `review` nor `apply`; self-approval is blocked against the creating actor;
  every transition writes an immutable, hash-chained audit Observation with a
  mandatory reason. Audit grains live and die with the file they govern —
  erasing a subject's file erases its audit (correct GDPR-shaped behavior).

## Roadmap

- Enforced grain signing / authenticity verification on import (COSE) —
  tracked in [#77](https://github.com/AreevAI/areev/issues/77).

If you find something that contradicts this document, that is itself worth
reporting — see [SECURITY.md](../SECURITY.md).
