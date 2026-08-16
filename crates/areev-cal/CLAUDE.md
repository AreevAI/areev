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
   grant decides, and `CalExecutorConfig::allow_destructive_ops` (**default
   true**; `--no-destructive-ops`) is a process-wide restrictive **cap** over
   any grant. Capped/ungranted → `Ok(Unsupported)`.
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
statements are refused there regardless of the gate). `cal_forget_scope`
remains an unwired stub.

Security invariants in the lexer: **S-1** bidi-control rejection
(`check_bidi`, U+202A–202E / U+2066–2069) and **S-6** NFC normalization —
both run before tokenization; `compute_query_hash` NFC-normalizes again for
the audit hash.

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
- `assemble.rs` — `AssembleEngine`: multi-source ASSEMBLE, dedup, 2000-grain
  cap, per-source budget weights, chars/4 token estimate.
- `render.rs` — THE per-grain renderer every surface shares: semantic
  `sml`, the documented `markdown` assertion line, `text`, registry-driven
  `toon`, the `json` envelope, per-type summaries, and the one `chars/4`
  token estimator. The executor's `FORMAT` arms and `areev-context`'s
  assembler both call it (parity pinned by areev-context's
  `tests/render_parity.rs`) — never grow a second implementation of a
  format name.
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
