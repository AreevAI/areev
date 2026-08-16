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
  .group_by_type/.grain_override/.query_text/.grain_type_diversity`).
- `presets.rs` — `FormatPolicy::claude()` (SML, grouped), `gpt4()` /
  `gemini()` (Markdown), `local_small()` (PlainText), `json_api()` (JSON).
  **Presets never set `token_budget`** — the caller owns that.
- `budget.rs` — `Allocation{Full,Summary,Omit}` and two allocators:
  `allocate()` (pure priority order) and `allocate_with_diversity()`
  (5-phase: group → reserve `min_per_type` → cap trim → Full → fill).
  Progressive disclosure is REAL: Full up to ~70% of budget, degrade to
  Summary up to ~95%, then Omit. `summary_tokens = full/3` heuristic.
- `render.rs` — the `GrainRenderer` trait + `RendererRegistry` (the seam a
  host can override via `ContextAssembler::with_renderer`), one
  `SharedRenderer` per grain type delegating to `areev_cal::render`, and the
  per-type `context_priority` table that feeds allocation (consent 0.95 >
  state 0.9 > goal 0.8 > fact 0.7 > … ; failed tool calls boosted).
- `assembly.rs` — `ContextAssembler` (`format()`, `format_with_hints()`),
  `RenderingHints`, `FormattedContext{text, estimated_tokens, included_count,
  omitted_count, truncated}`, and
  `strip_summaries_for_structured_formats` — JSON/TOON get whole entries or
  nothing (a prose summary inside a structured dump would corrupt it).

## Rendering modes

`format_with_hints` picks exactly one mode, in priority order:
aggregation > timeline (chronological; needs ≥2 hits + temporal intent) >
census (80/20 budget split, keyed on `RecallSource::Census`) >
relevance-highlight (>10 grains) > default. **JSON output bypasses all
modes** — it is a plain structured dump.

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
