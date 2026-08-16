---
name: areev-cal-feature
description: Playbook for extending CAL (Context Assembly Language) in areev-cal — the ordered chain of files to touch, the OMS-conformance gate on new syntax, preserving the three-layer destructive-op block, and the Ok(Unsupported) result convention. Use before adding a CAL keyword, statement, WITH option, pipeline stage, or format clause, or editing lexer.rs / parser.rs / executor.rs.
---

# Extending CAL

`areev-cal` is ~30k lines; `executor.rs` (~10k) and `parser.rs` (~9.4k) are the
two biggest files in the repo — **navigate with grep + offset reads, never full
reads**. CAL is an OMS conformance contract, and its destructive surface is
narrow and gated (only `FORGET <hash>`, off-switchable); both are load-bearing.

## GATE 1 — is this new *syntax*?

New CAL **syntax** (a keyword, statement form, operator) is an **OMS
conformance decision**, not a local change. **STOP** and confirm it is spec-level
approved before writing a token. Adding behavior behind *existing* syntax — a new
`WITH` option value, a new pipeline behavior, a new FORMAT target — is the
normal, safe case and does not need a spec change.

## GATE 2 — keep the destructive surface narrow and gated

The destructive CAL statements (CAL 1.3) are `FORGET <hash>`, `FORGET SUBJECT
"<id>" [WITH text_mentions]`, and `PURGE OLDER THAN <n><d|h|m> [TYPE t]` —
authorization-gated by the session's grants (BECAUSE mandatory on the latter
two, a Tier-2 audit Observation per execution), with
`CalExecutorConfig::allow_destructive_ops` as a process-wide restrictive cap
(`--no-destructive-ops`). Destruction takes a hash, an identity, or an age —
**never a predicate**. Preserve the surrounding blocks:
1. **Lexer** (`lexer.rs`): `is_destructive_keyword` hard-blocks
   DELETE/ERASE/INSERT/CREATE/… ; DELETE has no token at all.
2. **Parser** (`parser.rs`): `parse_statement` fast-rejects those idents with
   CAL-E002; `FORGET USER/SCOPE` stay refused from text with a pointer.
3. **DROP** is a token but `parse_drop` accepts only DROP TEMPLATE/QUERY.
   Saved-query bodies get an extra `check_statement_read_only` pass.
4. **Classification** has one source of truth: `areev_cal::classify`
   (exhaustive, no wildcard) — a new statement variant fails its build until
   someone decides its class.
Do **not** widen this (predicate-shaped destruction, user/scope erasure, new
destructive verbs) without a design + OMS-conformance decision — that's a
GATE 1 change.
Also preserve the lexer security invariants: **S-1** bidi-control rejection and
**S-6** NFC normalization, both before tokenization.

## Touch in this order

1. **`lexer.rs`** — the token (Logos DFA). Not a destructive word.
2. **`ast.rs`** — the `CalStatement`/`PipelineStage`/`WithOption`/`Condition` variant.
3. **`parser.rs`** — the parse fn + dispatch. Respect the const limits at the top
   (~line 52: MAX_QUERY_LENGTH 64KB, MAX_NESTING_DEPTH 8, MAX_LIMIT 1000,
   MAX_PIPELINE_STAGES 5). Many keywords double as field names via `is_word_token`
   (ON/WHEN/PRIORITY/SCOPE) — extensive tests guard this, keep them green.
4. **`executor.rs`** — payload variant + match arm + executor fn. **Two entry
   points must stay in sync**: `execute` (text) and `execute_parsed` (JSON-CAL
   AST) duplicate the LET → pipeline → format sequence — edit both.
5. **`errors.rs`** — a new `CAL-Exxx`. Codes live **inside the `#[error]`
   Display string** (E001–E019 parse, E020–E022 type, E030+ exec); pick the next
   free number in the range and add a row to `ERROR_CODES.md`. Append-only.
6. **`facade.rs`** (object-safe `CalStoreFacade` trait) + **`areev_facade.rs`**
   (concrete `AreevFacade`) — only if the feature needs new store access.
   Tier-2 destructive methods default to `Err`; keep it that way.
7. **`json.rs` / `json_build.rs`** — the JSON-CAL wire form for the new variant.
8. **`store_types.rs`** — if the store contract grows (`RecallParams`,
   `SearchHit`, `AddOptions`, …); facade methods speak only these types.
9. **`CalCapabilities::default`** — add to the supported-statements list so
   DESCRIBE reports it.

## The dominant failure mode — a clause that parses and does nothing

Setting a field on `RecallParams` is **not** wiring a feature up. The struct is
a contract with `areev-store`; if no code downstream reads the field, the
clause parses, the query succeeds, and the option silently has no effect. This
is not hypothetical — `WHERE … IN`, `WITH conflict_resolution`,
`WITH dedup(field)`, `WITH annotate_relative_time` and `WITH explanation` all
shipped this way and were only caught by black-box testing against the docs
(#47, #53).

It is the *worst* failure shape here, because these clauses usually **narrow**
a result set. Failing to apply one fails **open**: the over-broad answer goes
straight into a model's context with no error and no warning.

After adding any filter or `WITH` option, prove the loop is closed:

```bash
grep -rn "my_new_field" crates/areev-store/src crates/areev-cal/src/areev_facade.rs
```

A hit only in `store_types.rs` (the struct, its builder, `has_filters`) means
**nothing consumes it**.

Then pick one:

- **Wire it** — consume it in `AreevFacade::recall` (or the store) and test it.
- **Say it is inert** — push `CalWarning::WithOptionInert` (**`CAL-W014`**),
  naming the option and the statement, and drop it from `DESCRIBE`'s
  `with_options`. §5 of the CAL reference promises an unavailable option is
  honest rather than silently degrading; a warning is the non-breaking form of
  honest for an option that already shipped.

### Tests must assert a filter EXCLUDES

A test that only checks "the row I wanted is present" passes against a filter
that is ignored entirely — which is how `IN` reached a release. Always assert
the rows that must **not** come back, and cover the empty case: an empty `IN`
set selects *nothing*, and an unbound `$var` is `CAL-E008`, because silently
scoping to nothing is as wrong as silently scoping to everything.

## LET bindings resolve onto the query, not through a scope object

`LetScope::evaluate` used to be called and its result **dropped** (`let _scope =
…`, with a comment deferring WHERE resolution to "Phase 3"), so `$friends` never
reached any statement and the documented two-step pattern matched nothing it was
meant to scope by. Resolved bindings now ride on `CalQuery::let_values`
(`#[serde(skip)]` — execution state, not query text), filled in by **both**
`execute` and `execute_parsed`, and inherited by nested/surrogate queries and
ASSEMBLE sources. If you add a construct that consumes `$vars`, read
`query.let_values`; if you build a surrogate `CalQuery`, propagate it.

## Envelope fields are not in `grain.fields`

`hash` (and `grain_type`) are properties *of* the blob, so a post-filter that
looks them up in `fields` always misses — `WHERE hash = "<real address>"`
returned the entire result set with a spurious `CAL-W010`, and `hash IN (…)`
returned nothing. `grain_matches_condition` special-cases them; extend that
list rather than adding another `fields` lookup.

## Gotcha — Unsupported is returned as Ok

Tier-1 runtime failures (bad grain type, unresolved param) come back as
`CalResultPayload::Unsupported` inside an **`Ok`**, not an `Err`. Check the
payload, not just Ok/Err. REVERT exists in the AST/facade but always returns
Unsupported from text, and `cal_forget_scope` is an unwired stub — AST
coverage ≠ reachable surface.

## Verify

```bash
cargo test -p areev-cal
cargo +nightly fuzz run cal_parse         # if you touched lexer/parser
```

- `tests/cal_integration.rs` runs text → executor → facade → real store,
  including destructive-reject assertions — add cases there.
- Keep `test_error_codes_match_display` / `test_all_error_codes_have_unique_codes`
  green if you added a code.
- Then run the **areev-invariants** gate before committing.
