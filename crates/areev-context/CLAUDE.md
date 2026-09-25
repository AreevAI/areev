# areev-context

Budget-aware, provider-optimal **orchestration** of recall results into
model-ready context. Input: `&[SearchHit]` (from areev-cal); output:
`FormattedContext` text in SML / TOON / Markdown / PlainText / JSON.

**This crate renders nothing itself.** Since the render unification, all
per-grain rendering — formats,
summaries, token estimation — lives in `areev_cal::render`, the one
implementation CAL's `FORMAT` arms share; this crate decides *which* grains
make the budget and *how the envelope is shaped*. Byte parity between the two
surfaces is pinned by `tests/render_parity.rs` — never add a second
implementation of a format here.

## Module map

- `policy.rs` — config types: `OutputFormat`, `MetadataLevel`, `Ordering`,
  and the `FormatPolicy` builder (`.metadata/.ordering/.token_budget/
  .group_by_type/.grain_override/.query_text/.grain_type_diversity/
  .include_retracted`). `include_retracted` is off by default: a grain whose
  `verification_status` is `retracted` is **withheld** from assembly — from the
  main body and from the Knowledge Update section, which is built earlier and
  would otherwise leak the withdrawn value as "what changed". `contested` is
  demoted (`-0.15`), never withheld. Withholding lives here and never in
  `areev-store`: `subject_report` (DSAR) shares one selector with erasure and
  must keep disclosing retracted grains — pinned by
  `store_recall_still_returns_retracted_grains` in `areev-conformance`.
  `.decide(DecidePolicy)` tunes an installed decision backend (below);
  `decide: None` with a backend installed means `DecidePolicy::default()`.
- `presets.rs` — `FormatPolicy::claude()` (SML, grouped), `gpt4()` /
  `gemini()` (Markdown), `local_small()` (PlainText), `json_api()` (JSON).
  **Presets never set `token_budget`** — the caller owns that.
- `budget.rs` — `Allocation{Full,Summary,Omit}` and two allocators:
  `allocate()` (pure priority order) and `allocate_with_diversity()`
  (5-phase: group → reserve `min_per_type` → cap trim → Full → fill).
  Progressive disclosure is REAL: Full up to ~70% of budget, degrade to
  Summary up to ~95%, then Omit. `summary_tokens = full/3` heuristic.
  `ScoredEntry` carries two decision inputs (build it with
  `ScoredEntry::new`, which leaves both false): `force_omit` (Omit even with
  no budget; never reserved by the diversity floor) and `prefer_full` (Full
  may use the budget up to the 95% line instead of 70%). Neither pushes past
  95% — the budget rule stays final. There is no third allocator.
- `render.rs` — the `GrainRenderer` trait + `RendererRegistry` (the seam a
  host can override via `ContextAssembler::with_renderer`), one
  `SharedRenderer` per grain type delegating to `areev_cal::render`, and the
  per-type `context_priority` table that feeds allocation (consent 0.95 >
  state 0.9 > goal 0.8 > fact 0.7 > … ; failed tool calls boosted).
- `assembly.rs` — `ContextAssembler` (`format()`, `format_with_hints()`,
  `with_decider()`), `RenderingHints`, `FormattedContext{text,
  estimated_tokens, included_count, omitted_count, truncated, decision}`, and
  `strip_summaries_for_structured_formats` — JSON/TOON get whole entries or
  nothing (a prose summary inside a structured dump would corrupt it).

## Rendering modes

`format_with_hints` picks exactly one mode, in priority order:
aggregation > timeline (chronological; needs ≥2 hits + temporal intent) >
census (80/20 budget split, keyed on `RecallSource::Census`) >
relevance-highlight (>10 grains) > default. **JSON output bypasses all
modes** — it is a plain structured dump.

## Decision backends (decision-backend phase 3, rows A2/A3)

`ContextAssembler::with_decider(Arc<dyn areev_core::decide::DecisionBackend>)`
is host config (per process, never in the file). No backend = today's
behaviour byte for byte (`tests/decide_tests.rs` pins it, alongside the
untouched snapshots and render parity). With one, `format_with_hints` asks —
only when there is a query text (`policy.query_text`, else `hints.query_text`)
and ≥ 2 hits — through `areev_cal::judge`, the ONE home of the question
wording (CAL's ASSEMBLE asks the same A2 questions):

- **A3 intent** — one `choice` (`timeline` / `current_state` / `general`)
  replaces `detect_temporal_intent` and `is_recency_query`. The hint flags
  still win (and then the question is not asked). Timeline is a view choice,
  so an uncalibrated answer may pick it; `current_state` suppresses the old
  value of a Knowledge Update chain — an omission — so only a CALIBRATED
  answer decides recency, else the keyword list does.
- **A2 disclosure** — per candidate `rel_n` (`score` over off-topic /
  tangential / relevant / directly answers) and `verbatim_n` (`noul`: would a
  one-line summary lose a needed detail). Relevance `score/3` replaces the
  recall score as the priority's input (`adjusted_priority`'s 0.15 term, via
  a cloned hit — custom renderers see it too). CALIBRATED only:
  `r < drop_below` → `force_omit`, `p_verbatim ≥ full_above` → `prefer_full`.
  Uncalibrated: reorder only (proposal §2 rule 2). Candidate text is the
  registry's PlainText render capped at 600 chars; retracted/withheld grains
  are never sent; the state is split into several requests past ~28k
  estimated tokens; one `DecidePolicy.deadline_ms` bounds all calls together.
- **Fail open, per question**: any error/malformed answer/spent deadline →
  today's rule for that question. `FormattedContext.decision`
  (`DecisionProvenance{provider, model, calibrated, requests, latency_ms,
  intent}`) is `Some` only when an answer was applied, and is omitted from
  the serialized form otherwise. Decider-omitted grains count in
  `omitted_count` and set `truncated`.

## Provider-optimal means

Format matched to the consuming model: SML tags for Claude (XML-ish),
Markdown for GPT/Gemini, TOON compact tables / JSON for machines, PlainText
for small local models.

## Gotchas

- **Token estimation is `chars / 4`** — a heuristic, no real tokenizer, and
  ONE implementation: `areev_cal::render::estimate_tokens` (the trait's
  `token_estimate` delegates). `estimated_tokens` is approximate; don't
  treat budgets as exact.
- Budget pressure sets `truncated: true` and bumps `omitted_count` — check
  those instead of guessing from output length.
- Summary renders stay format-shaped: SML summaries keep the semantic tag
  (`<goal>…</goal>`, no attrs), Markdown summaries keep the `- ` bullet.
- Unit tests are inline `#[cfg(test)]` per module; `tests/` holds the insta
  snapshots (`snapshot_render.rs`, bless with `INSTA_UPDATE=always`) and the
  cross-surface parity golden (`render_parity.rs`). Run with
  `cargo test -p areev-context`.
