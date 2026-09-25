# Decision backends — typed, calibrated judgments as an optional seam

**Status:** proposal accepted 2026-09-25, implementation in progress on
`feat/decision-backend`. This document is the contract every phase
implements against; names, signatures, flags and env vars below are
binding until a phase changes them here first.

## 1. Why

Areev renders model-ready context, and every judgment on that path is a
constant or a keyword list today: RRF order with the score thrown away, a
per-grain-type priority table, a 70%/95% budget rule, twenty hard-coded
substrings for temporal intent, a 40-character "summary", bigram Jaccard
0.85 for dedup, newest-wins for conflicts. The loop's verifier gates on an
LLM's *self-reported* confidence. The runtime folds transcripts by position.

A **System One** (decision) model is a different kind of model: it takes a
`state` plus named typed questions and returns typed answers with
calibrated probabilities, in one parallel evaluation, with no text
generation — 70–500 ms hosted, sub-25 ms for local encoder clones. That is
exactly the shape of every judgment above. It cannot write Areev's
context, but it can decide it.

The seam is **provider-agnostic**. TypeSafe's Jev defined the wire shape
(`POST /v1/systemone`), but the same shape is served by OpenRouter, Vercel
AI Gateway, Cloudflare Workers AI, OpenJev, LiteLLM passthrough and at
least six Apache/MIT self-hosted clones (von, kev, jev-rs, jev-sim,
oido-systemone, chakuho). TypeSafe itself is US-hosted with no EU/UK/CA
residency and paused signups on 2026-09-22. So Areev ships **one seam and
a chain of providers the user orders**, with the deterministic rule as the
floor. Nothing is default-on. The keyless floor does not move.

## 2. The four rules (recorded in ARCHITECTURE.md §10)

1. **A decision model may score and order; only code omits, gates,
   approves or applies.** This extends §8 "deterministic core; LLM
   optional" to decision models. The 70/95 budget rule, the four loop
   gates, the run scheduler and the authz set stay code.
2. **An uncalibrated backend may reorder, never omit.** Every `Decision`
   carries `calibrated: bool`. LLM-emulated backends are `false`; policy
   code that omits/drops/skips on a probability must check the flag and
   fall back to rank-only behaviour when it is `false`.
3. **Fail open to the deterministic rule — except anonymization, which
   fails safe.** A backend error, deadline, 429 or malformed answer means
   the next chain entry, then today's rule. In `anon`, a backend may raise
   a tier-0 detection's confidence or add a category; it may never
   suppress one.
4. **Every judgment that shaped output is attributable.** `provider`,
   `model`, `calibrated` and latency travel with the answer into the recall
   explanation, the telemetry sidecar, `areev decide` output and
   `GET /api/config`.

Egress: `state` sent to a remote backend is memory egress and goes through
the same pseudonymization path as LLM egress (`areev-llm/src/pseudonymize.rs`).
`docs/security-model.md` records it.

## 3. The seam — `areev_llm::decide`

File: `crates/areev-llm/src/decide.rs` (always compiled; `ureq` is already
a non-optional dependency of areev-llm; no new dependency).

```rust
pub enum Question {
    /// Yes/no. `criteria` optionally describes the two outcomes.
    Noul { instructions: String, criteria: Option<NoulCriteria> },
    /// One of 2..=255 named options; the map value is the option's description.
    Choice { instructions: String, criteria: BTreeMap<String, String> },
    /// Ordered 2..=10 levels, index 0 = lowest.
    Score { instructions: String, levels: Vec<String> },
}
pub struct NoulCriteria { pub yes: String, pub no: String }

pub enum Answer {
    Noul  { p: f32 },
    Choice{ choice: String, probabilities: BTreeMap<String, f32>, confidence: f32 },
    /// `score` = Σ p_i · i over 0-based level indices (probability-weighted index).
    Score { score: f32, probabilities: BTreeMap<String, f32>, confidence: f32,
            legend: BTreeMap<String, String> },
}

pub struct DecideRequest {
    pub state: serde_json::Value,            // string | object | array
    pub questions: BTreeMap<String, Question>,
    pub deadline: Option<Duration>,          // per-call; None = backend default
}

pub struct Decision {
    pub answers: BTreeMap<String, Answer>,
    pub model: String,                       // as served, e.g. "jev-1.13.0"
    pub provider: String,                    // spec name, e.g. "typesafe", "cloudflare", "cmd"
    pub calibrated: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: u64,
}

pub trait DecisionBackend: Send + Sync {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError>;
    fn calibrated(&self) -> bool;
    /// Stable human label for provenance, e.g. "typesafe:jev-latest".
    fn describe(&self) -> String;
}
```

**Wire JSON** (the TypeSafe shape; every hosted gateway and clone speaks
it):

```json
{ "model": "jev-latest", "state": "…" | {…} | […],
  "questions": {
    "q1": { "type": "noul",   "instructions": "…", "criteria": { "true": "…", "false": "…" } },
    "q2": { "type": "choice", "instructions": "…", "criteria": { "a": "…", "b": "…" } },
    "q3": { "type": "score",  "instructions": "…", "criteria": [ "level0", "level1", "level2" ] } } }
```
```json
{ "model": "jev-1.13.0",
  "answers": {
    "q1": { "type": "noul",   "noul": 0.93 },
    "q2": { "type": "choice", "choice": "a", "probabilities": { "a": 0.8, "b": 0.2 }, "confidence": 0.6 },
    "q3": { "type": "score",  "score": 1.4, "probabilities": { "0": 0.1, "1": 0.4, "2": 0.5 },
            "legend": { "0": "level0", "1": "level1", "2": "level2" }, "confidence": 0.25 } },
  "usage": { "input_tokens": 318, "output_tokens": 34 } }
```

`confidence` for Choice/Score is `(n·p_max − 1)/(n − 1)` with `n` the option
or level count (TypeSafe's formula; emulation computes the same so the
field means one thing across providers). Noul has no confidence field.

**Errors — new domain `DEC`** (append-only, `ERROR_CODES.md`):

| Code | Meaning |
|---|---|
| `DEC-E001` | No backend configured, or the spec did not parse |
| `DEC-E002` | Provider transport/HTTP error (includes 401/422/5xx bodies) |
| `DEC-E003` | Malformed answer (missing question id, probabilities not summing, unknown type) |
| `DEC-E004` | Deadline exceeded |
| `DEC-E005` | Chain exhausted (every entry failed; carries each entry's error) |
| `DEC-E006` | Invalid question (criteria count out of range, empty instructions) |
| `DEC-E007` | Rate limited (429) — carries `Retry-After` seconds when present |

`DecideError` lives in `decide.rs`, `Display` leads with the code, and has
`code()`, mirroring `areev_loop::Error`.

### 3.1 Providers

| Spec entry | Adapter | Endpoint | Key env | Notes |
|---|---|---|---|---|
| `typesafe:<model>` | `SystemOneHttp` | `$TYPESAFE_BASE_URL` or `https://api.typesafe.ai` + `/v1/systemone` | `TYPESAFE_API_KEY` | US-hosted; ZDR enterprise only |
| `openrouter:<model>` | `SystemOneHttp` | `$OPENROUTER_BASE_URL` or `https://openrouter.ai/api` + `/v1/systemone` | `OPENROUTER_API_KEY` | model `jev-1.13` / `jev-latest`; no TypeSafe account |
| `vercel:<model>` | `SystemOneHttp` | `https://ai-gateway.vercel.sh/typesafe/v1/systemone` | `AI_GATEWAY_API_KEY` | model `typesafe-ai/jev`; BYOK/ZDR |
| `openjev:<model>` | `SystemOneHttp` | `https://api.openjev.sh/v1/systemone` | `OPENJEV_API_KEY` | model `openjev`; unaffiliated, token-funded proxy — **development only** |
| `cloudflare:<model>` | `CloudflareWorkersAi` | `https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/ai/run/<model>` | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` | model `typesafe/jev`; body is `{state, questions}` (no `model`), answers under `result`, `success` must be true; ZDR |
| `systemone:<url>[#model]` | `SystemOneHttp` | `<url>/v1/systemone` (or as given if it already ends in `/systemone`) | `AREEV_DECIDE_API_KEY` (optional) | LiteLLM `/typesafe`, von, kev, jev-rs, jev-sim, oido, chakuho; default model `jev-latest` |
| `llm:<llm spec>` | `LlmEmulated` | Areev's existing LLM providers via `areev_llm::resolve` | as today | JSON-schema self-report; `calibrated = false` |
| `--decide-cmd <cmd>` | `CommandDecide` | stdin: wire request JSON; stdout: wire response JSON | none | split on whitespace, no shell; `areev_core::proc::run`; 300 s default; appended as the **last** chain entry |

The spec is a comma-separated ordered list: `--decide typesafe:jev-latest,cloudflare:typesafe/jev,llm:ollama:qwen3.5:4b`.
Only the first colon splits provider from target, so URLs and nested LLM
specs pass through. `cmd:` is not a spec entry (a command line may contain
commas); it is its own flag/env.

```rust
pub fn resolve_chain(spec: Option<&str>, cmd: Option<&str>, default_deadline: Option<Duration>)
    -> Result<Option<Arc<dyn DecisionBackend>>, DecideError>;
// None spec and None cmd → Ok(None) (deterministic floor). Otherwise a Chain, even of one.
```

**Chain semantics.** Entries are tried in order. `DEC-E002/E004/E007` and
a `503` move to the next entry; `DEC-E006` (our own bad question) and
`422` do not — an invalid request is not retried. The chain's `Decision`
reports the entry that answered. `calibrated()` on a chain is the AND of
its entries (rank-only if any entry could answer uncalibrated).
`describe()` joins entries with `,`.

**Deadline.** `DecideRequest.deadline` overrides the chain default
(`--decide-timeout-ms`, env `AREEV_DECIDE_TIMEOUT_MS`, default **2000**).
The HTTP adapters set it as the request timeout. Any remaining budget is
what the next entry gets; a chain never exceeds the caller's deadline.

**Retry-After.** On 429 the adapter does **not** sleep (the recall path
cannot afford it); it returns `DEC-E007` with the header value so the
caller/chain moves on. `areev decide` (the CLI verb) may honour it once.

**Cache.** `DecisionRerank` (phase 2) keeps an in-process LRU keyed by
`(question-set version, sha256(state))`; grains are immutable so a
grain-side key never invalidates. Nothing is persisted in the memory file
(host config is per-process by invariant).

### 3.2 Env summary

| Env | Meaning |
|---|---|
| `AREEV_DECIDE` | chain spec (MCP, hooks, bindings default) |
| `AREEV_DECIDE_CMD` | command backend, appended last |
| `AREEV_DECIDE_TIMEOUT_MS` | per-call default deadline (2000) |
| `AREEV_DECIDE_API_KEY` | bearer for `systemone:<url>` |
| `AREEV_RECALL_DEADLINE_MS` | deadline threaded into `recall_hybrid` (phase 0) |
| `AREEV_RERANK_CMD` | command reranker (phase 0) |
| provider keys | `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL`, `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL`, `AI_GATEWAY_API_KEY`, `OPENJEV_API_KEY`, `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` |

## 4. Where it plugs in

Every row is opt-in; the fallback is today's rule.

| # | Where | Today | Typed question | Fallback | Phase |
|---|---|---|---|---|---|
| A1 | Rank recalled grains | RRF; facade forces every score to 1.0 | `score` ×4 levels per candidate, batched in `state` | RRF order | 2 |
| A2 | Full / Summary / Omit | per-type table; 70%/95% rule | `noul` "a summary would lose what matters" | type table | 3 |
| A3 | Timeline vs single-pass | 20 substrings + recency list | `choice` on the query | word lists | 3 |
| A4 | Dedup and conflicts | bigram Jaccard 0.85; newest wins | `noul` same-claim / contradicts | current rules | 7 |
| A5 | Claude Code recall hook | k=5, 400 tokens, no deadline | A1+A2 under a hard deadline | today's hook | 3 |
| A6 | MCP search payload | bare array | `score`, `provider` fields | unchanged | 0/2 |
| B1 | `capture-stop` | every turn stored, no filter | `score` salience stored as a field, never dropped | untagged | 5 |
| B2 | `remember()` grounding | an LLM call | `noul` per fact against source | LLM or none | 5 |
| B3 | Migration importers | type fixed per source; exact-hash dedup | `choice` over grain types; `noul` near-dup | fixed mapping | 5 |
| B4 | Anonymization | tier-0 regex; LLM detector at fixed 0.7 | `noul` per tier-0 candidate refines confidence (raise only) | tier-0 | 5 |
| C1 | Abstract-node tool offer | every pinned host tool | `choice` narrows to top-k inside the pinned set | full list | 4 |
| C2 | Transcript fold | keep last 4, then LLM summary | `noul` keep-call / keep-result per entry, verbatim kept | positional | 4 |
| C3 | Decision node | none | a `decide` Tool executor; edges branch in the frozen grammar | new | 4 |
| C4 | Evalset grading | `equals` / `contains` | `noul` / `score` rubric grader | exact match | 4 |
| D1 | Polling triggers | every item starts a run | the decision node as the plan's first step | all items | 4 (docs) |
| E1 | Loop GROUND / VERIFY | LLM self-reported confidence ≥ 0.75 | `noul` per draft × evidence | LLM | 5 |
| E2 | Duplicate / contradiction sweeps | Jaccard 0.9; ten seeded relations | `noul` same-claim / contradicts | current | 5 |
| E3 | Tool failure cause over MCP | caller-supplied string | `choice` over the closed cause enum | Unknown | 5 |
| F1 | LoCoMo answer judge | LLM YES/NO | `noul` judge (fixed weights → reproducible) | LLM judge | 5 |

## 5. Surfaces (exact names)

**CLI (global flags, parsed like `--embed-cmd`):**
`--decide <chain>`, `--decide-cmd <cmd>`, `--decide-timeout-ms <n>`,
`--recall-deadline-ms <n>`, `--rerank-cmd <cmd>`.
New verb: `areev decide --state <text|@file> (--questions <json|@file> | --noul "<instructions>" | --choice "<instructions>" --option k=desc… | --score "<instructions>" --level desc…)`
prints the wire response plus `provider`, `calibrated`, `latency_ms`.
`USAGE` and `docs/cookbook.md` ("Decision backends") move with it.

**MCP:** reads `AREEV_DECIDE`, `AREEV_DECIDE_CMD`, `AREEV_DECIDE_TIMEOUT_MS`,
`AREEV_RECALL_DEADLINE_MS`, `AREEV_RERANK_CMD`. `areev_search` result rows
gain `score` (phase 0) and, when a reranker answered, `provider` (phase 2).
Tool count unchanged. `docs/mcp-reference.md` moves with it.

**Python (`areev-py`):** `set_decider(spec: str | None = None, cmd: str | None = None, timeout_ms: int | None = None)`,
`decide(state: str, questions: str) -> str` (state is text or a JSON
document; questions is the wire `questions` object as JSON; returns the
wire response + provenance as JSON), `set_recall_deadline_ms(ms: int | None)`,
`set_reranker_command(cmd: str, model: str | None = None)`.

**Node (`areev-js`):** `setDecider(spec?, cmd?, timeoutMs?)`, `decide(state, questions)`,
`setRecallDeadlineMs(ms)`, `setRerankerCommand(cmd, model?)`; regenerate
`index.d.ts`.

**Facade (`areev-cal`):** `AreevFacade::set_decider(Arc<dyn DecisionBackend>)`
(stored for phases 3+; phase 2 installs `DecisionRerank` through
`set_reranker`), `set_reranker(Box<dyn RerankBackend>)`,
`set_recall_deadline(Option<Duration>)`.

**Store (`areev-store`):** `CommandRerank` (stdin `{"query": "...", "docs": ["..."]}` →
stdout JSON array of `docs.len()` numbers; mirrors `CommandEmbed`), and a
scored hybrid recall that returns the fused score per hit (rank-normalized
RRF, top = 1.0; or the reranker's score normalized to [0,1] when a reranker
ran).

**Server:** `GET /api/config` gains `decide: { chain: "<describe()>", calibrated: bool } | null`.

## 6. Phases and ownership

Each phase is releasable alone and gated by `cargo test --workspace` +
`cargo clippy --workspace --all-targets -- -D warnings` (+ the py/js gates
when touched). Owners are crate-scoped to avoid overlapping edits.

| Phase | Scope | Crates | Docs in the same commit |
|---|---|---|---|
| 0 | Plumbing, no model: real score into `SearchHit.score` and the MCP `areev_search` row; deadline threaded through facade/MCP/CLI/bindings; `CommandRerank` + `set_reranker`/`set_recall_deadline` reachable | areev-store, areev-cal, areev-mcp, then areev-cli/py/js | store `CLAUDE.md`, `docs/cal-reference.md` (`min_score` now sees real scores), `docs/mcp-reference.md` |
| 1 | The seam: `decide.rs`, adapters, chain, spec, `DEC` codes, fake `/v1/systemone` server tests; CLI/MCP/py/js surfaces; `areev decide` | areev-llm, areev-cli, areev-py, areev-js, areev-server (`/api/config`) | `ERROR_CODES.md`, `ARCHITECTURE.md` §10, `docs/security-model.md`, `docs/deployment-profile.md`, `docs/cookbook.md`, `CHANGELOG.md` Unreleased |
| 2 | `DecisionRerank: RerankBackend` (batched state, LRU), `--decide` installs it; `areev-bench` `decide_calibrate` (labeled JSONL → ECE/Brier/threshold suggestion per provider); conformance case on both backends with the fake server; LoCoMo A/B with positive control | areev-llm, areev-bench, areev-conformance, areev-cli | `crates/areev-bench/RESULTS.md` (provider/model/calibrated quoted), conformance case list |
| 3 | A2, A3, A5, A6; ASSEMBLE tier selector | areev-context, areev-cal (assemble), areev-cli (hook) | `docs/cal-reference.md`, cookbook |
| 4 | C3 first (unlocks D1), C2, C1, C4 | areev-run, areev-cli (eval), docs | `docs/run.md`, `docs/triggers.md` |
| 5 | B1–B4, E1–E3, F1 | areev-cli, areev-llm, areev-store (migrate), areev-loop, areev-bench | `docs/loop.md`, `docs/migrate.md`, `docs/security-model.md` (anon) |
| 7 | A4; headline `choice` inside the shared renderer; emulation v2 reading restricted next-token log-probs | areev-cal, areev-llm | `docs/cal-reference.md` |

Testing rules (from the `areev-testing` skill): keyless — every test runs
against an in-process fake `/v1/systemone` server on a std `TcpListener`
with fixtures per envelope (TypeSafe, Cloudflare `result` wrapper, a 429
with `Retry-After`, a 503, a malformed answer); deterministic — no wall
clock in assertions; the chain-fallback and deadline paths each have a
test; a regression test for every bug.

## 7. Region and residency guidance (for `docs/deployment-profile.md`)

| Situation | Chain to use |
|---|---|
| No residency requirement | any hosted entry; `openrouter:` needs no TypeSafe account |
| EU / UK / CA residency | `cloudflare:` (ZDR) or `vercel:` with BYOK + ZDR, or self-hosted `systemone:` |
| Air-gapped | self-hosted `systemone:` (von / kev / jev-rs / oido) or `--decide-cmd` |
| No decision model reachable at all | `llm:<local or regional LLM>` (rank-only) then the deterministic floor |
| Development | `openjev:openjev` (free) — never for customer memories |

## 8. Out of scope, deliberately

- No new CAL syntax. `WITH rerank`, `WITH dedup`, `WITH conflict_resolution`
  keep their syntax; a backend changes *how* they decide, not *what* they
  mean. New syntax is an OMS decision.
- No Trigger grain schema change. D1 is the decision node as the started
  plan's first step.
- No MCP tool added in v1 (`areev_decide` may follow; it moves the pinned
  tool count).
- Never default-on; never in the 50 ms voice-loop gate (a remote backend
  is 70–500 ms; only a local encoder clone qualifies there, still opt-in).
