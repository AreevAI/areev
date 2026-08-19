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

- **Three subkeys derive from the page-cipher key** via HKDF with distinct
  domain-separation strings: `areev.blobs.v1` (the CAS sidecar, above),
  `areev.anon.memory.v1` (value-derived pseudonym tokens — ingress mode and
  `memory` scope), and `areev.vault.v1` (the sealed placeholder→value vault:
  AES-256-GCM per row, the `vault:<ns>:<placeholder>` row key bound in as
  associated data). Destroying the page key destroys all three —
  **crypto-erasure reaches the vault by construction**.
- **No page key, no value-derived features.** On a plaintext file — or any
  Postgres schema, where the page cipher does not exist — ingress modes,
  `memory` scope, and the vault refuse loudly at policy `set` rather than
  degrade to unkeyed derivation (conformance-pinned on both backends). The
  egress session's `mapping_id` key falls back to a per-handle random key
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

- ⚠️ **No TLS.** All HTTP is plaintext. For any non-loopback deployment, front
  the console/hub with a **TLS-terminating reverse proxy**. Both `areev ui` and
  `areev hub` refuse to bind a non-loopback address unless you pass
  `--allow-remote` (and even then warn loudly). `--token-env` authentication
  is **not** a substitute for TLS: the token and all memory still cross the
  wire in the clear, so `--token-env` guards against unauthorized clients but
  not against a network eavesdropper — use it *with* a TLS proxy off-loopback.
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

`areev-sandbox` runs a pure `wasm32` module with no WASI, a frozen two-function
import set, a fuel ceiling, a memory-page ceiling, and a module-size cap applied
before the decoder sees the bytes. A module cannot open a socket, touch the
filesystem, read an environment variable, see a clock, or run forever.

Be precise about what that buys. Tier C protects **the host from the tool**, and
it is real isolation for parsing, extraction, classification and scoring. It is
**not credential protection**: a connector that legitimately holds an OAuth token
and makes outbound calls is not made safer by isolation, which is why the egress
allowlist and the credential broker exist. By Tier C's own design a module cannot
make a network call, so a connector will never be one — the two mechanisms cover
different threats and neither substitutes for the other.

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

- Enforced grain signing / authenticity verification on import (COSE).
- First-class TLS for the hub.

If you find something that contradicts this document, that is itself worth
reporting — see [SECURITY.md](../SECURITY.md).
