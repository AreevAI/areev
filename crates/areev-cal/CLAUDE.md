# areev-cal

CAL ("Context Assembly Language") — lexer, parser, executor, multi-source
ASSEMBLE, templates, saved queries, and the `AreevFacade` that binds it all
to `areev-store` (~30k lines). CAL syntax is an OMS conformance contract —
**do not invent new CAL syntax** without a spec-level decision.

`executor.rs` (~10k lines) and `parser.rs` (~9.4k lines) are the two biggest
files in the repo — navigate with grep and offset reads, never full reads.

## Pipeline

text → `parse()` (parser.rs:91): length check → bidi rejection → NFC → lex →
recursive-descent parse → `CalQuery` AST → `CalExecutor::execute`
(executor.rs): LET eval → `execute_statement` (big match) → `apply_pipeline`
→ `apply_format_clause` → `CalResultPayload`.

**WHERE fails closed (#91, 1.5.1).** `plan_residual_where` (executor.rs)
splits every recall-family WHERE tree: leaves the push-down consumes
(`leaf_pushdown_consumed`, a test-pinned truth table that must stay in sync
with `apply_where_clause`'s arms) become engine params; everything else —
type-specific fields, NOT/OR subtrees, unsupported comparators, IS NULL —
survives as a residual tree evaluated per grain by
`grain_matches_condition_tree` (the ONE boolean evaluator). Validation runs
before the scan: a field the target type cannot carry is CAL-E060; an
engine-level field (`ENGINE_ONLY_FIELDS`: query/time/entity/contradicted/
scope/scope_path/tags) in a position it cannot be honoured is CAL-E061. A
filter is pushed, evaluated, or refused — NEVER dropped. If you add a
push-down arm, extend the truth table and its pin test in the same change.

**…and absence is UNKNOWN, not FALSE (#207, 1.7.4).** The per-grain walk is
three-valued (`grain_condition_truth`): a leaf whose field the grain does not
carry is UNKNOWN, `AND`/`OR` combine by SQL's truth tables, `NOT UNKNOWN` is
UNKNOWN, and `grain_matches_condition_tree` treats UNKNOWN as no-match. Two-
valued evaluation cannot express "fails closed" under negation — reading
absence as `false` made every negation of it `true`, so `object != "retired"`,
`NOT (object = "retired")` and `object NOT IN ("retired")` each matched every
grain of a type that has no `object`. Only negation moved; `T ∧ U` and `F ∧ U`
already collapsed to no-match and `T ∨ U` already matched. `resolve_grain_field`
is the ONE answer to "does this grain carry the field" — envelope properties
(`hash`, `type`, `score`) and the omit-default discriminators (`kind`,
`status` on tools) resolve there, so a materialized default reads as *present*
and `kind != "definition"` keeps matching legacy execution grains.

Note the shared-evaluator hazard: `areev-trigger` uses the same tree for
composite gates, where an absent field means "this member has not fired" — a
definite FALSE, not UNKNOWN. `gate_satisfied` therefore materializes every
`referenced_members` name rather than projecting only the fired ones.

**Summarise / extract / navigate (#209/#210/#211, 1.7.4).** Three additions,
one spec decision, recorded in
[`docs/oms-1.7-amendments-cal-expressiveness.md`](../../docs/oms-1.7-amendments-cal-expressiveness.md):

- `GROUP BY <field>` then `COUNT` projects one row per group
  (`CalResultPayload::GroupCounts`, most frequent first, ties by key asc). The
  rows are `CalGrainResult`s with `grain_type "group"`, fields `{key, count}`
  and an **empty hash** — a group is computed, not stored, and anything keying
  on a content address must skip it. **No new syntax**: that combination used
  to answer the plain total, which is `COUNT` with extra words. Rendered via
  the `group.` template namespace (`GROUP_VARIABLES`, closed at `key`/`count`,
  bound only on a group row).
- `KNOWN_FILTERS` grows by seven: six extractors and `get` (a dotted JSON
  path). The list stays **closed** — `DESCRIBE CAPABILITIES` reports it. Bad
  arguments are refused in `parse_single_filter` (define time, `CAL-E049`), not
  at render time, so rendering stays total and a saved template stays readable.
  `match` compiles through `cached_pattern` (bounded LRU, `regex` — a
  finite-automaton engine, so no backtracking and therefore no backreferences
  or lookaround). Bounds: `MAX_EXTRACT_INPUT`, `MAX_PATTERN_LEN`,
  `MAX_GET_PATH_DEPTH`.
- `WHERE` accepts a dotted field path up to `MAX_FIELD_PATH_SEGMENTS` (8),
  resolved in `resolve_grain_field`. A field holding JSON **as a string**
  navigates identically to a parsed one. An unresolvable path is `None` →
  UNKNOWN → no match, so navigation inherits the fails-closed rule above
  rather than adding one. Keep the three depth constants (parser, executor,
  templates) equal: a path that parses must be one the executor walks and a
  template can express.

An `ASSEMBLE` **source** now carries its own `pipeline` (`NamedSource`), run in
`execute_source` before dedup and budgeting — that is what lets a frequency
roll-up be a prompt section. `extract_grains` lives in `executor.rs` and is
`pub(crate)`; `assemble.rs` used to keep a second copy that had drifted (it saw
only `Grains`, so a nested `Assembled` contributed nothing). One extractor.

**World-time validity is queryable (#206, 1.7.4).** `valid_from`/`valid_to`/
`system_valid_from`/`system_valid_to` are `GrainCommon` fields on every type,
already serialized and expanded back into `fields` — they were simply missing
from `COMMON_FIELDS` and `GRAIN_EVALUABLE_COMMON`, so the one read that makes a
validity window worth writing refused with CAL-E060. No push-down: they
post-filter over the widened scan like any other in-blob key.

**LET eval writes its results onto `CalQuery::let_values`** (`#[serde(skip)]` —
execution state, not query text); `apply_where_clause` expands `IN $var` from
it, and surrogate/nested queries plus ASSEMBLE sources inherit it. The scope
used to be evaluated and dropped, so `$var` never reached any WHERE clause.

Two entry points must stay in sync: `execute` (text) and `execute_parsed`
(JSON-CAL AST) duplicate the LET/pipeline/format sequence — including filling
`let_values`.

## The safety pillar: destruction is shaped and authorization-gated

Destruction takes **a hash, an identity, or an age — never a predicate**
(CAL 1.3). Three statements: `FORGET <hash>` (single-grain tombstone →
`Areev::forget`), `FORGET SUBJECT "<id>" [WITH text_mentions]` (identity
erasure → `forget_subject_with`), `PURGE OLDER THAN <n><d|h|m> [TYPE t]
[IN "<ns>"]` (retention sweep → `forget_older_than`). BECAUSE is mandatory
on the latter two, optional-but-recorded on the hash form.
1. **Lexer**: `is_destructive_keyword` (lexer.rs) hard-blocks DELETE, ERASE,
   INSERT, CREATE, … — DELETE has no token at all.
2. **Parser**: `parse_statement` fast-rejects those idents with CAL-E002.
   `FORGET USER/SCOPE` are refused from text with a pointer to SUBJECT.
   `DROP` accepts only TEMPLATE/QUERY.
3. **Authorization**: the session's `delete` (hash) / `erase` (subject, age)
   grant decides — plus `admin` on the namespace for `WITH override_hold`
   (#278) — and `CalExecutorConfig::allow_destructive_ops` (**default
   true**; `--no-destructive-ops`) is a process-wide restrictive **cap** over
   any grant. Capped/ungranted → `Ok(Unsupported)`.
3b. **Legal holds** (#278): a namespace under a hold refuses every form with
   `STO-E009`, and the facade records the deferral (`erase.refused` /
   `delete.refused`). `WITH override_hold BECAUSE "…"` takes the audited
   override path (`cal_delete_overriding` / `cal_forget_user_overriding`,
   both defaulted on the trait to a REFUSAL so a host facade that has not
   implemented the override cannot silently perform one); the Tier-2 record
   carries `context.hold_overridden`.
4. **Audit**: every execution writes a Tier-2 Observation in `agent:authz`
   via `areev_core::authz::audit_observation` — the one builder every
   surface shares. Subject erasures record a **fingerprint**
   (`subject_fingerprint`), never the identity: the audit grain is immutable
   and replicates, so a raw identifier there would undo the erasure it
   records.
5. **Classification**: `classify.rs` is the single source of truth
   (exhaustive, no wildcard). `REPORT SUBJECT` — the read-only DSAR mirror of
   `FORGET SUBJECT` — classifies `Read` and is `read`-gated, deliberately
   NOT behind the destructive cap.
Saved-query bodies get an extra `check_statement_read_only` pass (destructive
statements are refused there regardless of the gate), and `validate_query_body`
also **parses** the body at DEFINE. It used to stop at the word-level keyword
scan whenever the body contained `$` — i.e. for most saved queries — so any
syntax error was stored and first surfaced at RUN. The reason for that skip was
real but narrow (a parameter in a numeric position like `RECENT $limit` is not a
literal until RUN substitutes it), so the check parses the body as written and,
on failure, retries with `params_as_literals` standing the parameters in; only a
body that fails BOTH is refused (`CAL-E059`). The reported error is always the
one from the body as the author wrote it — the placeholder form is an internal
probe whose spans would point at text nobody typed. `cal_forget_scope`
remains an unwired stub.

Security invariants in the lexer: **S-1** bidi-control rejection
(`check_bidi`, U+202A–202E / U+2066–2069) and **S-6** NFC normalization —
both run before tokenization; `compute_query_hash` NFC-normalizes again for
the audit hash.

## Multi-principal reads: `PrincipalSession` IS a facade (#302)

`bind_principal` swaps ONE process-wide rights slot; its own doc says it is
safe only for hosts that serialize requests. `principal_session(p)` is the
race-free path, and since 1.9.0 it implements `CalStoreFacade`, so
`CalExecutor::execute(cal, &session)` runs any statement — read or write —
under the session's own fail-closed `AuthzSet`. Before, only writes were
per-principal; every gated read went to the shared slot.

Mechanism: a **thread-local scope**, keyed by facade identity, installed for
the duration of one call by a `SessionScope` guard that pops on drop (so a
refusal or a panic cannot leave one principal's rights installed for the next
call on this thread). `AreevFacade::rights()` is the single read every
`check_verb` goes through, and `default_ns()` the single read every
namespace-defaulting statement goes through — which is what makes
`in_namespace` work for `RELATED` / `ENTITY … AT` / `NOVELTY`.

The delegating impl covers every method the trait does NOT default (52 of
88). That is the safe half of the split, and it is worth knowing why rather
than trusting the count: the trait's 54 defaults are fail-closed refusals
("destructive operations not available"), so a method added to
`CalStoreFacade` later and left undelegated here REFUSES for a session
rather than falling through to the shared slot. The compiler will not catch
the omission — a defaulted method compiles — but the failure mode is a
refusal, not a leak. When you add one, delegate it, and check its default is
still a refusal before relying on that.

`as_any()` returns `None` — a downcast to `AreevFacade` would hand the caller
the UNSCOPED facade.

Three additions in 1.9.1 (#324), all for a host serving many principals:
`resolve_rights(p)` returns the fail-closed `AuthzSet` and `session_with(set)`
builds the borrowed session from it for free — so a host caches the SET by
`(principal, authz_epoch)` instead of re-reading grants under the store mutex
per request, and `principal_session` is now their composition. The set is a
snapshot by design (a later `set_grants` does not reach it; the epoch moves so
a cache can notice). `PrincipalSession::add(&grain)` is the typed attributed
write — same check, same `author_did`, grain builders instead of a JSON field
map; it REFUSES under an anonymization ingress policy rather than bypassing the
boundary `cal_add` routes through, because a new API must not be the quiet way
past a privacy control. `AuthzSet::namespaces(verb)` (areev-core) answers
"which namespaces" as `All` or `Exact` rather than forcing a probe per
namespace.

## Store access from a caller-facing surface is GATED (GHSA-rmrx-26f6-f97w)

`with_store` hands out the raw store with **no `AuthzSet` check**. That is
correct for a host acting under its own authority (areev-run journalling its
own run, areev-trigger evaluating its own cadence — between them the bulk of
its ~600 call sites) and WRONG for anything a bound principal can reach.

Until 1.9.1 the bindings and two MCP tools used it for nearly everything, so
`principal=` restricted CAL and almost nothing else: a read-only principal
could read any namespace, write one through `remember()`, and erase one through
the memory tool's `delete`. Use `store_read(ns, f)` / `store_write(ns, f)` /
`store_checked(verb, ns, f)` / `store_as(verb, ns, f)` on any such surface —
they check `rights()` (the session-aware read, exposed publicly as
`effective_authz()`) before the closure sees the store. `"*"` is the
memory-wide resource, the convention the gated CAL methods already used.

`tests/host_surface_gating.rs` fails the build on a new ungated `with_store` in
areev-py / areev-js / areev-mcp unless the store method is named there with a
reason — that test, not discipline, is what keeps the hole closed.

## `AsyncFacade`: the async-safe owner (#322)

`areev_store::AsyncAreev` wraps the raw store only, so an async host that also
needed authorization had none and hand-rolled it. `AsyncFacade` (src/
`async_facade.rs`) is the governed equivalent: `open` / `with` / `with_mut` /
`from_facade` / `close`, every call on the blocking pool, teardown off the
executor.

It takes a **closure**, not one async method per facade method, and that is
forced rather than chosen: `PrincipalSession<'f>` borrows its facade and
therefore cannot cross an `.await`. Inside `with`, a whole request — resolve
the principal, run its statements — happens on one blocking thread.

`tokio` is a direct dependency here for this and adds NO crate to the graph:
areev-store already depends on it with the same features.

## Module map

- `lexer.rs` — Logos DFA, S-1/S-6, destructive-keyword list.
- `ast.rs` — `CalStatement` (22 variants), `PipelineStage`, `Condition`,
  `WithOption` (~35 recall flags), FORMAT clause.
- `parser.rs` — hand-written recursive descent. Hard limits are consts at the
  top (~line 52): MAX_QUERY_LENGTH 64KB, MAX_NESTING_DEPTH 8, MAX_LIMIT 1000,
  MAX_PIPELINE_STAGES 5. Condition precedence via layered fns
  (`parse_condition_or` → `_and` → `_unary` → `_primary`).
- `executor.rs` — `CalExecutor`, per-statement executors (`execute_recall`,
  `execute_assemble`, …), pipeline + format application.
- `facade.rs` — `CalStoreFacade` **trait** (object-safe): the executor's only
  store access. Tier-2 destructive methods default to Err.
- `areev_facade.rs` — concrete `AreevFacade` over `areev_store::Areev`
  (Mutex-wrapped). `with_session(store, ns, user)` = session scoping.
  **Read-only mounts**: `mount(alias, store)`; `recall` routes
  `"alias.inner"` namespaces to the mount — writes only ever hit the session
  store, so mounts are read-only by construction.
  **Recall scores + host recall config** (decision-backend phase 0): the
  hybrid arms call the store's `recall_hybrid_scored*`, so `SearchHit.score`
  is the normalized fused score (top = 1.0) or the normalized reranker score;
  the unranked scans and any query-less recall carry the 1.0 sentinel (the
  executor's `is_deterministic` contract). `min_score` on a `RECALL … ABOUT`
  filters on it (no-ABOUT → `CAL-W014`); `recency_weight` stays RANK-based on
  purpose (sentinel paths + compressed RRF would skew the blend).
  `set_reranker(Box<dyn RerankBackend>)` installs on the primary store only
  (mounts keep their own); `set_recall_deadline(Option<Duration>)` is threaded
  into EVERY hybrid call — first pass, each `subject IN` leg, each
  `multi_hop` leg. `AsyncFacade` reaches both through `with_mut`. E2E:
  `tests/recall_score_tests.rs`.
  **Decision backend** (phase 3): `set_decider(Arc<dyn DecisionBackend>)` /
  `clear_decider()` / `decider()`, surfaced to the executor as the defaulted
  trait method `CalStoreFacade::decider()` (default `None`; `PrincipalSession`
  delegates — it is host config, not data). Only multi-source ASSEMBLE reads
  it; RECALL is untouched (its ranking is the reranker's).
  **Namespace scope resolution** lives at the top of `recall`: the scope
  terms are `params.namespaces` (the `IN` set — every member queried, issue
  #19) else `params.namespace` else the session default; each term may be
  exact, a `"org.*"` prefix (expanded via the store's namespace registry), or
  mount-routed (pattern allowed in the inner part). A set spanning mounts
  refuses with a pointer at ASSEMBLE. Under a bound principal the expansion
  fails closed per covered namespace, and the refusal names the pattern —
  never a discovered namespace. A `namespace_override` pin clears any
  caller-supplied scope (all three executor application sites). Grants refuse
  `*`-bearing namespaces except `*` itself (`parse_grant_parts`). E2E:
  `tests/ns_scope_cal_tests.rs`.
- `assemble.rs` — `AssembleEngine`: multi-source ASSEMBLE, dedup, 2000-grain
  cap, per-source budget weights, chars/4 token estimate. **The budget applies
  written or not** (`DEFAULT_BUDGET_TOKENS` 4000, parser ceiling 16000), and a
  budget that drops grains emits `CAL-W017` naming the sources, the counts, and
  whether the default applied (#208). `total_available` is the PRE-budget
  count — reporting the trimmed one made a truncated assembly arithmetically
  indistinguishable from a complete one. If you add a path that discards grains
  here, it warns or it is the same bug again.
  With a decider, a non-pinned source whose sub-query is `RECALL … ABOUT`
  is judged (`judge::judge_candidates`); `budget_by_relevance` spends the
  source's share highest-relevance first (same stop rule as `budget_prefix`,
  survivors in recall order), and a CALIBRATED backend first drops
  `relevance < judge::DEFAULT_DROP_BELOW` into `omitted` with `CAL-W019`.
  Any backend failure → `budget_prefix`, unchanged. Pins/literals/no-ABOUT
  sources are never judged. `select_tier` itself is unchanged — it sees the
  post-drop grain count. E2E: `tests/assemble_decide_tests.rs`.
- `judge.rs` — the decision-backend questions for context assembly (rows
  A2/A3): `judge_candidates` (per candidate `rel_n` score over
  `RELEVANCE_LEVELS` + `verbatim_n` noul, split under `MAX_STATE_TOKENS`,
  all-or-nothing, one deadline for all requests), `judge_intent` (one choice:
  timeline / current_state / general), `JudgeProvenance`. ONE home for the
  wording — areev-context's allocator and ASSEMBLE both call it. It asks and
  validates; callers decide what to omit, and only on a calibrated answer.
- `render.rs` — THE per-grain renderer every surface shares: semantic
  `sml`, the documented `markdown` assertion line, `text`, registry-driven
  `toon`, the `json` envelope, per-type summaries, and the one `chars/4`
  token estimator. The executor's `FORMAT` arms and `areev-context`'s
  assembler both call it (parity pinned by areev-context's
  `tests/render_parity.rs`) — never grow a second implementation of a
  format name.
  **`Disclosure`** (OMS §4 `WITH progressive_disclosure`) is the body axis,
  orthogonal to `MetadataDetail`'s envelope axis: `summary`/`headlines` clip
  free-text bodies (40/80 chars, the same ladder `templates::effective_truncate`
  uses), `full` leaves them whole AND emits the long-form definition bodies no
  other tier carries — a Skill's `instructions`/`when_to_use`, which otherwise
  reach no rendered path at all. `None` (nothing requested) is the historical
  render, byte for byte; the `*_at` entry points take the tier and the bare
  `render_grain_sml`/`render_grain_markdown` delegate with `None`, which is what
  keeps render parity honest. Gating the definition body behind `full` is
  deliberate: a recall of twenty skills must stay a listing, not twenty
  playbooks.
- `templates.rs` — Mustache-subset engine (closed variable set, 10 filters,
  F1–F7 security invariants, 1MB output cap). Builtins are exactly the three
  §10.1 sectioned presets (`structured`/`readable`/`compact`); a builtin must
  never take a `FORMAT` arm name (debug-asserted). Budgeted template renders
  pick their `DisclosureTier` via `select_tier` (wired in the executor's
  `template_tier`), which is what makes `ELEMENT_SUMMARY`/`ELEMENT_OMIT`
  fire. `queries.rs` — saved queries (100/namespace, 8KB body cap).
- `store_types.rs` — the areev-store contract: `RecallParams`, `SearchHit`,
  `AddOptions`, etc. Facade methods speak exclusively in these types.
- `errors.rs` — `CalError` (thiserror); **CAL-Exxx codes live inside the
  `#[error]` display strings**, not a separate code fn. E001–E019 parse,
  E020–E022 type, E030+ exec.

## Adding a language feature (touch in this order)

lexer.rs (token) → ast.rs (variant) → parser.rs (parse fn + dispatch) →
executor.rs (payload variant + match arm + executor fn) → errors.rs (new
CAL-Exxx) → facade.rs trait + areev_facade.rs impl (if store access) →
json.rs (wire form) → store_types.rs (if the store contract grows) → tests →
`CalCapabilities::default` supported_statements list.

## Gotchas

- `CalResultPayload::Unsupported` is returned as **Ok** for Tier-1 runtime
  failures (bad grain type, unresolved param) — check the payload, not just
  Ok/Err.
- REVERT exists in the AST/facade/executor but always returns Unsupported
  from text, and `cal_forget_scope` is an unwired stub. AST coverage ≠
  reachable surface.
- ADD requires a `REASON`/`BECAUSE` clause (missing → CAL-E018) and uses
  repeated `SET field = value`.
- Many keywords double as field names (ON, WHEN, PRIORITY, SCOPE) via
  `is_word_token` — extensive tests guard this; keep them green.
- The `cal` cargo feature is default-on and always enabled here (gates
  alias normalization + DESCRIBE capability listing).

## Tests

`cargo test -p areev-cal` (~700 inline unit tests in parser/executor/lexer/
assemble). `tests/cal_integration.rs` = text → executor → facade → real store
end-to-end incl. destructive-reject; `tests/assemble_mount_tests.rs` =
multi-source ASSEMBLE across a mounted org replica;
`tests/docs_examples.rs` parses **every** ```sql fence in
`docs/cal-reference.md` and cross-checks §4's pipeline-stage table against the
parser's own error list — the reference is executable, so a documented query
that does not parse fails CI instead of a user's first session.

Filter tests must assert what a clause **excludes**. A test that only checks
"the expected row is present" passes against a filter that is ignored
entirely — which is how `WHERE … IN` reached a release doing nothing.
