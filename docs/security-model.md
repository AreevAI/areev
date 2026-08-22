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

### Tier C: sandboxed pure-compute tools

`areev-sandbox` runs a pure `wasm32` module with no WASI, a frozen one-function
import set (`areev::emit` — `alloc` is a guest export the host calls), a fuel ceiling, a memory-page ceiling, and a module-size cap applied
before the decoder sees the bytes. A module cannot open a socket, touch the
filesystem, read an environment variable, see a clock, or run forever.

Be precise about what that buys. Tier C protects **the host from the tool**, and
it is real isolation for parsing, extraction, classification and scoring. It is
**not credential protection**: a connector that legitimately holds an OAuth token
and makes outbound calls is not made safer by isolation, which is why the egress
allowlist and the credential broker exist. By Tier C's own design a module cannot
make a network call, so a connector will never be one — the two mechanisms cover
different threats and neither substitutes for the other.

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

Three properties are worth stating because each closes a specific hole:

- **The broker authenticates its callers.** It binds loopback on an ephemeral
  port, and loopback is not an authorization — any process on the box could
  otherwise post to it and spend the credentials it holds. Each caller
  presents an unguessable per-caller capability token.
- **Scope is per caller.** The token is also what lets one port serve N pool
  workers and still tell them apart, so one tool's grant buys nothing of
  another's. A caller with no grant never receives the broker's address.
- **Writes are deny-by-default.** A grant naming no method may only `GET`/
  `HEAD`.

Grants are host configuration, never grains, for the same reason the
code-executor allowlist is: a Definition declaring its own reach would be a
permission arriving in the same bundle as the code it authorizes.

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
