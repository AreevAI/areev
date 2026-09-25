# areev-llm

Out-of-box LLM provider backends for [Areev Loop](../areev-loop)'s reflection engine
(design: [`docs/loop-reflection.md`](../../docs/loop-reflection.md) §9).

Three adapters implement `areev_loop::LlmBackend` over a small **blocking** HTTP
client (`ureq`) — no tokio/reqwest, matching the tree's dependency-light posture.
The HTTP surface lives in this opt-in crate so `areev-loop` and the core stay
serde-only.

| Adapter | Endpoint | Reaches |
|---|---|---|
| `OpenAiCompat` | `POST {base_url}/chat/completions` | OpenAI, Groq, DeepSeek, xAI, Together, Mistral, **Gemini (OpenAI-compat)**, OpenRouter, LiteLLM, vLLM, LM Studio, `llama.cpp` server |
| `Anthropic` | `POST /v1/messages` | Claude models |
| `Ollama` | `POST /api/chat` | local models, no key |

## Use it from `areev`

```bash
export ANTHROPIC_API_KEY=sk-...
areev loop run --db agent.db --model claude-sonnet         # key from the env
areev loop run --db agent.db --model openai:gpt-5
areev loop run --db agent.db --model ollama:llama3.1       # local, no key
areev loop run --db agent.db --model openrouter:openai/gpt-4o-mini   # one key → many models
```

Structured output is **schema-constrained** where the provider supports it
(OpenAI/compat `json_schema` strict, Ollama native `format`), with a
`json_object` fallback. Prompt caching is transparent on OpenAI/OpenRouter
(auto-cached prefixes) and explicit on Anthropic (`cache_control` on the
instruction prefix). The reflection loop is async/batchy, so a slow call is
fine; a dedicated 24h Batch-API job is a possible future add for a full-memory
sweep (not the interactive `areev loop run` path).

Keys are read from the environment (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` /
`OLLAMA_HOST`, or `--llm-api-key-env VAR`), never taken on the command line.
`--llm-base-url` points the OpenAI-compatible adapter at any gateway/local
server. `--llm-cmd` (a subprocess) remains the zero-dependency escape hatch for
anything these three don't cover.

## Library

```rust
let backend = areev_llm::resolve("claude-sonnet", None, None)?; // Box<dyn areev_loop::LlmBackend>
let engine = areev_loop::Engine::with_builtins().with_llm(backend);
```

Each adapter translates the Areev Loop wire protocol (a JSON request whose
`instructions` field is the fixed engine prompt, kept separate from the evidence
data) into a chat request — `instructions` → the system message, the rest → the
user message — and requests JSON output. Areev Loop's parsers tolerate malformed
output (dropping that stage's contribution), so the whole thing is fail-soft.

## Decision backends (`areev_llm::decide`)

Typed, calibrated judgments from a **decision** (System One) model: a `state`
plus named `noul` (yes/no), `choice` (2..=255 options) and `score` (2..=10
ordered levels) questions in, probabilities out, over TypeSafe's
`POST /v1/systemone` wire shape. Optional and never default-on — design and
rules in [`docs/decision-model-proposal.md`](../../docs/decision-model-proposal.md).

| Adapter | Reaches |
|---|---|
| `SystemOneHttp` | TypeSafe, OpenRouter, Vercel AI Gateway, OpenJev, LiteLLM passthrough, self-hosted clones |
| `CloudflareWorkersAi` | Cloudflare Workers AI (`result` envelope) |
| `CommandDecide` | any host command (wire JSON on stdin/stdout, no shell) |
| `LlmEmulated` | any `LlmBackend`, self-reporting — always `calibrated = false` |

`resolve_chain(spec, cmd, default_deadline)` builds an ordered fallback
`Chain` from `--decide typesafe:jev-latest,cloudflare,llm:ollama:qwen3.5:4b`
(only the first colon splits an entry) plus `--decide-cmd`, appended last;
`env_chain()` reads `AREEV_DECIDE`, `AREEV_DECIDE_CMD` and
`AREEV_DECIDE_TIMEOUT_MS` (default 2000). A chain never exceeds the caller's
deadline, and a 429 is reported (`DEC-E007`, with `Retry-After`) rather than
slept on. Errors are the `DEC` domain in `ERROR_CODES.md`.

Not published during the engine's churn phase (`publish = false`).
