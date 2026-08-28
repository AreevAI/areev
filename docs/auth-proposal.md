# Identity for the one networked surface — an auth proposal

**Status:** **implemented** 2026-08-27 (A0–A3, same day it was written).
Supersedes
[`areev-enterprise-proposal.md`](areev-enterprise-proposal.md) §3.3 (E3, "SSO
— trusted-header first, OIDC second"), whose v0 has since shipped and whose
v1 this document re-scopes and re-orders. Nothing here touches `areev-core`,
the `.mg` format, the CAL grammar, or the engine — everything is host-plane,
per [`areev-enterprise-proposal.md`](areev-enterprise-proposal.md) §4.

---

## What shipped, and what changed on contact

All four phases landed. Three decisions moved between proposal and
implementation, each because building it surfaced something the design did
not:

1. **The credential `id` is optional, not required (§5.1).** As proposed it
   would have refused every `areev-auth.json` written before it existed —
   breaking running consoles on upgrade. A console that will not start gets
   rolled back, which is a worse security outcome than an ugly default. It now
   falls back to a stable, **non-positional** derived id (a digest prefix, or
   `env:VAR`); positional ids (`token-0`) were rejected because an index
   shifts when an unrelated line is added, and `revoke --id token-1` would
   eventually revoke the wrong credential.
2. **A group-derived principal may never approve (§7.2).** The proposal
   treated groups as pure convenience. Implementing it made the consequence
   obvious: binding a role as the request principal means an approval's audit
   record reads `role:engineering approved` — naming nobody who can be asked
   why. This refusal deliberately has *no* flag, unlike `--sso-approvals`.
3. **Algorithm confusion had to be closed explicitly (§6.1).** The first
   implementation built its JWT validation from the token header's own `alg`
   — the classic footgun, where `alg: HS256` signed with the issuer's public
   key verifies anywhere the header is trusted. Symmetric algorithms are now
   refused before a key is selected, and the allowlist fails closed on any
   algorithm a future library version adds.

The dependency count in §10 was also understated: the `oidc` feature pulls
`jsonwebtoken` **plus** `ureq`, `serde`, `sha2`, `hex` and `getrandom` — all
of which were already in the tree, so it adds one new *package* but six new
edges for `areev-server`. A0–A2 added none.

One gate is honestly weaker than §9 claims: **"round-trip against a local
IdP" is not in CI.** Signature verification is delegated to a vetted library
precisely so it is not this crate's to re-implement — or to mock — and faking
it would test the mock. What *is* tested is everything this crate owns:
PKCE against RFC 7636's vector, the algorithm allowlist, single-use login
state, nonce comparison, session expiry on both clocks, digest-only session
storage, exact cookie matching, and the cookie's flags. End-to-end against a
real provider is a deployment step, in `runbooks/oidc-setup.md` §5.

---

## 0. The question, answered up front

Four mechanisms were asked about. Three of the four answers are "no", and the
one "yes" is not the one that looks most urgent.

| Ask | Verdict | Why |
|---|---|---|
| **Google SSO** as an integration | **No — reframe** | Google is an OIDC provider. A Google-specific code path buys nothing a generic OIDC client doesn't, and costs a permanent second code path. Ship one OIDC client; Google becomes *config*. §3 |
| **Microsoft SSO** as an integration | **No — reframe** | Same. Entra ID is an OIDC provider. §3 |
| **OAuth client-based auth** (client credentials, M2M) | **No** | Machines already have the right primitive. `client_credentials` buys a shorter-lived bearer at the cost of a token endpoint, a JWT validator and clock-skew handling. The MCP spec itself tells stdio servers not to do this. §4 |
| **Token-based auth** | **Yes — and it is P0** | It already exists and is the weakest part of the stack. Five concrete gaps, all fixable with zero new dependencies. §5 |
| *(not asked, but the actual finding)* **Native OIDC** | **Yes, P2, behind a non-default feature** | Not for login convenience — for the one control the proxy pattern structurally cannot secure: HITL approval identity. §6 |

**The single highest-security-value change in this document costs nothing and
ships this week:** a proxy-asserted SSO identity should not be allowed to
answer a HITL approval by default (§7.4).

---

## 1. What we already have (inventory before opinion)

Any proposal that starts "Areev has basic auth with a token from the env" is
working from a two-release-old picture. The current state:

| Mechanism | Flag | What it proves | Where |
|---|---|---|---|
| Loopback, unauthenticated | `areev ui` | Nothing — you are on the box | `lib.rs:239` |
| Shared secret | `--token-env VAR` | Possession of one console-wide secret; implies owner | `lib.rs:401` |
| Per-principal credential map | `--auth FILE` | Possession of a token bound to a named principal, optionally scoped per memory. SHA-256 or env-indirected, constant-time compare, constant-shape scan | `authz.rs:383` |
| Trusted-header SSO (v0) | `--sso-header` + `--sso-secret-env` | An authenticating proxy did OIDC/SAML and vouches. Two-secret rotation window | `lib.rs:266` |
| Native TLS | `--tls-cert/--tls-key`, `tls` feature | Transport | `lib.rs:134` |

Supporting controls that are already correct and must not regress: DNS-rebinding
`Host` check on every method, Origin allowlist on POST with no wildcards, 1 MiB
body cap, header count/byte caps, per-IP auth-failure counting with a bounded
map, DSN redaction at every display surface, and `Drop`-restored per-request
principal binding that survives a panic.

**Two facts frame everything below:**

1. **There is exactly one networked surface.** `areev ui`. The CLI is a
   process, MCP is stdio, the bindings are in-process, `areev run` is a
   library, and `areev hub` was deliberately removed on 2026-08-24
   (ARCHITECTURE.md §10, "Sync is file-to-file"). This is not a
   "does our product support SSO" question. It is "how does one loopback-first
   console over one memory file authenticate a human."
2. **Authentication is already fully separated from authorization.** Identity
   is host config, never in the file (invariant 5). Rights are `mg:permits`
   grains *in* the file. `AreevFacade::bind_principal`
   (`areev-cal/src/areev_facade.rs:217`) always constructs
   `AuthzSet::restricted(principal, grants)` — verified — so a new identity
   source can never escalate. It can only *select among grants the file
   already made*, and a store failure reading grants falls to anonymous.

Point 2 is the load-bearing architectural property of this whole proposal:
**a new authn mechanism only has to produce a trustworthy principal string.**
Nothing downstream changes. That is why adding an IdP is cheap in the
authorization dimension and expensive only in the protocol dimension — and it
is why the protocol dimension is where all the scrutiny belongs.

---

## 2. What the open-source world actually converged on

Three patterns, and the industry has split them by *who* is authenticating.

**Humans → delegate to an authenticating proxy.** This is the dominant
self-hosted pattern: oauth2-proxy, Pomerium, Authelia, Authentik, Cloudflare
Access, or IAP terminates OIDC/SAML and forwards identity in headers. Grafana
ships this as a first-class mode (`auth.proxy`) *alongside* its native OAuth,
and Prometheus's own security model is explicit that anything past basic auth
belongs in a reverse proxy. The decision point is
[not the feature matrix but where the identity check happens][fa] — in the
proxy or in the app.

**Machines → long-lived, hashed, prefixed, revocable tokens.** GitHub's PAT
design is the reference: a recognizable prefix (`ghp_`, `ghu_`) so secret
scanners and humans can spot one, hashed at rest so a database dump is inert,
optional expiry with rotation policies, and fine-grained scopes so a leak is
bounded. Notably, Prometheus stores its basic-auth passwords **bcrypt**-hashed,
not SHA-256 — because operators choose those values, and operator-chosen
secrets are crackable offline. That distinction matters here (§5.2).

**When an app does do OAuth itself, the bar moved.** RFC 9700 (OAuth 2.0
Security BCP) makes PKCE mandatory for *all* authorization-code clients
including confidential ones, requires exact redirect-URI matching, and formally
deprecates the implicit and password grants. For anything browser-facing, the
browser-apps BCP (still an I-D, draft-26) recommends the BFF shape: the backend
is the confidential client, tokens stay server-side, and the browser gets only
an `HttpOnly` `SameSite=Strict` session cookie — so XSS cannot exfiltrate a
token.

**And the counter-current worth naming:** building SSO into an app is a
permanent maintenance surface, not a feature you finish. The consistent advice
for small projects is to delegate rather than own it — the median
time-to-patch for critical vulns in community-maintained auth code is
materially worse than in commercial equivalents. A dependency-light project
with one networked surface should be extremely reluctant to become an OAuth
client, and should never become an OAuth *server*.

That reluctance is the default position of this document. §6 argues the one
case that overcomes it.

---

## 3. Rejected: per-vendor Google and Microsoft SSO

Google Workspace and Microsoft Entra ID are both OIDC providers with RFC 8414
discovery documents. "Google SSO" and "Microsoft SSO" as *separate
integrations* would mean two client registrations, two claim-mapping code
paths, two test matrices, two sets of breakage when either vendor changes a
consent screen — in exchange for zero capability a single generic OIDC client
doesn't already have.

**The correct form of the ask:** one OIDC client with discovery. Then Google is:

```
--oidc-issuer https://accounts.google.com --oidc-client-id ... --oidc-client-secret-env ...
```

and Entra is the same three flags with a different issuer URL. Vendor support
becomes documentation and a smoke test, not a subsystem.

**Corollary, and it is a hard rule:** no vendor names in the codebase, ever.
The moment `google.rs` exists, the second one is inevitable, and then the
per-vendor claim quirks leak into the identity model. If a provider needs
special handling, that is a config knob (`--oidc-principal-claim`), not a
module.

---

## 4. Rejected: OAuth client-credentials for machines

The `client_credentials` grant would let an agent exchange a client
ID/secret for a short-lived access token. Against Areev's shape it is a
downgrade:

- **The primitive already exists and is better-fitted.** The credential map is
  memory-scoped, constant-time, constant-shape (an out-of-scope token is not
  even timing-distinguishable from an unknown one), stores no raw secret, and
  resolves to a principal the file's own grants govern. `client_credentials`
  ends at the same place — a bearer token in an `Authorization` header — after
  a network round trip Areev would have to make, to an authorization server
  Areev would have to require.
- **The MCP spec explicitly says not to.** Implementations using stdio
  transport SHOULD NOT follow the MCP authorization spec and should retrieve
  credentials from the environment instead. `areev-mcp` is stdio-only. If an
  HTTP MCP transport is ever added, *that* is the moment to revisit — as an
  OAuth **resource server** (RFC 9728 protected-resource metadata), never as
  an authorization server.
- **Areev is not a multi-tenant API.** The isolation unit is a memory file
  (invariant 5). Tenancy is one memory per tenant. The problems
  `client_credentials` solves — central issuance across many resource servers,
  short token lifetimes, revocation at an AS — are problems of a fleet of APIs
  behind one identity plane. Areev is one process holding one file.

**What to do instead:** §5 makes the existing token model as good as a
well-designed PAT system, which is the actual industry answer for M2M in
self-hosted tools.

---

## 5. P0 — the token model we already have

Five gaps, each verified in the current code. None needs a dependency.

### 5.1 A credential has no identity

`CredentialEntry` is `{sha256|env, principal, memories?}`
(`authz.rs:391`). There is no per-credential id or label. Consequences: two
tokens for the same principal are indistinguishable, so revoking one means
knowing which line is which by hand; an auth-failure log line can name a source
IP but never a credential; and there is no way to say "the CI token leaked"
without rotating everything that principal holds.

**Add** a required `id` (operator-chosen, e.g. `ci-runner-2026q3`) and an
optional `label`. The id appears in auth logs on *success* and in
`GET /api/whoami`; it never appears in a failure message (a refused secret
must not confirm which credential it nearly matched — the existing discipline
in `resolve_for_memory` is right and must extend to this).

### 5.2 Operator-chosen tokens are hashed with a fast, unsalted digest

`resolve` computes `hex(Sha256(presented))` and compares against the stored
digest. For a 256-bit random token that is correct and salting would add
nothing. **But nothing mints tokens today** — the operator brings their own
value, for both `--token-env` and the map. An operator who picks a memorable
passphrase has put an offline-crackable digest into a file the docs encourage
sharing across server instances. This is exactly why Prometheus bcrypts.

**Add** `areev auth mint` — emits a 256-bit CSPRNG token, prints it once,
prints the JSON entry to paste. Plus a load-time entropy floor: refuse a
`--token-env` value or `sha256` entry that came from a token shorter than N
bytes where the length is knowable, and always warn in the startup banner when
a token was not minted by Areev. Do **not** add bcrypt/argon2 — the right fix
is to remove operator-chosen secrets from the path, not to make weak ones
slower to crack.

### 5.3 Tokens are recognizable to no one

**Add** a prefix on minted tokens: `areev_pat_<base32>`. Cost: a constant.
Benefit: GitHub/GitLab secret scanning and every commercial scanner can be
taught one regex; a human sees a token in a paste and knows what it opens; and
Areev can reject an obviously-malformed credential before the constant-time
scan. This is free and it is the single best return per line in the document.

### 5.4 There is no expiry

No `expires_at` anywhere. A credential map entry is valid until an operator
edits the file. **Add** optional `expires_at` (RFC 3339, UTC), checked inside
the existing constant-shape scan so an expired credential is
indistinguishable from an unknown one. Optional, not mandatory — a homelab
console should not break at 3am because a default 90-day clock ran out — but
`areev auth mint --expires 90d` should be the documented path and the startup
banner should name credentials expiring within 14 days.

### 5.5 The shared secret is still the documented on-ramp for writes

The startup error today reads: *"restart areev ui with `--token-env VAR` to
enable writes"*. That points every operator at the one mechanism that cannot
be attributed — `--token-env` implies owner, and by design resolves
`request_principal` to `None` so its holder cannot approve. Every audit grain
it produces says `user:console`.

**Change the guidance, not the mechanism.** `--token-env` stays (it is the
right thing for a single-user loopback console and removing it would break
people). But: the error text should point at `--auth` first; the startup
banner should say plainly that shared-token writes are unattributable; and
`docs/cookbook.md` should show `--auth` as the default recipe with
`--token-env` as the single-user shortcut.

### 5.6 Failure counting does not fail closed

`note_auth_failure` counts and logs but never delays or rejects, with a
documented and *correct* reason: `serve` is a strictly serial accept loop, so
a per-request sleep is a lever an unauthenticated caller pulls to stall the
console for everyone. That reasoning rules out *delay*. It does not rule out
*rejection*: after N consecutive failures from one IP within a window, return
`429` immediately — no sleep, no store access, no constant-time scan. That
costs an attacker their pipeline and costs a legitimate operator a documented
cooldown. Keep the bounded map; keep the "never log the token" rule.

---

## 6. P2 — native OIDC, and the only argument that justifies it

The proxy pattern (§2) covers humans well, ships today, and needs no
dependency. The case for taking on an OIDC client anyway rests on one thing,
and it is specific to Areev rather than general.

**Areev's strongest governance control is backed by its weakest identity
primitive.**

`POST /api/run/respond` enforces separation of duties: shared-token and
anonymous callers are refused outright, because "the approver's identity IS
the audit record" (`lib.rs:1321`). Only a resolved `request_principal` may
approve. Correct — and the SSO v0 path sets `request_principal` from a
**header**, trusted because the request also carried
`x-areev-proxy-secret` (`lib.rs:888`).

The code is admirably honest about what that secret is:

> It is an impersonation-grade credential: whoever holds it can present any
> identity header, approval-capable principals included.

So the chain is: whoever holds one shared static secret can assert *any*
identity, including the compliance officer's, and produce an audit record that
names that officer approving a spend. Nothing in the file can detect it,
because from the store's side it is a well-formed approval by a granted
principal. The blast radius of that secret is the entire integrity of the HITL
audit trail — which is the feature the governance story is sold on.

A proxy cannot fix this. Header-forwarded identity is *by construction* an
assertion whose only proof is a bearer secret shared between two processes.
Native OIDC replaces "the proxy says so" with "the IdP signed this, and we
verified the signature against its published JWKS" — an assertion that is
bound to a subject, an audience, a nonce and a clock, and that a leaked static
secret cannot manufacture.

That is the argument. It is not "logging in is nicer." If the HITL approval
gate did not exist, this section would say "use the proxy, we're done."

### 6.1 Scope, hard-bounded

- **Authorization code + PKCE**, per RFC 9700 — PKCE on a confidential client
  too. Exact redirect-URI matching. No implicit grant. No password grant. No
  hybrid flows.
- **BFF-shaped by construction, and nearly free here.** The console is one
  server-rendered HTML file with no build step and no SPA token store. The
  server holds the tokens; the browser gets an `HttpOnly`, `Secure`,
  `SameSite=Strict` session cookie and nothing else. This is what the
  browser-apps BCP recommends, and Areev's existing architecture arrives there
  by accident rather than effort.
- **Discovery via RFC 8414 metadata.** Google and Entra are then config (§3).
- **`id_token` validation is the whole security surface** — issuer, audience,
  `exp`/`nbf` with bounded skew, nonce, `azp`, and signature against a cached,
  refreshed JWKS. **This must be a vetted crate, not hand-rolled.** It is the
  second recorded dependency exception, same posture as rustls: non-default
  cargo feature (`oidc`), one paragraph in ARCHITECTURE.md §10.
- **Machines keep tokens.** SSO is for humans in the console. There is no
  OIDC path for `areev-mcp`, the CLI, or the bindings.
- **Areev never becomes an authorization server.** No user database, no
  password storage, no MFA, no consent screen, no token issuance to third
  parties. Identity lives in the IdP.

### 6.2 The honest cost, stated plainly

**Sessions are the first server-side state Areev has ever held.** Today
`areev-server` is one-request-per-connection with no state between requests —
that is a large part of why its security surface is small enough to reason
about. A session store introduces: session fixation, idle vs absolute
timeouts, logout that actually invalidates, rotation on privilege change, and
a bounded map that is attacker-influenced key space (the same class of problem
`auth_failures` already had to solve).

This is the real price, and it is bigger than the JWT dependency. It is also
why this is P2 and gated on §7 landing first: if the interim hardening removes
the impersonation risk from the approval path, the urgency of §6 drops and it
can be scheduled honestly rather than under pressure.

---

## 7. P1 — interim hardening for SSO v0 (no dependencies, ships now)

Everything here is cheap and closes most of §6's gap without the session
machinery.

### 7.1 Constrain the asserted identity

`sso_identity_raw` is taken from a header and `trim()`ed, then used verbatim as
a principal (`lib.rs:642`, `lib.rs:890`). It flows into audit grains. There is
no charset validation and no length cap beyond the global header budget. This
is not currently an escalation — `bind_principal` always builds a *restricted*
set (§1) — but a principal string containing control characters, whitespace
runs, or a name colliding with a reserved one is a log-injection and
audit-legibility problem at minimum.

**Add**: reject control characters and non-NFC forms; cap length; reject the
reserved names (`anonymous`, `user:console`); optionally
`--sso-principal-prefix user:` so proxy-asserted identities are visibly
distinct from credential-map ones in every audit grain.

### 7.2 Groups → principals

§3.3 of the enterprise proposal promised "+ optional groups header"; it was
never implemented (no `groups` handling exists in the server or CLI). Without
it, every SSO user must be granted individually in the file, which is the
thing SSO exists to avoid. **Add** `--sso-groups-header` with a declared
mapping to principals, so `GRANT` targets a role name that a directory group
resolves to.

### 7.3 Prefer a channel-bound proof over a shared static header secret

The proxy secret is a static bearer value in a header. Better proofs exist and
cost nothing to *document* as the recommended deployment: a **Unix domain
socket** between proxy and console (filesystem permissions become the proof,
and no secret exists to leak), or **mTLS** with the client certificate
verified by the `tls` feature. Ship the docs now; the socket listener is a
small, self-contained addition later.

### 7.4 Proxy-asserted identities may not approve, by default

**This is the recommendation to act on first.** Add
`--sso-approvals deny|allow`, defaulting to **`deny`**: a `request_principal`
that originated from an SSO header is refused at `POST /api/run/respond` with
the same 403 shape a shared token gets, unless the operator explicitly opts in.

The rationale is exactly §6's: the approval gate's whole value is that the
approver's identity is real, and a static shared secret cannot carry that
weight. An operator who understands the trade-off can accept it with one flag.
An operator who does not gets the safe default. Credential-map principals are
unaffected — they hold a per-principal secret, which is a materially stronger
claim than "some process presented the proxy secret."

This turns a silent structural weakness into an explicit, documented, opt-in
decision, and it costs one flag and one branch.

---

## 8. What we deliberately will not do

- **No SAML, no SCIM, no directory sync, no user provisioning.** OIDC plus the
  proxy pattern covers SAML shops.
- **No password database, no MFA/TOTP, no account recovery.** Areev
  authenticates; it does not manage humans.
- **No per-vendor SSO integrations.** §3.
- **No OAuth authorization-server role.** Areev is never an IdP, and never
  issues tokens to third parties.
- **No `client_credentials`.** §4.
- **No auth in CAL, the `.mg` format, or the engine.** CAL syntax is an OMS
  conformance contract; identity and transport are not query-language
  concepts.
- **No security config persisted in a memory file.** Invariant 5. A memory
  file must never arrive pre-armed with credentials. Grants (`mg:permits`) are
  in the file and stay there; *identity* stays host-side.
- **No intra-memory ACLs.** The memory is the isolation unit.
- **No ACME / certificate lifecycle.** A proxy's job.
- **No compliance-certification claims.** Mechanisms, never "compliant."

---

## 9. Phasing and gates

| Phase | Deliverable | Gate (in CI) |
|---|---|---|
| **A0 — approval trust floor** (§7.4) | `--sso-approvals deny\|allow`, default deny | An SSO-asserted principal gets 403 on `run.respond` by default and 200 with `--sso-approvals allow`; a credential-map principal is unaffected |
| **A1 — token model** (§5) | `areev auth mint/list/revoke`; entry `id`+`label`+`expires_at`; `areev_pat_` prefix; entropy warning; `429` after N failures; guidance flipped to `--auth` | A minted token round-trips; an expired entry is refused indistinguishably from unknown; a revoked id stops working while its principal's other tokens keep working; N+1 failures from one IP get 429 with no added latency; no failure path names a credential |
| **A2 — SSO v0 hardening** (§7.1–7.3) | Identity validation, reserved-name refusal, `--sso-principal-prefix`, `--sso-groups-header` + group→principal map, UDS/mTLS deployment docs | A control-character identity is refused; `anonymous`/`user:console` cannot be asserted; a group-derived principal receives exactly the file's grants for that role |
| **A3 — native OIDC** (§6), `oidc` feature | Auth-code+PKCE, discovery, JWKS validation via a vetted crate, `HttpOnly`/`Secure`/`SameSite=Strict` session cookie, logout that invalidates, idle+absolute timeouts, bounded session map | Round-trip against a local IdP; a token with a wrong `aud`, expired `exp`, replayed nonce, or JWKS-mismatched signature is refused; no token ever reaches the browser; `--sso-approvals` becomes moot for OIDC-authenticated principals |

A0 before A1 is deliberate: A0 is one flag and closes an impersonation path
into the audit trail. A3 last, and only if A0–A2 leave a gap that matters.

---

## 10. Dependency budget

Exactly **one** new dependency, in A3 only: a JWT/JWKS validation crate,
behind the non-default `oidc` feature, recorded in ARCHITECTURE.md §10 as the
second dependency-policy exception after rustls. The reasoning mirrors the
first: *crypto is the one domain where "hand-rolled, no deps" flips from
virtue to negligence.* Signature verification over an issuer-published key set
is squarely in that domain; the authorization-code flow itself is not, and
stays hand-rolled like the rest of the HTTP surface.

A0, A1 and A2 add **zero** dependencies. `hex` and `sha2` are already in
`areev-core`, and `getrandom` is already a workspace dependency
(`areev-run`, `areev-store`) — so `areev auth mint`, which lives in the CLI
above both, has its CSPRNG without a new crate. (Note the anon engine's
session tokens are HKDF-*derived*, not random — a different mechanism for a
different job; minting needs fresh entropy, not determinism.)

---

## 11. Risks

1. **Session state becomes the new attack surface** (§6.2). Mitigation: A3 is
   gated on A0–A2 landing, so it is never shipped under urgency; the session
   map is bounded from the first commit, the way `auth_failures` is.
2. **`--sso-approvals deny` breaks an existing deployment.** Anyone relying on
   proxy-asserted approvals today gets a 403 after upgrade. Mitigation: it is
   a one-flag opt-in, named in `CHANGELOG.md` as a behavior change, with the
   error text naming the flag and the reason. Failing closed on an approval
   path is the correct direction to break.
3. **Scope creep into an IdP.** Every OIDC implementation attracts "can it
   also do local users / MFA / SCIM." Mitigation: §8 is the answer, and it is
   a list, not a sentiment.
4. **Vendor-specific pressure.** "Google works but Entra needs X" is how
   `google.rs` gets born. Mitigation: §3's hard rule — quirks are config
   knobs, never modules.
5. **Claim rot.** "SSO" invites checkbox inflation on the README. Mitigation:
   a row flips to shipped only when its §9 gate is in CI, same discipline as
   the enterprise proposal §7.

---

## 12. Docs contract sweep

Per CLAUDE.md, a surface change is incomplete until its doc moves in the same
commit. This proposal touches:

| Phase | Must also update |
|---|---|
| A0 | `docs/security-model.md`, `docs/run.md` (the HITL section), in-binary `USAGE`, `CHANGELOG.md` |
| A1 | `docs/security-model.md`, `docs/cookbook.md`, `USAGE`, `ERROR_CODES.md` (new refusal codes, append-only), `docs/deployment-profile.md` |
| A2 | `docs/security-model.md`, `docs/deployment-profile.md` §"SSO note", `docs/runbooks/sso-secret-rotation.md`, `USAGE` |
| A3 | `docs/security-model.md`, `docs/deployment-profile.md`, `ARCHITECTURE.md` §10 (named decision + dependency exception), `CHANGELOG.md`, a new `docs/runbooks/oidc-setup.md` |

`areev-server`'s auth surface is covered by `tests/http_smoke.rs` and the
in-crate `sso_tests` module; every gate in §9 belongs in one of those two.

---

## References

- [RFC 9700 — Best Current Practice for OAuth 2.0 Security](https://www.rfc-editor.org/info/rfc9700/)
  ([oauth.net summary](https://oauth.net/2/oauth-best-practice/))
- [OAuth 2.0 for Browser-Based Applications (draft-ietf-oauth-browser-based-apps-26)](https://www.ietf.org/archive/id/draft-ietf-oauth-browser-based-apps-26.html)
- [MCP — Authorization](https://modelcontextprotocol.io/specification/draft/basic/authorization)
  (stdio transports SHOULD NOT follow it; use environment credentials)
- [Prometheus — Security model](https://prometheus.io/docs/operating/security/)
- [Grafana — Configure auth proxy authentication](https://grafana.com/docs/grafana/latest/setup-grafana/configure-access/configure-authentication/auth-proxy/)
- [GitHub — Managing your personal access tokens](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens)
  and [PAT rotation policies](https://github.blog/changelog/2024-10-18-new-pat-rotation-policies-preview-and-optional-expiration-for-fine-grained-pats/)
- [Auth for self-hosted apps: Basic Auth vs forward-auth vs OIDC][fa]
- [The hidden costs of open source SSO](https://workos.com/blog/open-source-sso-hidden-costs)

[fa]: https://blog.stackademic.com/auth-for-self-hosted-apps-basic-auth-vs-forward-auth-vs-oidc-plus-a-decision-table-5d390eeb4a7c
