# Areev for application builders — earning "Hermes for enterprise"

**Status:** proposal. Written 2026-08-08. Companion to
[`areev-run-proposal.md`](areev-run-proposal.md) (the runtime); this document is
the adoption plan and enterprise-capability roadmap that sits around it.
Nothing here touches `areev-core`, the `.mg` format, or the CAL grammar.

---

## 1. The claim, unpacked

> **Areev is Hermes for enterprise.**

Hermes Agent is the open-source, self-hosted, self-improving agent stack — the
thing an individual operator runs for themselves, and the closest comparable to
our whole story (`docs/loop-reflection.md`, `docs/loop-explainer.md`). What
it structurally lacks is everything a team deploying agents *at work* gets
asked for:

- Its memory is two size-capped markdown files (`MEMORY.md` 2,200 chars,
  `USER.md` 1,375) plus FTS5 over session history. When a file hits its cap the
  agent consolidates by **replacing** it — the prior wording is gone from disk.
- Its self-improvement loop runs with **write-approval off by default**: the
  agent edits its own memory and skills with no gate, no audit record, no
  separation of duties.
- There is no erasure story, no retention story, no roles, no actor identity.

The claim is not "use Areev instead of Hermes." We are already *inside* Hermes:
`examples/hermes/` is a shipped `MemoryProvider` plugin, verified against
Hermes 0.16.0 through its real plugin loader, with `prefetch()` at **0.83 ms
p50** (2,080 grains) on Hermes's *synchronous* per-turn path. The claim is the
answer to the question that follows: *"we love this loop — can we run it at
work?"*

### The claim → capability map

Every adjective in "enterprise" maps to a concrete capability. Most are
shipped; three are roadmap, and §3 is their design.

| "Enterprise" means | Areev answer | Status |
|---|---|---|
| Self-improvement is governed, not autonomous | Areev Loop's four gates: Propose (no free prose from analyzers) → Review (mandatory written reason, self-approval blocked) → Apply (scoped, inverse recorded) → Verify (metric re-run, revert proposed on regression) | **Shipped** (`docs/loop.md`) |
| Every change is auditable | Op-log records every add/supersede/forget (`areev log`, `changes_since`, `GET /api/log`); every Areev Loop transition writes an immutable, hash-chained audit grain with actor + reason | **Shipped** |
| Right to erasure & retention | `forget_subject` / `forget_older_than` (REQ-ERASE-1..8): full supersession history, dictionary, vocabulary, telemetry, CAS refs; tombstones replicate to peers; counts-only report | **Shipped** (`docs/erasure.md`) |
| The data is yours | One memory = one portable file (or one Postgres schema: `pg_dump` exports it, `DROP SCHEMA` erases it); AES-256-GCM at rest + crypto-erasure on the file backend | **Shipped** (blobs-sidecar caveat, `docs/security-model.md`) |
| You can explain what the agent did | `run_trace` / `run_yield` / `runs_touching` / `entity_at` — the graph surface, §2 rung 1 | **Shipped** (PR #56) |
| Ops your team already drilled | Postgres backend: schema-per-memory, concurrent writers, pgvector; inherits your failover/PITR/backup | **Shipped** (stage 1) |
| Encrypted transport | TLS | **Roadmap** — §3.2 |
| Least privilege | RBAC over the existing scope model | **Roadmap** — §3.1 |
| Enterprise identity | SSO (OIDC) | **Roadmap** — §3.3 |

### This resolves the Hermes bet

`areev-run-proposal.md` §10.1 left "the Hermes bet" open: A — stay the memory
layer under Hermes; B — replace Hermes in our own stack. The enterprise frame
dissolves the either/or:

- **Bet A is the funnel.** The `MemoryProvider` stays and stays excellent — it
  is how Hermes users meet us, and it is a live proof point (sub-millisecond
  prefetch where a network-backed provider pays a round trip per turn).
- **Bet B is the trajectory, rebranded.** `areev run` plus the enterprise plane
  is not "our Hermes replacement" — it is what a *team* graduates onto when the
  individual-operator posture stops being acceptable. That also sets `areev
  run`'s v1 bar: it must be demonstrable as a governed runtime, not
  feature-parity with Hermes's DX.

### The conversational line (claim discipline)

Safe to say today, because every clause is shipped:

> "Areev is Hermes for enterprise. Same self-improving loop — an agent that gets
> better from its own experience — but every memory write passes four
> governance gates, every decision leaves a hash-chained audit record with a
> named actor and a written reason, right-to-erasure is one command that
> reaches replicas, and the memory itself is a file you own — or a Postgres
> schema your ops team already knows how to run."

And the differentiation follow-up:

> "Hermes keeps memory in two size-capped markdown files; when they fill up,
> consolidation rewrites them and the old wording is gone from disk. In Areev
> nothing is ever overwritten — every version stays addressable — and we're
> already the fastest memory provider Hermes can load: sub-millisecond
> prefetch on its synchronous turn path."

Rules:

1. **Never present-tense TLS, RBAC, or SSO.** The honest form: "on the
   security roadmap; today you deploy it the way enterprises already deploy
   internal tools — behind an authenticating TLS proxy — and the design for
   native support is written."
2. **Re-verify the Hermes facts per release.** Hermes postdates the Jan-2026
   knowledge cutoff and moves fast; the char-cap and write-approval claims must
   be re-checked against the current version before being repeated.
3. Same discipline as always: never headline LoCoMo, never claim what
   `honesty_metrics` doesn't measure.

---

## 2. The adoption ladder

Four rungs. Each is independently valuable — a builder can stop at any rung
and have gotten their money's worth — and each makes the next one the obvious
question.

### Rung 0 — drop-in memory (day one, zero lock-in)

`pip/npm/cargo install areev`; `areev migrate` imports from mem0 (with real
supersession history), LangGraph/LangMem, Letta (+archival), Zep/Graphiti,
basic-memory, generic JSONL, and OpenAI-style tool logs — idempotent, so
re-running is safe; the MCP server (16 tools) and the Anthropic memory-tool
adapter plug into stacks we'll never see; the Hermes provider slots into an
existing Hermes install with two commands.

The trust move at this rung is the **exit asymmetry**: the memory is a
self-describing portable file. Coming in is one command; leaving is copying a
file. Say that out loud — it is the opposite of every memory-SaaS pitch.

### Rung 1 — memory-native app features (the "more value")

This is where the graph-engineering work (PR #56) turns into features
*builders ship to their users*, not engine internals. Four recipes worth
documenting as first-class cookbook entries:

| The feature in their app | The call | Notes |
|---|---|---|
| **"Why did the agent do that?"** — a support/debug panel showing a run's full transcript *and* the durable knowledge it produced | `run_trace` (+ its `run_yield` leg: grains *derived from* the run that outlived it) | Requires stamping `run_id` at write time — `remember(..., run_id)` exists on every surface |
| **Blast radius** — "this fact is wrong; which runs produced or refined it?" | `runs_touching` | Honest caveat, always stated: runs that merely *read* a grain leave no grain and are invisible to this |
| **"What did we know when?"** — dispute and audit answers on two time axes | `entity_at` with `world` (what was true then) vs `knowledge` (what we believed then) | Bitemporal as-of reads; nobody else in the agent-memory space surfaces the distinction |
| **Related-entities navigation** — k-hop graph exploration | `related` (bounded BFS, `out`/`in`/`both`) | Reverse traversal needs `entity_relations` declared on the file; pre-existing files need one `areev reindex` |

**Gap to close (Phase E0):** these ride the store API, CLI, MCP, and both
bindings — but **not** the HTTP server (the console's graph view is built off
`/api/browse`). A web-app builder who isn't linking Rust can't reach them.
Add read-only `GET /api/graph/*` endpoints for the four reads; no new
semantics, pure exposure.

### Rung 2 — the runtime

`areev run`, per the companion proposal: the agent runtime whose execution
history *is* queryable memory, with deterministic replay from a journal we
already have (every tool result an immutable content-addressed grain). Rung 1
is also its demo surface — a runtime whose every run is born answerable by
`run_trace`.

### Rung 3 — the enterprise plane

Hub + RBAC + TLS + SSO + governance + erasure: the rung where the buyer is a
platform team with a security review, and the checklist in §1's table is the
sales artifact. §3 is the build plan for the three missing rows.

---

## 3. The enterprise plane: RBAC, TLS, SSO

Ordering argument: **RBAC first.** TLS already has a deployment answer today
(the documented TLS-terminating proxy — which is how enterprises front
internal tools anyway), and SSO v0 rides that same proxy. RBAC has *no*
workaround — today every surface is all-or-nothing — and it needs zero new
dependencies because the model already exists.

### 3.1 RBAC — the model exists; the grants don't

The finding that makes this cheap: `areev_loop::Scope { Read, Write, Review,
Apply, Admin }` (`crates/areev-loop/src/engine.rs:30`) is already defined **and
already enforced engine-side** — destructive applies require `Admin` +
`allow_destructive`, `write` grants neither `review` nor `apply`,
self-approval is blocked, analyzer config requires `Admin`. What's missing is
that every entry point hands out root: the CLI grants `ScopeSet::all()`
unconditionally (`areev-cli/src/main.rs:2044`) and so does the server
(`areev-server/src/lib.rs:748`), with a hardcoded `user:console` actor.

**Design:**

- **Multi-token auth.** Replace the single `--token-env` secret with a host
  config file (working name `areev-auth.json`) mapping tokens → `{actor,
  scopes, namespaces?, memories?}`. Roles are just named scope bundles
  (`reader`, `writer`, `reviewer`, `operator`, `admin`) — sugar over
  `ScopeSet`, not a new model.
- **Config posture copied from `loop-policy.json`:** host-side file, unknown
  keys rejected at load, **never persisted in a memory file** (invariant 5:
  host config is per-process). A stolen or synced auth file must be inert
  without its tokens.
- **Enforcement at the host boundary only** — the server resolves token →
  `ScopeSet` + actor and passes both down; the engine's existing checks do the
  rest. Nothing enters the CAL grammar, the file format, or the engine.
- **Real actors in the audit chain.** The token's `actor` replaces
  `user:console` on every audit grain — this is what makes the audit story
  *accountable* rather than merely append-only, and it's the single change
  that most upgrades the §1 table.
- **Hub grants are per-memory.** The hub serves a directory of memories; a
  token's `memories` list scopes which ones it can push to or pull from.
- Single-token `--token-env` stays as the simple mode (implied
  `admin`) — no breaking change.

**Non-goals:** grain-level or row-level ACLs inside a memory. The isolation
unit *is* the memory (invariant 5) — tenancy is one memory per
principal/tenant, which the Postgres backend's schema-per-memory shape already
mirrors. Intra-memory ACLs would be a permanent complexity tax fighting the
architecture.

**Gate:** a `reviewer` token can approve but not apply; an `apply` token
cannot approve its own recommendation; the resulting audit grain carries the
token's actor; an auth file with an unknown key refuses to load; a hub token
scoped to memory A gets 403 on memory B's segments.

### 3.2 TLS — proxy stance stays; native TLS becomes a feature flag

Today: no TLS anywhere, by explicit design (`docs/security-model.md`: "No
TLS… front with a TLS-terminating reverse proxy"); both `areev ui` and `areev
hub` refuse non-loopback binds without `--allow-remote` and warn loudly.
"First-class TLS for the hub" is already on the security roadmap, and the
Postgres proposal already pre-decided the dependency posture: "rustls only
if/when demanded."

**Design:**

- **The reverse-proxy pattern remains the documented default forever.**
  Enterprises terminate TLS at their edge regardless; native TLS is for the
  deployments with nowhere to put a proxy (edge boxes, Pis, appliances).
- **Native TLS = non-default cargo feature (`tls`), rustls.** Hub first —
  a hub exists to be written to over a network; the console is loopback by
  design and inherits second. Server-side on the hub listener; client-side on
  the sync verbs (`areev follow`, pushes). Certificate + key as PEM paths via
  flags/env.
- **Same feature covers `open_postgres` over TCP+TLS** (today's guidance is
  Unix-socket-only for Cloud SQL).
- **This is a recorded dependency-policy exception**, the same shape as the
  erasure decision: hand-rolling TLS is the one thing no one should ever do,
  and rustls is the boring, auditable industry default. One paragraph in
  ARCHITECTURE.md's decision log, mirroring `docs/erasure.md`'s role.
- **No ACME, no cert lifecycle management.** That is a proxy's or an
  operator's job; taking it on is a permanent support surface.

**Gate:** CI round-trips a hub push/pull over TLS; with TLS configured,
plaintext connections are refused (no silent downgrade); the `--allow-remote`
warning text distinguishes "remote + TLS" from "remote + plaintext."

### 3.3 SSO — trusted-header first, OIDC second

**v0 — trusted-header auth (zero new dependencies).** The standard pattern for
self-hosted tools: an authenticating proxy (oauth2-proxy, Pomerium,
Cloudflare Access, IAP) does OIDC/SAML against the IdP and forwards identity
in headers. Areev's part: `--sso-header X-Forwarded-User` (+ optional groups
header), honored **only** when the request also carries a proxy shared secret
— otherwise headers are attacker-controlled input. Identity becomes the
actor; groups map to §3.1 roles via `areev-auth.json`. This composes with the
TLS proxy stance — one proxy does both — and ships as soon as RBAC does.

**v1 — native OIDC for the console.** Authorization-code flow + session
cookie, for the deployments that resent the proxy. The code flow itself is
small, but **JWT/JWKS validation must be a vetted crate, not hand-rolled** —
the second recorded dependency exception (behind the same non-default feature
posture as TLS). Machines keep bearer tokens; SSO is for humans in the
console and hub admin surfaces.

**Non-goals:** SAML (OIDC only — the proxy pattern covers SAML shops), SCIM /
directory sync, user provisioning. Identity lives in the IdP; Areev only maps
it to scopes.

**Gate:** a demo deployment behind oauth2-proxy where console login lands as a
named actor with group-derived scopes, and a request with forged identity
headers but no proxy secret is rejected.

### 3.4 The evidence pack (small, cheap, very enterprise)

Two gaps the sweep found, worth folding into E0/E1:

- `changes_since` (the op-log cursor read) is exposed in Rust, the CLI, and
  the HTTP API — but **not in the Python or Node bindings**. Add it; the
  audit story should be reachable from every surface.
- ~~An `areev audit export` verb~~ — **shipped** with the GDPR compliance pack:
  the Tier-2 destruction trail plus the Areev Loop lifecycle chain as JSONL,
  hash-chain verified (a record whose named predecessor is absent from the
  export is flagged, because evidence that cannot say it was truncated is
  worse than none). `--since`/`--until` window it; `--out` writes the file.
  See [`gdpr.md`](gdpr.md) §1.

---

## 4. Architectural rules (non-negotiable)

1. **Everything in §3 is host-plane.** Server, CLI, host config. The engine,
   the `.mg` format, and content addressing are untouched. If a phase needs a
   core change, the design is wrong — same rule as `areev run` §5.
2. **Nothing enters the CAL grammar.** Auth, identity, and transport are not
   query-language concepts, and CAL syntax is an OMS conformance contract.
3. **Security config is never persisted in a memory file.** Auth files, TLS
   material, SSO settings are host config (invariant 5). A memory file must
   never arrive pre-armed with permissions.
4. **The dependency-light policy stays; exceptions are recorded decisions.**
   Exactly two are proposed — rustls (TLS) and a JWT-validation crate (OIDC
   v1) — both behind non-default features, both written up in the decision
   log like the erasure deviation. Crypto is the one domain where
   "hand-rolled, no deps" flips from virtue to negligence.

---

## 5. Phasing

| Phase | Deliverable | Gate |
|---|---|---|
| **E0 — Truth & reach** | Fix doc drift (README/mcp-reference say 8 MCP tools, CLAUDE.md says 11, the code registers 13); read-only `GET /api/graph/*` for the four graph reads; `changes_since` in both bindings; rung-1 cookbook recipes; a one-page "Hermes for enterprise" positioning doc sourced from §1's table | Every public claim has a file/test pointer; graph reads reachable from a browser app |
| **E1 — RBAC** | Multi-token `areev-auth.json`, roles as scope bundles, per-memory hub grants, real actors in audit grains, `areev audit export` | §3.1's gate, in CI |
| **E2 — TLS** | `tls` feature: rustls on hub (server) + sync (client) + Postgres TCP; decision-log entry | §3.2's gate, in CI |
| **E3 — SSO** | v0 trusted-header + group→role mapping; then v1 native OIDC for the console | §3.3's gate |
| *(parallel)* | `areev run` proceeds per its own proposal's phases 0–5 | Its own gates |

E1 before E2 is deliberate: RBAC has no workaround and no new dependencies;
TLS has a good workaround (the proxy) and needs a policy decision first.

---

## 6. What we deliberately will not do

- **No grain- or row-level ACLs.** The memory is the isolation unit; tenancy
  is one memory per principal.
- **No identity or permissions in the file format or CAL.** §4.
- **No multi-tenant SaaS control plane.** Areev stays an engine + self-hosted
  surfaces. (A managed offering is a separate business decision, not this
  document.)
- **No cert lifecycle / ACME.** Proxy's job.
- **No SAML, no SCIM.** OIDC + the proxy pattern cover them.
- **No compliance-certification claims.** SOC 2 / HIPAA / ISO are
  organizational attestations, not code features. The honest line: "the audit,
  erasure, and retention *mechanisms* your compliance program needs" — never
  "compliant."

---

## 7. Risks

1. **Claim rot.** "Enterprise" invites checkbox inflation. Defense: the §1
   table is the only source for the pitch, and a row flips to "Shipped" only
   when its phase gate is in CI.
2. **Hermes moves.** The comparison facts postdate the knowledge cutoff and
   Hermes evolves quickly; a stale claim ("two markdown files") repeated after
   Hermes fixes it would burn credibility precisely with the audience this
   targets. Re-verify per Hermes release; the `examples/hermes` CI smoke
   against the pinned version is the tripwire for provider breakage.
3. **Dependency-policy erosion.** Two exceptions are proposed; the third ask
   will cite these as precedent. Defense: exceptions require the same
   decision-log write-up as erasure, and "crypto only" is the stated
   principle.
4. **Solo-maintainer surface growth.** Auth and TLS are forever-surfaces
   (CVEs, config support). Mitigation: proxy-first stances keep the native
   implementations small and optional; non-default features keep them out of
   the default build and its audit surface.
5. **The wrong buyer conversation.** Rung-3 conversations pull toward RFP
   checklists (HA, DR, SLAs, certifications). The position to hold: Areev is
   the *engine with the mechanisms*; the deployment inherits ops from the
   platform under it (a file on your disk, or your Postgres).
6. **Carried from `areev-run-proposal.md`:** the memory/execution split is an
   annoyance, not a blocker — but note the asymmetry: *governance and erasure
   are blockers* for regulated buyers. The enterprise frame aims this
   proposal at the genuine blocker, which is why rung 3, not rung 2, is the
   revenue story.

---

## 8. Decisions needed

1. **Adopt the reframed Hermes bet** (§1): provider = funnel, `areev run` +
   enterprise plane = trajectory. This closes `areev-run-proposal.md` §10.1 and
   sets `areev run`'s v1 bar (governed runtime demo, not Hermes DX parity).
2. **The two dependency exceptions** — rustls and a JWT-validation crate,
   non-default features, decision-log entries. Yes/no on each.
3. **`areev-auth.json` shape and home** — confirm the loop-policy posture
   (host file, unknown-keys-rejected, never in the memory file) and the token
   → `{actor, scopes, namespaces?, memories?}` schema before E1 starts.
4. **Console parity for SSO/TLS** — recommendation: hub first for both, the
   console follows only on demand (it is loopback-by-design and most console
   deployments never leave the operator's machine).
