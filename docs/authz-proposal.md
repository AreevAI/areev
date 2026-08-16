# Grants, not gates — RBAC for Areev

**Status:** superseded 2026-08-09 by
[`cal-all-you-need-proposal.md`](cal-all-you-need-proposal.md)
("CAL is all you need"), same day it was written. The mechanism here
(principals, verbs, enforcement chokepoint, phasing skeleton) survives inside
that document, but **D2 is reversed** (grants move into the memory file as
grains; the host file shrinks to a credential map), **D4 simplifies** to
namespace-only, the loop lifecycle and DCL enter CAL, and §7's
tombstone-inverse claim was wrong (FORGET is one-way — see cal2 D7). Kept for
lineage. Historically it superseded
[`areev-enterprise-proposal.md`](areev-enterprise-proposal.md) §3.1 (E1).

---

## 1. The thesis

Today Areev answers "may this operation run?" with **configuration**: a
per-process boolean (`allow_destructive_ops`), a CLI flag
(`--no-destructive-ops`), a confirmation flag (`--yes`), an all-or-nothing
token, and a grammar that refuses whole statement families. That is a switch
panel, not an authorization model. Every switch is process-wide, so every
caller behind one process is equally trusted, and "who may do what" cannot be
expressed at all.

The revision: **authorization decides, configuration doesn't.** Like MySQL or
Postgres — operations are always *expressible*; whether they *execute* depends
on what the calling principal was granted on that resource. If a principal
holds `delete` on a memory, it can delete there — whether it is a human, an
agent, or a batch job is irrelevant. A principal is a principal.

What does **not** change: the storage model. Grains stay immutable and
content-addressed; "delete" remains tombstone-or-crypto-erasure, never blob
mutation. Immutability is how the store works, not a permission — RBAC governs
*who may tombstone and erase*, not whether blobs can be edited (they can't).

## 2. Decision record (2026-08-09)

| # | Question | Decision |
|---|---|---|
| D1 | CAL grammar stance | **Grants gate, grammar grows.** The existing host-level destructive ops enter CAL — `FORGET <hash>`, `FORGET SUBJECT`, `PURGE OLDER THAN` — all executing iff the caller's grants allow. No raw `DELETE`-by-predicate, ever: deletion stays tombstone/erasure-shaped. The CAL spec's safety pillar is rewritten from "non-destructive by grammar, no unsafe mode" to "**destruction is authorization-gated**." |
| D2 | Grant storage | **Host auth file** (`areev-auth.json`): per-process, never persisted in a memory file (invariant 5), fail-closed on unknown keys, inert if stolen (no raw secrets in it — §6). Postgres additionally maps roles to native `GRANT`s as defense-in-depth (§8). Grants do **not** ride as grains. |
| D3 | Embedded API | **Principal everywhere, owner default.** The facade accepts an optional principal at open; absent = owner/superuser (MySQL `root@localhost`), so local dev and existing code are unchanged. Network surfaces always resolve token → principal. |
| D4 | Granularity | **Memory × namespace**, per-verb. MySQL's `db.table` maps to `memory.namespace`. Grain-level / subject-level ACLs stay a non-goal (the isolation unit is the memory — invariant 5). |

## 3. What exists today (the gates being replaced)

| Gate | Where | Granularity |
|---|---|---|
| `allow_destructive_ops: bool` (default **on**) | `areev-cal/src/executor.rs:77` — gates CAL `FORGET <hash>` | per-process |
| `enable_writes: bool` (Tier 1 toggle) | same config — gates `ADD`/`SUPERSEDE`/`ACCUMULATE` | per-process |
| `--no-destructive-ops` | `areev serve` / `ui` / `cal` | per-process |
| `--yes` | `forget-subject`, `purge-older-than` | per-invocation confirmation |
| Single `--token-env` | `ui` (optional) / `hub` (mandatory) | all-or-nothing |
| `admin` scope for server-path FORGET | `areev-server` | binary |
| Parser refusal of `FORGET USER/SCOPE`, `PURGE` | `areev-cal` parser (AST + executor arms already exist: `ast.rs:719`, `executor.rs:1299/1311/1449`) | structural |
| Saved-query bodies read-only | CAL | structural (stays — §5) |

And the part that makes this cheap — **enforcement machinery already exists
and is already exercised**: `areev_loop::Scope { Read, Write, Review, Apply,
Admin }` (`crates/areev-loop/src/engine.rs:30`) is checked engine-side
(destructive applies need `Admin` + `allow_destructive`, `write` grants
neither `review` nor `apply`, self-approval blocked). What's missing is that
every entry point hands out root: `ScopeSet::all()` at
`areev-cli/src/main.rs:2044` and `areev-server/src/lib.rs:748`, with a
hardcoded `user:console` actor. The loop's scopes become a *view* of the new
model (§5.4), not a second system.

## 4. The model

Three nouns, one check:

- **Principal** — a named caller: `user:anna`, `agent:support-bot`,
  `job:retention-sweep`. Humans and agents are not distinguished by the
  model. The special principal **owner** is the local root of trust (D3).
- **Verb** — an operation class (table below). Per-verb, exactly like
  `GRANT SELECT, DELETE`.
- **Resource** — `memory × namespace`, with `*` wildcards on either axis.

A **grant** is `(principal, verbs, memories, namespaces)`. A **role** is a
named verb bundle — sugar over grants, not a new mechanism. The single
enforcement question everywhere:

```
authz::check(principal, verb, memory, namespace) -> Ok | AUT-E001
```

### 4.1 The verb taxonomy

Every user-facing operation on every surface maps to exactly one verb. This
table is the contract; a cross-surface test pins it (§10).

| Verb | Covers | Today's gate |
|---|---|---|
| `read` | recall/search/history/provenance/forks/related/entity-at/step-actions/run-trace/runs-touching/stats/verify/log/novelty; CAL `RECALL`/`ASSEMBLE`/`EXISTS`/`HISTORY`/`DESCRIBE`/`EXPLAIN`/set ops/`COALESCE`; hub pulls | none / token |
| `write` | add, remember, record_tool_call, import/migrate/follow, memtool writes; CAL `ADD`/`ADD WORKFLOW`/`ACCUMULATE`; hub pushes | `enable_writes` |
| `supersede` | merge; CAL `SUPERSEDE`/`REVERT` — separated from `write` so an append-only logger principal cannot rewrite heads | `enable_writes` |
| `delete` | single-grain tombstone: CAL `FORGET <hash>`, MCP `areev_forget`, memtool delete | `allow_destructive_ops` |
| `erase` | bulk + crypto erasure: `forget_subject`, `forget_older_than` and (new, D1) CAL `FORGET SUBJECT` / `PURGE OLDER THAN` | host-only + `--yes` |
| `loop.run` | trigger analysis (`loop run`/`reflect`, `areev_loop_adapter`, `/api/loop/run`) | none |
| `loop.review` | approve/reject | `Scope::Review` |
| `loop.apply` | apply/rollback (a destructive apply additionally requires `delete`) | `Scope::Apply` (+`Admin`) |
| `admin` | `DEFINE/DROP TEMPLATE/QUERY`, reindex, analyzer config, `/api/loop/config`, hub segment admin | `Scope::Admin` / admin scope |

**Recommendation grains stay engine-authored** regardless of grants —
`host_addable: false` is a type-system fact, not a permission; no verb
overrides it.

### 4.2 Built-in roles

`reader` (read) · `writer` (read, write) · `editor` (read, write, supersede)
· `reviewer` (read, loop.review) · `operator` (read, write, supersede,
loop.run, loop.apply) · `admin` (everything **except `erase`**).

`erase` is deliberately in no built-in role — it must be granted explicitly
(or you are the owner). Blast-radius argument: with D1, bulk erasure becomes
reachable from every CAL surface (console box, MCP, saved hosts); an explicit
grant is the honest price for that reach. This is the one place we are
stricter than MySQL's root-can-everything, on purpose.

## 5. Enforcement architecture

**One check function, host-boundary enforcement, engine mechanisms unchanged.**

- **`areev_core::authz`** (new module): `Principal`, `Verb`, `Grant`,
  `AuthzSet`, `check()`, and the `areev-auth.json` loader (serde,
  `deny_unknown_fields`, fail-closed). Core is the right home because both the
  store and CAL layers need the types and core sits below everything.
- **`AreevFacade` carries an `AuthzSet`** (default: owner-all). Every facade
  dispatch — CAL execution included — consults `check()` with the statement's
  verb and the target memory/ns before touching the store. Mounts inherit the
  caller's authz with `read` masked on (mounts are already read-only).
- **CAL executor**: `CalExecutorConfig::allow_destructive_ops` and
  `enable_writes` stop being independent booleans and become *derived* from
  the caller's grants (kept as deprecated process-wide **caps** — either flag
  set restrictive still wins over any grant, belt-and-suspenders for "serve
  untrusted callers read-only"). The `FORGET`/`PURGE` executor arms that today
  refuse (`executor.rs:1299/1311/1449`) get wired to the store's existing
  `forget_subject`/`forget_older_than` under the `erase` verb.
- **Store-direct paths** (CLI verbs that bypass the facade, e.g.
  `forget-subject`): the CLI resolves its principal first (default owner;
  `--as <principal> --auth <file>` to run restricted) and calls `check()`
  before the store call. `--yes` **stays** — it is confirmation UX, not
  authorization.
- **Saved-query bodies stay read-only** (unchanged). A stored body executing
  with the *invoker's* grants would be a confused-deputy footgun; revisit only
  with a real use case.
- **Honesty clause (D3):** in-process enforcement is a guardrail, not a
  boundary — a Rust caller holding the file bytes can bypass the facade. The
  real boundaries are the network surfaces and the Postgres backend. The docs
  must say this plainly (security-model.md rewrite, §11).

### 5.4 Areev Loop unification

`areev_loop::Scope` stays (the engine is substrate-agnostic, zero Areev deps).
The adapter translates: `read→Read`, `write→Write`, `loop.review→Review`,
`loop.apply→Apply`, `admin→Admin`; a destructive apply requires `delete` on
the Areev side, replacing `allow_destructive`. The engine's
separation-of-duties checks (self-approval block, write∌review) keep working
untouched — they just finally receive real, distinct scope sets instead of
`ScopeSet::all()`. The token's actor replaces `user:console` on every audit
grain — the single change that most upgrades the audit story from append-only
to *accountable*.

### 5.5 Per-surface identity resolution

| Surface | Principal comes from | Default |
|---|---|---|
| Rust / Python / Node embedded | `open` option (`principal=` / `authFile=`) | owner |
| CLI | `--as` + `--auth` (or `$AREEV_AUTH`) | owner |
| MCP (`areev serve`) | `--as` + `--auth` at spawn (stdio = one principal per process) | owner |
| `areev ui` | token → principal via auth file; single `--token-env` stays = implied `admin` role | unauthenticated loopback = owner (today's posture) |
| `areev hub` | token → principal; per-token `memories` list scopes push/pull | mandatory auth (today's posture) |
| Postgres backend | facade check first; optional native role mapping (§8) | — |

## 6. The auth file

`areev-auth.json`, loaded per-process, `deny_unknown_fields`, **no raw secrets**:
tokens are referenced by env-var name or by SHA-256 of the bearer value, so a
synced or exfiltrated file is inert.

```json
{
  "version": 1,
  "roles": {
    "support-writer": ["read", "write", "supersede"]
  },
  "principals": {
    "user:anna":         { "roles": ["admin"],
                           "grants": [{ "verbs": ["erase"], "memories": ["support.db"] }] },
    "agent:support-bot": { "roles": ["support-writer"],
                           "memories": ["support.db"], "namespaces": ["caller", "shared"] },
    "job:retention":     { "grants": [{ "verbs": ["read", "erase"],
                                        "memories": ["*"], "namespaces": ["*"] }] }
  },
  "tokens": [
    { "sha256": "9f2c…", "principal": "user:anna" },
    { "env": "SUPPORT_BOT_TOKEN", "principal": "agent:support-bot" }
  ]
}
```

Resolution: token (or asserted local principal) → principal entry → union of
role verbs + explicit grants, intersected with the entry's
`memories`/`namespaces` (omitted = `*`). Custom roles may reference built-ins.
Unknown principal, unknown token, malformed file: **fail closed**.

## 7. Spec changes (`~/opensource/oms`)

D1 deviates from the published CAL spec, so the spec moves — we own it, and
CAL syntax is a conformance contract (invariant 4): **the spec PR merges
before the syntax ships** (phase gate in §10).

**CAL spec** (`CONTEXT-ASSEMBLY-LANGUAGE-CAL-SPECIFICATION.md`) — CAL 1.3:

- The pillar (§ "non-destructive by grammar, not by convention… no unsafe
  mode") is rewritten: CAL is **append-only by construction for evolution,
  authorization-gated for destruction**. Destruction means tombstone or
  crypto-erasure of whole grains; there is still no statement that mutates a
  stored blob, and no `DELETE`-by-predicate.
- Grammar: `FORGET <hash>` | `FORGET SUBJECT <id> [WITH text_mentions]` |
  `PURGE OLDER THAN <duration> [TYPE <t>]` become defined Tier-2 statements.
  `DELETE`, `DROP` (non-TEMPLATE/QUERY), `ERASE`, `DESTROY`, `TRUNCATE` remain
  reserved non-tokens.
- New section — **Authorization model**: Tier-2 execution REQUIRES a
  host-verified authorized principal (verbs `delete`/`erase`, resource
  memory × namespace); grant storage is host-defined and MUST NOT be carried
  in grain payloads; hosts without an authorization mechanism MUST refuse
  Tier 2. Tier table gains "Tier 2 — Destroy (authorization-required)".
- The defense-in-depth FAQ ("CAL cannot delete data because…") is rewritten
  to the new claim: *CAL destruction cannot happen without an authorized
  principal, and can never rewrite history* — and the stale "12 variants"
  closed-enum text is refreshed while we're in there.

**OMS spec** (`SPECIFICATION.md`) — OMS 1.6:

- §12 gains **12.6 Authorization**: principals (humans and agents uniformly),
  per-verb grants on memory × namespace, roles as bundles — RECOMMENDED host
  model, storage host-defined. Cross-references `author_did` (§12.1) as the
  natural principal id for signed deployments — `user:anna` and a DID are both
  valid principal names; DID-backed principals are the upgrade path, not a
  prerequisite.
- The audit-lifecycle text stating "CAL has no destructive verb… a destructive
  change MUST be proposed as `proposal_data`" is updated: `proposal_cal` MAY
  contain Tier-2 statements; a tombstone's inverse is un-tombstoning (blob
  survives), an erasure has no inverse and MUST be recorded non-rollbackable.
- §28 (erasure) aligns: subject/age-scoped erasure is now *expressible* in CAL
  where the host authorizes it — retiring the "documented deviation" status of
  [`erasure.md`](erasure.md), whose decision note gets updated to point here.

## 8. Backends

- **Embedded Turso**: no native authn exists (it's an in-process library) —
  the facade check *is* the mechanism. Nothing pretends otherwise.
- **Postgres**: real principals exist, so map them — optionally, as defense in
  depth, not as the source of truth: `areev pg sync-roles --auth areev-auth.json`
  creates one Pg role per principal, `GRANT USAGE` + `SELECT` on the memory's
  schema for `read`, `INSERT` for write-class verbs, nothing for others.
  Honest limit, documented: Pg cannot distinguish our verbs — a tombstone and
  a fact are both `INSERT`s, erasure is `DELETE` on index tables — so
  Areev-level checks remain authoritative; the Pg roles just make a stolen
  read-only connection string actually read-only. RLS stays out (D4 —
  intra-memory ACLs are a non-goal; tenancy = one memory per tenant, which
  schema-per-memory already mirrors).

## 9. Compatibility

Default behavior is **byte-for-byte unchanged**: no auth file + no principal =
owner = every verb (including `erase` — owner is above roles). Then:

- `allow_destructive_ops` / `--no-destructive-ops` / `enable_writes`:
  deprecated in docs, kept working as process-wide restrictive caps (§5).
  Removal is a 2.0 question, not this project.
- Single `--token-env`: unchanged, implied `admin` role (note: post-RBAC that
  means *no erase* over the network without an auth file — a deliberate,
  changelog-called-out tightening; today's single token can't erase via CAL
  anyway since the statements don't exist yet).
- `--yes` confirmations: unchanged (UX, not authz).
- New error domain **`AUT`** (append-only per `ERROR_CODES.md`): `AUT-E001`
  verb refused for principal, `AUT-E002` unknown principal, `AUT-E003` auth
  file invalid/unreadable, `AUT-E004` token unrecognized.
- Bindings: `open()` gains optional `principal=`/`auth_file=` (Python) and
  `principal`/`authFile` (Node) — additive, scalars in / JSON out unchanged.

## 10. Phasing

| Phase | Deliverable | Gate |
|---|---|---|
| **0 — Decide** | This doc + drafted spec diffs | Done (D1–D4) / spec PR reviewed |
| **1 — Authz core** | `areev_core::authz` types, auth-file loader, `AUT` codes | Unit tests incl. fail-closed loads; full suite green with zero callers |
| **2 — Plumbing** | Facade + surfaces carry `AuthzSet`; owner default; old flags become caps | **Zero-behavior-change gate**: entire existing suite green with no auth file present |
| **3 — Enforcement** | `check()` on every verb × surface; loop scope mapping; real actors in audit grains | Role × verb × surface **matrix test** (the §4.1 table, executable); loop separation-of-duties suite green with restricted sets |
| **4 — CAL growth** | Parser accepts `FORGET SUBJECT`/`PURGE OLDER THAN`; executor arms wired under `erase` | **Spec PR merged first**; conformance cases on both backends; `erasure.md` deviation note retired |
| **5 — Surfaces** | Multi-token ui/hub, per-memory hub grants, `--as/--auth`, bindings params, console login, optional Pg role sync | Enterprise §3.1 gate list: reviewer can approve-not-apply; no self-approval; audit carries token's actor; unknown-key auth file refuses; hub token scoped to A gets 403 on B |

Doc/reference updates ride the phase that makes them true:
`security-model.md` (rewrite, §5 honesty clause), `CLAUDE.md` invariant 3,
`ARCHITECTURE.md` decision log entry (mirroring `erasure.md`'s role),
`cal-reference.md`, `mcp-reference.md`, cookbook.

## 11. Risks

1. **The advisory-in-process trap.** Someone will read "RBAC" and assume the
   embedded library sandboxes hostile code. It does not (D3 honesty clause) —
   every doc that mentions grants states where the real boundaries are.
2. **Erasure reachable from CAL widens blast radius.** Mitigations: `erase` in
   no built-in role (§4.2), process-wide caps still win (§5), hub/ui require
   explicit grants, and erasure keeps writing its audit/report trail.
3. **Spec drift vs other OMS implementations.** CAL 1.3 is a *capability*
   version bump; Tier-2 support stays optional (hosts MAY refuse), so a 1.2
   conformant reader remains conformant.
4. **Two authz systems during transition.** Loop `ScopeSet` and `AuthzSet`
   coexist until §5.4 lands; phase 3's matrix test is the guard against a
   surface checking the wrong one.
5. **Token sprawl.** The auth file is one file per deployment, env/hash refs
   only; SSO (enterprise §3.3) later maps groups → roles onto this same model.

## 12. What we deliberately will not do

- **No `DELETE`-by-predicate, no blob mutation** — destruction stays
  tombstone/erasure-shaped (D1).
- **No grain-level or row-level ACLs** — memory × namespace only (D4).
- **No grants inside memory files** — host config never syncs (D2,
  invariant 5).
- **No new dependencies** — serde is already in-tree; token hashing is SHA-256
  (already in-tree); no auth framework, no JWT.
- **No identity *verification* in v1** — local principals are asserted, tokens
  are bearer. Signatures/DIDs are the existing OMS §12 upgrade path, not this
  project.
