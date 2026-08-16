# CAL 1.3 — "CAL is all you need"

**Status:** proposal, synthesized 2026-08-09 from a four-track deep review
(coverage audit, DCL design, governance design, positioning/spec strategy);
same-day consistency + adoption re-review applied (REMEMBER pulled to
Wave 1, Tier-2 audit trail added, surface-consolidation policy added).
Nothing built except where marked shipped.
**Supersedes [`authz-proposal.md`](authz-proposal.md)** — that
document's mechanism (principals, verbs, enforcement, phasing) survives inside
this one, but three of its four decisions are revised (§2), and its scope
grows from "authorization" to "one language."

---

## 1. The thesis

The adoption claim: **CAL is all you need to operate your agent's memory** —
assemble context, evolve knowledge, review and apply the agent's own
improvements, erase for compliance, and grant who may do which of those. One
auditable grammar, identical across CLI, console, MCP, and every binding. The
way SQL unified query, definition, and control (GRANT/REVOKE are SQL-89 — DCL
in the language is 35 years of precedent, not exotic), CAL puts recall,
evolution, governance, and access control in one language.

The line that is genuinely new and true: **your agent speaks CAL, so the
language boundary is the enforcement boundary.** Platforms bolt RBAC onto
dashboards humans look at; CAL puts permissions where the agent operates —
and the agent can ask "what am I allowed to do?" in the same language it acts
in.

The boundary, stated in the announcement itself, never a footnote:

> CAL deliberately does not authenticate principals, hold keys, or move
> bytes — the host proves who you are, custodies encryption, and transports
> files; CAL governs what an authenticated principal may do inside a memory.

Operationally: **"If it needs a filesystem path, a credential, or a process
to exist, it's a host verb; everything that acts on grains in an open memory
is CAL."** That one sentence classifies every operation in the product (§3).

The truth condition, enforced before the headline ships: **no governed
operation requires dropping out of CAL** to a config flag, JSON file, or
HTTP-only endpoint — a cross-surface parity test, in CI (§10).

## 2. Decision record

Revisions to `authz-proposal.md`'s D1–D4, plus new decisions. D-numbers
continue.

| # | Decision |
|---|---|
| D1 *(extended)* | Grammar grows beyond erasure: destruction (Tier 2) **and** governance + control (Tier 3) enter CAL. Destruction stays tombstone/erasure-shaped: takes **a hash, an identity, or an age — never an arbitrary predicate, and never a key**. |
| D2 *(reversed)* | **Grants live in the memory file as grains; credentials stay host-side.** The Postgres split: object ACLs live inside each database (`pg_class.relacl`), passwords outside (`pg_authid`). Grants ride as `Fact + mg:permits`/`mg:revokes` — vocabulary the OMS/CAL specs already define (relation category `PERMISSION`) — giving append-only grant history, CAL queryability, and replication for free, with zero format change. `areev-auth.json` shrinks to a **credential map only**: `{token sha256/env → principal name}`. A stolen memory file reveals ACL structure but zero secrets; a stolen credential file names principals but grants nothing the file's own ACL doesn't gate. |
| D3 *(unchanged)* | Principal everywhere, owner default. Owner-on-open is the implicit superuser (`root@localhost`); a file with zero grant grains grants nothing to anyone else — fail-closed for non-owners, byte-identical to today for the single-user path. |
| D4 *(simplified)* | Resource granularity is **namespace-only** — grants live in the memory they govern, so the memory axis is implicit. `GRANT read ON caller`, `ON *`. Cross-memory = each file's own grants. |
| D5 *(new)* | **The loop lifecycle enters CAL.** All four gates are engine-side and parameterized on `(actor, scopes, because)`, not on the entry point — a principal-bound executor preserves them mechanically. Loop **policy writes stay host** (the policy gates CAL; CAL editing it is self-licensing). |
| D6 *(new)* | **Spec is CAL 1.3** (paired with OMS 1.6), with four conformance tiers (§4). Decided: the change is additive in the semver sense — every 1.x statement stays valid, and the tier system keeps existing implementations conformant — so a minor version is the honest number. The announcement carries the launch; the version number doesn't have to. |
| D7 *(new)* | **FORGET is one-way.** No `UNFORGET`; an undoable erasure is not an erasure. (This corrects `authz-proposal.md` §7, which wrongly claimed a tombstone's inverse is un-tombstoning — the engine has this right: additions roll back by *retraction*; tombstones and erasure are final.) |
| D8 *(new)* | **`erase` is admin-grantable** (SQL parity — decided over the owner-only carve-out). In v1 every `erase` grant is explicit — an `admin` (or the owner) may issue one, including to themselves, and the grant grain makes the delegation auditable. If roles ship (D9), no built-in bundle ever carries `erase`. |
| D9 *(new)* | **Roles are plan-first.** `DEFINE ROLE` is designed here (§5.2) but **not committed to v1** — a dedicated design note gets written and approved before any role implementation; the v1 grant surface may ship verbs-only. |
| D10 *(new)* | **The headline waits for Wave 1 + Wave 2**: DCL, governance, and erasure end-to-end *plus* the as-of reads and the run↔memory join in-language, so the claim is broad at launch (§10). |

## 3. What CAL covers — audit result

Ground truth from the coverage audit: CAL's reachability ceiling is the
`CalStoreFacade` trait (`areev-cal/src/facade.rs:58-304`); anything not
behind it is unreachable regardless of grammar. Classification of every
user-facing operation:

- **Already CAL (A):** add, recall/search, get, history, supersede/revert/
  accumulate, single-grain FORGET, templates/saved queries, ASSEMBLE/STREAM,
  fork detection (as `CONTRADICTIONS OF`), forward provenance.
- **Becomes CAL (B)** — ranked by adoption value:
  1. **Loop lifecycle** (`RUN LOOP`, APPROVE/REJECT/APPLY/ROLLBACK,
     DESCRIBE LOOP/ANALYZERS/OUTCOMES) — the differentiator is currently the
     one thing the "one language" can't touch; every demo switches surfaces
     mid-story.
  2. **The run↔memory join** (run-trace / runs-touching / step-actions
     walks) — the pitch is "execution history *is* queryable memory," yet the
     query language can't express the join.
  3. **Bulk erasure** (`FORGET SUBJECT`, `PURGE OLDER THAN`) — cheapest to
     ship: AST + executor arms + facade stubs all exist; only the parser
     refuses (`parser.rs:5047`, `:1279`).
  4. **As-of reads** (`ENTITY … AT`) — bitemporal time-travel is the
     wow-demo; `SINCE/UNTIL` gets mistaken for it and disappoints.
  5. **REMEMBER** — the highest-frequency onboarding verb.
  6. Graph walk with direction/depth control; 7. reverse provenance;
  8. MERGE (atomic fork close); 9. tool-call occurrence semantics in ADD;
  10. DESCRIBE STATS/INTEGRITY, novelty.
- **Stays host (C):** open/serve/ui/hub/repl (process), bundle/stream/
  restore/import/migrate/follow (paths), init/hooks (glue), encryption keys
  and embedder config (credentials/capability), memtool (protocol adapter),
  loop policy writes (self-licensing). Every exclusion is explained by the §1
  boundary sentence. Note `REINDEX` and integrity/stats *pass* the sentence —
  they're admitted (late wave), with Postgres `REINDEX` as precedent.

### The waves

- **Wave 1 — the claim's core:** DCL (§5), governance statements (§6), bulk
  erasure wired, **and `REMEMBER`** — pulled forward from the capture wave
  because it is the highest-frequency onboarding verb and the community
  adopter's first five minutes should never force a second surface; the
  enterprise buyer who needs GRANT arrives later and doesn't come via HN.
  `BECAUSE` is **mandatory on every *new* Tier-2/-3 statement** and
  **optional-but-recorded on the existing `FORGET <hash>`** — making it
  mandatory there would break a legal statement and violate the P2
  zero-behavior-change gate.
- **Wave 2 — the reads that complete the story:** `ENTITY … AT`, the
  run↔memory join statements, reverse provenance, first-class fork listing,
  `DESCRIBE STATS`/`INTEGRITY`.
- **Wave 3 — capture & resolution:** `MERGE`, graph-walk options,
  `ADD tool` occurrence stamping, novelty. Each Wave-2/3 statement
  is individually a spec decision (invariant 4) — the waves are scope, not a
  blank check.

## 4. The tier model

Hosts declare tiers; conformance claims are per-tier. Interop rides the `.mg`
file + Tier-0 reads — **tiers gate operations, not portability**. This
sentence goes in both specs; it is the answer to "small implementations must
now build RBAC" (no — they declare Tier 0/1 and stay conformant forever).

| Tier | Name | Contents | Gate |
|---|---|---|---|
| 0 | **Core** | read/assemble — the old non-destructive CAL, verbatim | none |
| 1 | **Evolve** | append-only writes (ADD/SUPERSEDE/ACCUMULATE/REVERT) | `write`/`supersede` verbs |
| 2 | **Destroy** | FORGET hash/subject, PURGE older-than | `delete`/`erase` verbs; requires *an* authorization model (host-defined authz suffices — Tier 2 without Tier 3 is legal) |
| 3 | **Control** | GRANT/REVOKE/roles, governance lifecycle, principal-bound sessions | `admin` + loop verbs; implies the model Tier 2 consumes |

A session without grants is exactly the non-destructive CAL of 1.x — **that
language is now the floor, not the ceiling.**

`DESCRIBE CAPABILITIES` reports the host's supported tiers — capability
discovery doubles as the conformance declaration surface, and it is how an
agent (or a doc example) learns what it may attempt before attempting it.

## 5. The control plane (DCL)

### 5.1 Storage: grants are grains

Grants ride as `Fact + mg:permits` / revocations as `mg:revokes` — both
already in the CAL spec's relation table with category `PERMISSION`
(`RECALL WHERE relation IS PERMISSION` is *already specified*), and OMS §6.11
already defines `authorized_namespaces` (`ans`). Consequences, all free:
append-only grant history (a REVOKE is a withdrawal grain, nothing deleted),
audit by construction, hub/bundle/PITR replication, CAL introspection. Do
**not** overload Consent (0x0A) — that is data-subject consent
(GDPR/HIPAA); RBAC is `Fact + mg:permits`. If grant reads ever become
perf-critical, promotion to a dedicated grain type follows the documented
Consent/Skill precedent (spec §8.10–8.11) — later, if proven.

Enforcement: an `AuthzSet` cache built from PERMISSION-relation heads at
open, invalidated on PERMISSION writes (same pattern as the dictionary
caches). The recall path is untouched. Cross-process invalidation is moot on
the embedded backend — the single-writer open-path registry means one handle
owns the file, so the cache cannot go stale under it.

**Bootstrap:** the owner writes the first GRANT. **Tamper:** whoever holds
the file bytes is root over it — true of MySQL's and Postgres's data dirs
too; say it plainly. Content addressing + op-log make out-of-band edits
*detectable* (`areev verify`); COSE signing (OMS §9) is the high-assurance
upgrade path, not v1. **Replication rule (new, for the spec):** grant grains
replicate as *sync* (applied without re-authorization, like recommendation
grains); *authoring* one requires `admin`/owner. **Disclosure caveat:** a
synced file reveals its principal names to replicas — documented; orgs that
care use opaque principal ids.

### 5.2 Syntax

Follows existing CAL conventions (statement-initial verb, `WITH` options,
the `DEFINE`/`DROP` family):

```
GRANT read, write ON caller TO agent:support-bot
GRANT delete ON * TO user:anna WITH because("support rotation")
REVOKE write ON caller FROM agent:support-bot WITH because("offboarded")

DEFINE ROLE editor AS (read, write, supersede)
DROP ROLE editor
GRANT ROLE editor ON * TO agent:refiner

SHOW GRANTS [FOR agent:support-bot]      -- sugar over RECALL … IS PERMISSION
DESCRIBE PRINCIPAL agent:support-bot     -- effective verbs per namespace
```

- Verbs: `read, write, supersede, delete, erase, loop.run, loop.review,
  loop.apply, admin` (unchanged from the authz proposal).
- **Principals are implicit** — a principal is a name; identity *binding* is
  the host credential map. No `CREATE PRINCIPAL` (also: `CREATE` stays a
  blocked token; `DEFINE ROLE` matches `DEFINE TEMPLATE/QUERY` exactly).
- Roles live as meta rows (`rol:<name>`, the `tpl:`/`qry:` precedent) with
  **live resolution** (editing a role updates holders — SQL semantics); role
  definitions are admin-gated. **Plan-first (D9):** this is the design sketch;
  roles ship only after their own design note is approved — v1 may be
  verbs-only.
- Who may GRANT: `admin` (and owner). `erase` sits in no built-in role
  bundle, but an `admin` may grant it explicitly (D8) — delegation is
  auditable because the grant is a grain. No `WITH GRANT OPTION` in v1.
- GRANT/REVOKE are **append-only writes, not destructive**: Tier 3 is gated
  by `admin`, capped by `enable_writes`, untouched by
  `allow_destructive_ops`.
- No `WITH expires(...)` in v1 — time-dependent authorization needs its own
  semantics (cache expiry, determinism) before it earns grammar (§12).

### 5.2b Session binding per surface

The lookup splits: the credential map resolves the *name*, the file's grant
grains resolve the *rights*.

| Surface | Principal comes from | Default |
|---|---|---|
| Rust / Python / Node embedded | `open` option (`principal=` / `authFile=`) | owner |
| CLI | `--as` + `--auth` (or `$AREEV_AUTH`) | owner |
| MCP (`areev serve`) | `--as` + `--auth` at spawn (stdio = one principal per process) | owner |
| `areev ui` | token → principal via the credential map; single `--token-env` stays = implied `admin` | unauthenticated loopback = owner (today's posture) |
| `areev hub` | token → principal; per-memory scoping via each file's own grants | mandatory auth (today's posture) |
| Postgres backend | facade check first; optional native role mapping as defense-in-depth (D2) | — |

When a credential map is configured on `ui`, unauthenticated requests become
principal `anonymous` (read-only unless granted) — formalizing today's
read-only `/api/cal` carve-out.

### 5.3 What this breaks, deliberately

- `GRANT`/`REVOKE` leave the lexer's blocked-token list — that block existed
  precisely to force a spec decision; this is the decision. `CONSENT`,
  `POLICY`, key/crypto vocabulary stay blocked forever.
- CLAUDE.md invariant 5 is **restated**, not broken: the real line is
  *credentials/host capability* (embedder, limits, tokens — never in the
  file) vs *truths about the file's data* (text_index, saved queries — and
  now access policy). The enterprise proposal's "a stolen auth file must be
  inert" survives — narrowed to the credential file, which is now inert by
  construction.
- CAL spec line "CAL cannot touch encryption keys, policies, or consent
  records" is rewritten: **keys and credentials never; access policy is
  Tier-3 DCL under authorization.**

## 6. Governance in CAL

The old objection ("CAL statements carry no actor identity") dissolves under
principal-bound sessions — verified against the engine: every gate is
enforced in `areev-loop` on `(actor, scopes, because)` regardless of entry
point (review `engine.rs:839-866`, apply `:946-966`, rollback `:1043-1046`,
hash-chained audit `:869-880`).

```
RUN LOOP [FULL] [WITH min_new(3), if_stale("6h")]   -- FULL = reflect; rides the RUN prefix
APPROVE <hash> BECAUSE "matches our deploy policy"
REJECT  <hash> BECAUSE "false positive – seasonal"
APPLY   <hash> BECAUSE "approved in standup"
ROLLBACK <hash> BECAUSE "regressed @30d"
DESCRIBE LOOP | ANALYZERS | OUTCOMES | POLICY        -- reads; engine APIs exist
```

`BECAUSE` is already a lexer token — mandatory reason becomes a **parse
error**, with the engine's non-empty check as the second layer. `RUN LOOP`
carries no credentials ever (LLM backends stay host-configured).

**Three residues that do not dissolve automatically** (all resolved in the
design):

1. **ObserverType must derive from the principal's credential record**
   (`observer: human|agent`) — never from statement text or a hardcoded
   `ObserverType::Human` (today: `areev-server/src/lib.rs:748`). A statement
   must never be able to claim humanity.
2. **Self-approval gap — FIXED 2026-08-09 (`b3d9a2c`), ahead of this
   project:** LLM/Command-origin recommendations recorded only
   `engine:<analyzer>` as creator, so the principal who triggered the LLM
   run could approve its own output. Runs now record the triggering
   principal as co-creator on every non-builtin recommendation and review
   blocks against both. The CAL executor inherits this for free: `RUN LOOP`
   passes the session principal as `triggering_actor`.
3. **Dependency seam:** `areev-cal` cannot depend on `areev-loop`. Wire via a
   `GovernanceHost` callback trait on `CalExecutorConfig`; `areev-loop-adapter`
   implements it; absent handler → `Ok(Unsupported)` (existing convention).

**Gate-preservation countermeasures:** double enforcement (executor verb
check + engine ScopeSet re-check); governance statements refused inside
`BATCH` (no one-round-trip approve+apply macro — deliberate friction
preserved), inside `proposal_cal` execution (no self-licensing recursion),
and inside saved-query bodies; destructive apply re-keys to `loop.apply` +
`delete`/`erase` (the two-key property, verb-shaped); optional two-person
mode (reviewer ≠ applier per memory) as an enterprise knob, off by default.

**Honest residual risks:** owner-default solo surfaces are self-attestation
(identical to today's CLI); BECAUSE quality is unenforceable (non-empty
only); a properly-granted but prompt-injected reviewer agent still approves
badly — **RBAC bounds blast radius; it does not supply judgment.**

### 6.5 The Tier-2 audit trail (new design piece)

Loop transitions already write hash-chained audit Observations; destructive
CAL statements today write nothing — `FORGET <hash>` tombstones and returns.
That gap would make the §10 headline demo ("RECALL the audit grain showing
who granted and who deleted") a promise nothing keeps. The rule: **every
Tier-2 execution writes an audit Observation** carrying the session
principal, the verb, the target (hash / subject / age window), and the
reason (BECAUSE where given). It rides the same grain machinery as the loop's
audit chain, syncs with the file, and is RECALLable — completing the §7
narrative's "the audit trail sees it" sentence. Erasure keeps its existing
`ErasureReport`; the Observation references it.

## 7. Spec strategy

- **CAL 1.3** (pillar restatement + grammar growth + tiers; minor version per
  D6) paired with **OMS 1.6**
  (authorization §12.6, grant-replication rule, one-way-FORGET language,
  `proposal_cal` MAY carry Tier-2 with erasure recorded non-rollbackable).
- Published **first as an RFC draft with a comment window** — a draft is a
  shipped artifact; announcing it present-tense is honest and stakes the
  first-mover claim before a line of Rust exists.
- The pillar changes **loudly, not quietly** — CHANGELOG narrative:

  > 1.0–1.2 promised "non-destructive by grammar; no unsafe mode." The
  > promise was true and had a hidden cost: real deployments still needed
  > erasure — GDPR, retention — so destruction happened anyway, outside the
  > language, ungoverned by any spec (Areev's own documented deviation,
  > `docs/erasure.md`, is the evidence). Exiling destruction never prevented
  > it; it only prevented *specifying* it. 1.3 brings it inside, where
  > grammar bounds its shape (tombstone and crypto-erasure only), grants
  > gate it per principal, and the audit trail sees it. Every 1.0–1.2
  > document remains valid Tier-0/1 CAL; for any session without grants, CAL
  > is still exactly the language those releases promised — now the shared
  > floor, not a ceiling that pushed dangerous operations off the books.

- First-mover claim that survives scrutiny: **"agent memory's first language
  with grants in the grammar."** Never "first database language with access
  control" (SQL, Redis ACLs, Cypher admin commands falsify it). Positioning
  sentence: *platforms put RBAC in the dashboard; CAL puts it in the language
  the agent actually speaks.*

## 8. Confusion register

From the audit — thirteen items; each is either resolved by this design or
fixed directly:

| # | Confusion | Resolution |
|---|---|---|
| 1 | Spec says CAL cannot delete; shipping Areev has FORGET on by default | CAL 1.3 owns the model (§7) — the single worst credibility gap today |
| 2 | Tier vocabulary three-way inconsistent (spec 0/1, code invents "Tier 2", cal-reference avoids it) | One tier table (§4), in the spec, matching shipped defaults |
| 3 | `enable_writes` vs `allow_destructive_ops` overlap | Subsumed by verbs; both become deprecated process-wide caps |
| 4 | Destructive FORGET needs no reason; safe REVERT demands BECAUSE | BECAUSE mandatory on every *new* Tier-2/-3 statement; optional-but-recorded on the existing `FORGET <hash>` (a hard requirement there would break a legal statement — the P2 zero-behavior-change gate wins) |
| 5 | FORGET USER/SCOPE + PURGE: AST/executor/facade stubs exist, parser refuses — shipped limbo | Wired under grants (Wave 1) |
| 6 | Flagship loop invisible to CAL; lifecycle scattered over 4 surfaces with 3 naming schemes | Governance statements (§6) |
| 7 | MCP tool count wrong in two docs (11/8 vs actual 13) | **Fixed 2026-08-09** (CLAUDE.md, mcp-reference.md + 5 missing tool sections); count-pinning test to add |
| 8 | Server auth is three-and-a-half models | Collapses to token→principal→grants; anonymous = reader (§5.2) |
| 9 | DESCRIBE advertises `recommendation`, ADD refuses | "Writable by host?" column in cal-reference type table (Wave-1 docs) |
| 10 | Grammar accepts more than the engine does (inert WITH options) | Document "parses ≠ supported" + `DESCRIBE CAPABILITIES` prominently (warnings already reach bindings post-#68) |
| 11 | Recording tool calls via raw `ADD tool` silently dedups retries — #66's bug survives on the CAL path | Short-term doc warning; Wave 3: occurrence semantics in ADD |
| 12 | Erasure is a "documented OMS deviation" on host surfaces only | Wave 1 + CAL 1.3 retire the deviation |
| 13 | Py/JS binding drift (Python has 5 methods Node lacks) | Parity pass per the areev-bindings playbook, scheduled with Wave 1 |

Engineering hard rule surfaced by the audit: `areev-server`'s
`cal_body_is_read_only` (`lib.rs:318`) is a second, independent classifier of
CAL destructiveness — **statement classification must have one source of
truth exported from `areev-cal`** before any grammar growth, or read-only
mode silently misclassifies new statements.

**Surface-consolidation policy** (without it, confusion #6 only half-dies):
after governance enters CAL, every loop action has two spellings on every
surface (`/api/loop/approve` *and* `/api/cal` APPROVE; `areev_loop_adapter` *and*
`areev_cal`; `areev loop approve` *and* `areev cal`). The policy: **CAL is
canonical; existing endpoints stay as documented sugar over it; no *new*
bespoke endpoints for anything CAL can express.** Otherwise "all you need"
decays into "all you need, plus the other spellings we keep adding."

## 9. Positioning guardrails

- README/quickstart mention no principals, no RBAC — owner-default makes
  that the literal truth of the default experience, not an omission. The
  words "Tier" and "conformance" **never appear in the README or
  quickstart** either — necessary spec apparatus, enterprise smell in dev
  docs (the OMS-demotion lesson applies verbatim).
- **`docs/cal-for-llms.md` ships with Wave 1**: a one-page CAL grammar card
  designed to paste into a system prompt. The adopter's end user is an LLM;
  nobody in the space ships this artifact, and it makes "your agent speaks
  CAL" literal.
- **A hosted read-only demo console** ("try CAL in the browser") rides P5 —
  the server + `--no-destructive-ops` + anonymous=reader make it nearly
  free, and it is the try-before-install path SQLite-class tools win with.
- `cal-reference.md` ordered Core → Evolve → Destroy → Control; the GRANT
  chapter opens: "you need this page the day a second principal touches your
  memory — not before."
- `AUT-E001` messages name the missing verb, the resource, and the GRANT
  statement that would fix it — the error is the on-ramp, for humans and
  LLMs alike.
- The existing wedge (self-improving agents + importers) stays primary.
  "CAL is all you need" is the second, trust-shaped wedge: **the loop is why
  you stay; the language is why you can trust it.**
- Standing rule holds: nothing unshipped in present tense.

## 10. Phasing

| Phase | Deliverable | Gate |
|---|---|---|
| **S0 — Spec draft** | CAL 1.3 + OMS 1.6 RFC published in the OMS repo *as a draft*, comment window; announce the draft | Pillar narrative + tier table reviewed; grant vocabulary confirmed already-specified |
| **P1 — Core** | `authz` module (AuthzSet from PERMISSION heads, credential-map loader, `AUT` codes); statement-classification single source; confusion quick wins #9/#10/#11 docs *(the self-approval co-creator bug fix already shipped, `b3d9a2c`)* | Unit + fail-closed tests; classification exported and consumed by server |
| **P2 — Plumbing** | Principal threading everywhere, owner default, old flags → caps | **Zero-behavior-change gate**: full suite green with no credentials configured |
| **P3 — Enforcement** | Verb checks per surface; loop verb mapping; ObserverType from principal; real actors in audit grains | Role × verb × surface matrix test; separation-of-duties suite green under restricted sets |
| **P4 — Wave 1 grammar** | GRANT/REVOKE/SHOW GRANTS/DESCRIBE PRINCIPAL (DEFINE ROLE only if its D9 design note is approved by then); APPROVE/REJECT/APPLY/ROLLBACK/RUN LOOP/DESCRIBE LOOP-family; FORGET SUBJECT/PURGE wired; **REMEMBER**; the Tier-2 audit Observation (§6.5); BECAUSE per the §3 rule; `docs/cal-for-llms.md` | **Spec 1.3 merged first** (invariant 4); **cross-surface parity gate** ("every governed operation has a CAL spelling") in CI; parser golden + `humanize` arm + read-only-classification tests per new statement; conformance cases both backends |
| **P5 — Surfaces** | Credential-map ui/hub (anonymous=reader), per-memory hub scoping via each file's grants, `--as/--auth`, binding params, console; the hosted read-only demo console | Enterprise §3.1 gate list; hub token scoped to A gets 403 on B |
| **P6 — Wave 2 reads** | `ENTITY … AT`, run↔memory join statements, reverse provenance, first-class fork listing, DESCRIBE STATS/INTEGRITY | Each statement lands with its spec addendum |
| **Headline** | The announcement + a 60-second **braided** proof, one paste end to end: `REMEMBER` a conversation → the loop finds a contradiction → `APPROVE … BECAUSE` → `ENTITY … AT` shows the fact time-travel → `GRANT` scopes an agent → the agent's `FORGET` bounces with `AUT-E001` → `RECALL` the audit grain showing who granted and who deleted. Post title is concrete ("agent memory with GRANT and APPROVE in the query language"), not the tagline — "CAL is all you need" lives in the docs, where the boundary sentence sits next to it; show, don't claim | **After P4+P5+P6 green (D10)** |
| **P7 — Wave 3** | MERGE, graph-walk options, ADD-tool occurrence stamping, novelty | Normal releases; each with its spec addendum |

## 11. What we deliberately will not do

- No `DELETE`-by-predicate, no `FORGET WHERE`, no blob mutation — destruction
  takes a hash, an identity, or an age.
- No `UNFORGET` (D7).
- No credentials, tokens, or keys in CAL — no `CREATE TOKEN`, no key
  statements, crypto-erasure stays host (keys are not grains).
- No loop-policy writes via CAL (self-licensing).
- No row/grain-level ACLs; no `WITH GRANT OPTION` in v1.
- No new dependencies.

## 12. Open questions

Four of the original five were decided 2026-08-09 and are now D6 (CAL 1.3),
D8 (erase admin-grantable), D9 (roles plan-first), D10 (headline after
Wave 1 + Wave 2). Remaining:

1. *(minor, default no)* Reserve a dedicated Grant grain type byte (0x0D) in
   the same spec PR, vs riding `Fact + mg:permits` until perf proves the
   need (recommended: ride the Fact).
2. The D9 roles design note — to be written and approved before P4 if roles
   are wanted in v1.
3. *(deferred with roles)* `WITH expires(...)` on GRANT — time-dependent
   authorization needs cache-expiry and determinism semantics before it
   earns grammar; cut from the v1 sketch.
