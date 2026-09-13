//! The tool-calling LLM seam (governed-agents proposal §6.11).
//!
//! [`areev_loop::LlmBackend`] is one JSON string in, one JSON string out —
//! deliberately, for the loop's DISCOVER/GROUND/VERIFY protocol. A runtime
//! executing abstract workflow nodes needs what that trait cannot carry: a
//! `tools` array in the provider's native format and parsed `tool_calls` out,
//! with token usage the budget accounting can trust. [`ToolCallLlm`] is that
//! trait; the same three adapters ([`crate::OpenAiCompat`],
//! [`crate::Anthropic`], [`crate::Ollama`]) implement it beside their loop
//! trait, and tools are rendered from **Tool Definition grains** via core's
//! `tool_schema` module, so every provider sees its own canonical shape.
//!
//! Contract points, pinned by the fixture tests below:
//! - `usage` is **required**: a response without it is an error, because a
//!   backend that cannot report usage cannot serve budgeted runs — by
//!   construction, not by convention.
//! - `max_tokens` is **mandatory** on every request: per-dispatch budget
//!   reservation (§6.7) needs a per-call ceiling to reserve.
//! - Errors carry a `retryable` classification: HTTP 429/5xx and transport
//!   faults are retryable; other 4xx are terminal. The runtime's retry table
//!   (§6.3) consumes this, never re-parses message strings.
//! - Provider quirks are absorbed here, not upstream: OpenAI-compat delivers
//!   `arguments` as a JSON-encoded *string* (unparseable ones surface as
//!   [`ToolCallOut::arguments_raw`], never a silent `{}`); Ollama returns
//!   argument objects but no call ids (deterministic `call_<i>` ids are
//!   synthesized, in response order, so journal keys stay reproducible).
//!
//! Streaming: [`ToolCallLlm::call_streaming`] defaults to the non-streaming
//! call delivering one final chunk. Provider SSE/NDJSON parsers land with the
//! Wave-2 `TokenChunk` stream events; the trait shape is fixed now so that
//! work is additive.

use areev_core::format::tool_schema::{render_json, ProviderKind};
use areev_core::types::Tool;
use serde_json::{json, Value};

/// A tool-call seam failure with the runtime's retry classification.
#[derive(Debug, Clone, Default)]
pub struct ToolCallError {
    /// True for HTTP 429/5xx and transport faults (timeout, connect, IO) —
    /// the [`areev_core::types::FailureCause::Timeout`]/`ExecutorError`
    /// retryable family. False for everything the caller must not blindly
    /// retry (auth failures, malformed requests, missing usage).
    pub retryable: bool,
    pub message: String,
    /// The provider rejected the request because the PROMPT was too long.
    ///
    /// Set only from a provider's own STRUCTURED signal — OpenAI-compatible
    /// endpoints return `error.code = "context_length_exceeded"`. Never from
    /// reading a message string: prose is not an API, and a seam that guesses
    /// at wording would silently mis-fire the moment a vendor rewrites it.
    /// Providers that report overflow only in prose (Anthropic's
    /// `invalid_request_error`) leave this false and are covered instead by
    /// the proactive ceiling ([`ToolCallLlm::context_window`]).
    ///
    /// The runtime treats it as neither retryable nor terminal: it folds the
    /// transcript and tries the same turn again on something smaller.
    pub context_overflow: bool,
}

impl std::fmt::Display for ToolCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({})",
            self.message,
            if self.retryable { "retryable" } else { "terminal" }
        )
    }
}

impl std::error::Error for ToolCallError {}

fn terminal(msg: impl Into<String>) -> ToolCallError {
    ToolCallError { retryable: false, message: msg.into(), ..Default::default() }
}

pub type ToolCallResult<T> = std::result::Result<T, ToolCallError>;

/// One prior message in the conversation the model continues.
#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    /// A prior assistant turn — text and/or the tool calls it made.
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCallOut>,
    },
    /// The result of a tool call from a prior assistant turn, addressed by
    /// the id the provider assigned (never by position).
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

/// How the model may use the offered tools.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    /// The model must call some tool.
    Required,
    /// The model must not call tools this turn.
    None,
    /// The model must call this named tool.
    Named(String),
}

/// A parsed tool call from the model.
#[derive(Debug, Clone)]
pub struct ToolCallOut {
    pub id: String,
    pub name: String,
    /// The parsed argument object. [`Value::Null`] when the provider's
    /// argument payload failed to parse — see `arguments_raw`.
    pub arguments: Value,
    /// The provider's raw argument text when it was not valid JSON. The
    /// runtime routes this to its schema-validation re-prompt path
    /// (`FailureCause::SchemaValidationFailed`) instead of dispatching
    /// garbage — and instead of this seam silently substituting `{}`.
    pub arguments_raw: Option<String>,
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

/// Token accounting — **required** on every response (§6.7: budgets are pure
/// functions of the journal, and this is what gets journaled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
}

/// One tool-calling request. `max_tokens` is mandatory by design.
pub struct ToolCallRequest<'a> {
    pub system: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// Tool **Definition** grains; rendered per provider inside the adapter.
    pub tools: &'a [Tool],
    pub tool_choice: ToolChoice,
    pub max_tokens: u32,
    /// Defaults to 0.0 at every call site that wants reproducible-ish output;
    /// carried explicitly so the journaled request is complete.
    pub temperature: f64,
}

/// A parsed tool-calling response.
#[derive(Debug, Clone)]
pub struct ToolCallResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCallOut>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// The tool-calling seam. Implementations are transports: they encode
/// faithfully, decode faithfully, classify errors — and leave policy
/// (retries, schema validation, unknown-tool handling) to the caller, which
/// owns the journal.
pub trait ToolCallLlm: Send + Sync {
    fn model(&self) -> &str;
    /// The provider this transport talks to, in OpenTelemetry's
    /// `gen_ai.provider.name` vocabulary (`anthropic`, `openai`,
    /// `gcp.vertex_ai`, …). Defaulted to semconv's `_OTHER` sentinel so a
    /// host's own implementation of this trait keeps compiling and still
    /// exports a *valid* attribute value rather than a guessed one — the
    /// runtime puts this on every `chat` span.
    fn provider(&self) -> &'static str {
        "_OTHER"
    }
    /// The model's total context window in tokens, when this transport can
    /// state one HONESTLY. `None` means "unknown", never "unbounded".
    ///
    /// The runtime uses it to default a fold ceiling, so a wrong answer here
    /// is worse than no answer: too high and the ceiling never fires, which
    /// fails exactly as it did before anyone set it. Report a number only
    /// where it is a stable property of the provider's whole model family,
    /// not a per-model table that goes stale between releases — a provider
    /// that returns `None` is covered reactively instead, by
    /// [`ToolCallError::context_overflow`].
    fn context_window(&self) -> Option<u64> {
        None
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse>;
    /// Streaming variant: `on_token` receives text deltas as they arrive.
    /// Default: the non-streaming call, delivered as one final chunk —
    /// correct, just not incremental. Provider SSE parsers arrive in Wave 2.
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let resp = self.call(req)?;
        if let Some(t) = &resp.text {
            on_token(t);
        }
        Ok(resp)
    }
}

// ---- shared transport ------------------------------------------------------

/// POST JSON and classify failures for the retry table. Unlike the loop's
/// `post_json`, status codes survive: 429/5xx → retryable, other 4xx →
/// terminal, transport faults → retryable.
fn post_json_classified(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> ToolCallResult<Value> {
    // Status codes are NOT errors at the agent level here, so a 4xx body
    // survives to be read. That body is the only place a provider states
    // *why* structurally — `error.code = "context_length_exceeded"` — and
    // without it an overflow is indistinguishable from any other 400.
    let mut req = crate::agent_reading_error_bodies()
        .post(url)
        .header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body_text = serde_json::to_string(body)
        .map_err(|e| terminal(format!("encode request: {e}")))?;
    let (status, text) = match req.send(&body_text) {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().map_err(|e| ToolCallError {
                retryable: true,
                message: format!("read response: {e}"),
                ..Default::default()
            })?;
            (status, body)
        }
        Err(e) => {
            // The transport layer (connect, timeout, TLS, IO) — status codes
            // no longer land here. Worth one more try from the caller's side.
            return Err(ToolCallError {
                retryable: true,
                message: format!("{url}: {e}"),
                ..Default::default()
            });
        }
    };
    if !(200..300).contains(&status) {
        let overflow = is_context_overflow(&text);
        return Err(ToolCallError {
            retryable: !overflow && (status == 429 || (500..=599).contains(&status)),
            // The body is where the reason is; a bare code sends an operator
            // to the provider's dashboard to find out what we already read.
            message: format!("{url}: HTTP {status}{}", error_detail(&text)),
            context_overflow: overflow,
        });
    }
    serde_json::from_str(&text).map_err(|e| terminal(format!("decode response: {e}")))
}

/// Is this error body a provider saying "your prompt is too long", in a form
/// that is part of its API rather than part of its prose?
///
/// One shape, deliberately: OpenAI-compatible endpoints put a stable
/// `error.code` in the body, and `context_length_exceeded` is a documented
/// value of it. Anthropic reports the same condition as a generic
/// `invalid_request_error` whose only distinguishing content is an English
/// sentence — so it is NOT matched here. Matching prose would work until the
/// day the wording changed, and then fail silently, which is worse than not
/// matching at all. Anthropic is covered proactively instead
/// ([`ToolCallLlm::context_window`]).
fn is_context_overflow(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("code")?.as_str().map(str::to_string))
        .is_some_and(|code| code == "context_length_exceeded")
}

/// The provider's own error text, appended to a status line — bounded, because
/// an error body is untrusted input that lands in a journaled failure detail.
fn error_detail(body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if msg.is_empty() {
        return String::new();
    }
    let clipped: String = msg.chars().take(300).collect();
    format!(" — {clipped}")
}

fn render_tools(tools: &[Tool], provider: ProviderKind) -> ToolCallResult<Vec<Value>> {
    tools
        .iter()
        .map(|t| {
            render_json(t, provider)
                .map_err(|e| terminal(format!("tool '{}' does not render: {e}", t.tool_name)))
        })
        .collect()
}

fn require_usage(u: Option<Usage>, provider: &str) -> ToolCallResult<Usage> {
    u.ok_or_else(|| {
        terminal(format!(
            "{provider} response carries no token usage — a backend that cannot \
             report usage cannot serve budgeted runs"
        ))
    })
}

// ---- OpenAI-compatible ------------------------------------------------------

fn openai_messages(req: &ToolCallRequest<'_>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(s) = &req.system {
        out.push(json!({"role": "system", "content": s}));
    }
    for m in &req.messages {
        match m {
            ChatMessage::User(t) => out.push(json!({"role": "user", "content": t})),
            ChatMessage::Assistant { text, tool_calls } => {
                let mut msg = json!({"role": "assistant"});
                if let Some(t) = text {
                    msg["content"] = json!(t);
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        // Round-trip in the provider's own
                                        // encoding: arguments as a string.
                                        "arguments": c.arguments_raw.clone()
                                            .unwrap_or_else(|| c.arguments.to_string()),
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                out.push(msg);
            }
            ChatMessage::ToolResult { tool_call_id, content, is_error: _ } => {
                // The chat-completions shape has no error flag on tool
                // results; the content carries it.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }
    out
}

fn openai_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Required => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Named(n) => json!({"type": "function", "function": {"name": n}}),
    }
}

fn openai_parse(resp: &Value) -> ToolCallResult<ToolCallResponse> {
    let msg = resp
        .pointer("/choices/0/message")
        .ok_or_else(|| terminal("response has no choices[0].message"))?;
    let text = msg
        .get("content")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
        for c in calls {
            let id = c.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
            let name = c
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let raw = c
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            let (arguments, arguments_raw) = match serde_json::from_str::<Value>(&raw) {
                Ok(v) if v.is_object() => (v, None),
                // Parsed-but-not-an-object is as undispatachable as unparseable.
                Ok(_) | Err(_) => (Value::Null, Some(raw)),
            };
            tool_calls.push(ToolCallOut { id, name, arguments, arguments_raw });
        }
    }
    let stop_reason = match resp.pointer("/choices/0/finish_reason").and_then(|f| f.as_str()) {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("unknown".into()),
    };
    let usage = resp.get("usage").and_then(|u| {
        Some(Usage {
            input_tokens: u.get("prompt_tokens")?.as_u64()?,
            output_tokens: u.get("completion_tokens")?.as_u64()?,
            cache_read_tokens: u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|c| c.as_u64()),
        })
    });
    Ok(ToolCallResponse {
        text,
        tool_calls,
        stop_reason,
        usage: require_usage(usage, "openai-compatible")?,
    })
}

fn openai_body(req: &ToolCallRequest<'_>, model: &str) -> ToolCallResult<Value> {
    let mut body = json!({
        "model": model,
        "messages": openai_messages(req),
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "stream": false,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(render_tools(req.tools, ProviderKind::OpenAiTools)?);
        body["tool_choice"] = openai_tool_choice(&req.tool_choice);
    }
    Ok(body)
}

impl ToolCallLlm for crate::OpenAiCompat {
    fn model(&self) -> &str {
        &self.model
    }
    /// Carried on the adapter, not derived from `base_url`: Vertex and
    /// OpenRouter are BOTH reached through this one OpenAI-compatible
    /// transport, and sniffing a hostname to tell them apart would silently
    /// mislabel every self-hosted gateway.
    fn provider(&self) -> &'static str {
        self.provider_name
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse> {
        let body = openai_body(req, &self.model)?;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.cred.token().map_err(|e| terminal(e.to_string()))?);
        openai_parse(&post_json_classified(&url, &[("Authorization", &auth)], &body)?)
    }
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let mut body = openai_body(req, &self.model)?;
        body["stream"] = json!(true);
        // Without this the final chunk carries no usage — and a usage-less
        // stream is refused (§6.7: budgets need the figures).
        body["stream_options"] = json!({"include_usage": true});
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.cred.token().map_err(|e| terminal(e.to_string()))?);
        let lines =
            crate::toolcall_stream::post_stream_lines(&url, &[("Authorization", &auth)], &body)?;
        crate::toolcall_stream::openai_accumulate(lines, on_token)
    }
}

// ---- Anthropic --------------------------------------------------------------

fn anthropic_messages(req: &ToolCallRequest<'_>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m {
            ChatMessage::User(t) => out.push(json!({"role": "user", "content": t})),
            ChatMessage::Assistant { text, tool_calls } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for c in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.arguments,
                    }));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            ChatMessage::ToolResult { tool_call_id, content, is_error } => {
                // tool_result rides in a USER-role message in the native API.
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                        "is_error": is_error,
                    }]
                }));
            }
        }
    }
    out
}

fn anthropic_tool_choice(tc: &ToolChoice) -> Option<Value> {
    match tc {
        ToolChoice::Auto => Some(json!({"type": "auto"})),
        ToolChoice::Required => Some(json!({"type": "any"})),
        ToolChoice::Named(n) => Some(json!({"type": "tool", "name": n})),
        // The native API has no "none"; omitting tools entirely is the
        // caller's move, so choice None with tools present maps to auto.
        ToolChoice::None => None,
    }
}

fn anthropic_parse(resp: &Value) -> ToolCallResult<ToolCallResponse> {
    let blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| terminal("response has no content blocks"))?;
    let mut text_parts: Vec<&str> = Vec::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t);
                }
            }
            Some("tool_use") => {
                tool_calls.push(ToolCallOut {
                    id: b.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string(),
                    name: b.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
                    arguments: b.get("input").cloned().unwrap_or(Value::Null),
                    arguments_raw: None,
                });
            }
            _ => {}
        }
    }
    let text = (!text_parts.is_empty()).then(|| text_parts.join(""));
    let stop_reason = match resp.get("stop_reason").and_then(|s| s.as_str()) {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("unknown".into()),
    };
    let usage = resp.get("usage").and_then(|u| {
        Some(Usage {
            input_tokens: u.get("input_tokens")?.as_u64()?,
            output_tokens: u.get("output_tokens")?.as_u64()?,
            cache_read_tokens: u.get("cache_read_input_tokens").and_then(|c| c.as_u64()),
        })
    });
    Ok(ToolCallResponse {
        text,
        tool_calls,
        stop_reason,
        usage: require_usage(usage, "anthropic")?,
    })
}

fn anthropic_body(req: &ToolCallRequest<'_>, model: &str) -> ToolCallResult<Value> {
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "messages": anthropic_messages(req),
    });
    if let Some(s) = &req.system {
        // Same cache posture as the loop adapter: the stable prefix is
        // marked ephemeral-cacheable.
        body["system"] =
            json!([{"type": "text", "text": s, "cache_control": {"type": "ephemeral"}}]);
    }
    if !req.tools.is_empty() && req.tool_choice != ToolChoice::None {
        body["tools"] = Value::Array(render_tools(req.tools, ProviderKind::AnthropicTools)?);
        if let Some(tc) = anthropic_tool_choice(&req.tool_choice) {
            body["tool_choice"] = tc;
        }
    }
    Ok(body)
}

const ANTHROPIC_HEADERS_VERSION: &str = "2023-06-01";

/// The context window every current Claude family shares, used as a FLOOR
/// rather than a specification.
///
/// Anthropic reports overflow as a generic `invalid_request_error` whose only
/// distinguishing content is prose, so the reactive path cannot see it and
/// this is the one provider that needs a number. 200k has held across the
/// Claude 3, 4 and 5 families; a model with a larger window (a 1M beta, say)
/// simply folds earlier than it strictly must, which costs one summary and
/// never a failed run. `--llm-context-tokens` overrides it either way.
///
/// A floor is the right shape for a constant that can go stale: wrong-low is
/// a summary nobody needed, wrong-high is the crash this exists to prevent.
pub const CLAUDE_CONTEXT_FLOOR: u64 = 200_000;

impl ToolCallLlm for crate::Anthropic {
    fn model(&self) -> &str {
        &self.model
    }
    fn provider(&self) -> &'static str {
        "anthropic"
    }
    /// Only for `claude-*`. This transport also fronts Bedrock- and
    /// Vertex-hosted models under other names, and a floor asserted over a
    /// model family this constant was never checked against would be exactly
    /// the stale-table failure it exists to avoid.
    fn context_window(&self) -> Option<u64> {
        self.model.starts_with("claude-").then_some(CLAUDE_CONTEXT_FLOOR)
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse> {
        let body = anthropic_body(req, &self.model)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let key = self.cred.token().map_err(|e| terminal(e.to_string()))?;
        anthropic_parse(&post_json_classified(
            &url,
            &[("x-api-key", key.as_str()), ("anthropic-version", ANTHROPIC_HEADERS_VERSION)],
            &body,
        )?)
    }
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let mut body = anthropic_body(req, &self.model)?;
        body["stream"] = json!(true);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let key = self.cred.token().map_err(|e| terminal(e.to_string()))?;
        let lines = crate::toolcall_stream::post_stream_lines(
            &url,
            &[("x-api-key", key.as_str()), ("anthropic-version", ANTHROPIC_HEADERS_VERSION)],
            &body,
        )?;
        crate::toolcall_stream::anthropic_accumulate(lines, on_token)
    }
}

// ---- Ollama -----------------------------------------------------------------

fn ollama_parse(resp: &Value) -> ToolCallResult<ToolCallResponse> {
    let msg = resp
        .get("message")
        .ok_or_else(|| terminal("response has no message"))?;
    let text = msg
        .get("content")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
        for (i, c) in calls.iter().enumerate() {
            tool_calls.push(ToolCallOut {
                // Ollama assigns no call ids. Synthesized deterministically
                // from response order so journal keys are reproducible.
                id: c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{i}")),
                name: c
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                // Native Ollama delivers argument OBJECTS, not strings.
                arguments: c.pointer("/function/arguments").cloned().unwrap_or(Value::Null),
                arguments_raw: None,
            });
        }
    }
    let stop_reason = match resp.get("done_reason").and_then(|d| d.as_str()) {
        Some("stop") if !tool_calls.is_empty() => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None if !tool_calls.is_empty() => StopReason::ToolUse,
        None => StopReason::EndTurn,
    };
    let usage = match (
        resp.get("prompt_eval_count").and_then(|v| v.as_u64()),
        resp.get("eval_count").and_then(|v| v.as_u64()),
    ) {
        (Some(i), Some(o)) => Some(Usage { input_tokens: i, output_tokens: o, cache_read_tokens: None }),
        _ => None,
    };
    Ok(ToolCallResponse {
        text,
        tool_calls,
        stop_reason,
        usage: require_usage(usage, "ollama")?,
    })
}

fn ollama_body(req: &ToolCallRequest<'_>, model: &str) -> ToolCallResult<Value> {
    let mut body = json!({
        "model": model,
        "messages": openai_messages(req),
        "stream": false,
        "options": {"temperature": req.temperature, "num_predict": req.max_tokens},
    });
    if !req.tools.is_empty() {
        // Ollama's native /api/chat takes OpenAI-shaped tool definitions.
        body["tools"] = Value::Array(render_tools(req.tools, ProviderKind::OpenAiTools)?);
    }
    Ok(body)
}

impl ToolCallLlm for crate::Ollama {
    fn model(&self) -> &str {
        &self.model
    }
    /// Not one of semconv's enumerated values; the registry's rule for an
    /// OpenAI-compatible provider it does not list is to use the provider's
    /// own name, which is more useful to an operator than `_OTHER`.
    fn provider(&self) -> &'static str {
        "ollama"
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse> {
        let body = ollama_body(req, &self.model)?;
        let url = format!("{}/api/chat", self.host.trim_end_matches('/'));
        ollama_parse(&post_json_classified(&url, &[], &body)?)
    }
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let mut body = ollama_body(req, &self.model)?;
        body["stream"] = json!(true);
        let url = format!("{}/api/chat", self.host.trim_end_matches('/'));
        let lines = crate::toolcall_stream::post_stream_lines(&url, &[], &body)?;
        crate::toolcall_stream::ollama_accumulate(lines, on_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one shape that counts as evidence: a provider's own structured
    /// `error.code`. Everything else — including a body whose PROSE clearly
    /// says the prompt was too long — is deliberately not matched, because a
    /// seam that reads wording breaks silently the day the wording changes.
    #[test]
    fn only_a_structured_code_counts_as_a_context_overflow() {
        assert!(is_context_overflow(
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#
        ));

        // Anthropic's shape: the condition is real, the evidence is prose.
        assert!(!is_context_overflow(
            r#"{"type":"error","error":{"type":"invalid_request_error",
                "message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#
        ));
        // A different structured code is a different problem.
        assert!(!is_context_overflow(r#"{"error":{"code":"rate_limit_exceeded"}}"#));
        // And nothing at all must not panic or guess.
        assert!(!is_context_overflow(""));
        assert!(!is_context_overflow("upstream returned 400"));
        assert!(!is_context_overflow(r#"{"error":"context_length_exceeded"}"#));
    }

    /// A provider's own words reach the journaled failure detail — bounded,
    /// because an error body is untrusted input.
    #[test]
    fn the_providers_message_is_surfaced_and_clipped() {
        assert_eq!(
            error_detail(r#"{"error":{"message":"prompt is too long"}}"#),
            " — prompt is too long"
        );
        assert_eq!(error_detail("not json"), "");
        assert_eq!(error_detail(r#"{"error":{}}"#), "");

        let long = "x".repeat(1_000);
        let body = format!(r#"{{"error":{{"message":"{long}"}}}}"#);
        let out = error_detail(&body);
        assert_eq!(out.chars().count(), 300 + " — ".chars().count());
    }

    /// The floor is claimed for `claude-*` and nothing else: this transport
    /// also fronts Bedrock and Vertex model names it was never checked
    /// against, and a wrong-high window is the failure it exists to prevent.
    #[test]
    fn the_context_floor_is_claimed_only_for_claude_models() {
        let claude = crate::Anthropic::new("k", "claude-sonnet-5");
        assert_eq!(claude.context_window(), Some(CLAUDE_CONTEXT_FLOOR));

        let bedrock = crate::Anthropic::new("k", "anthropic.claude-v2:1");
        assert_eq!(bedrock.context_window(), None, "an unchecked name claims nothing");
    }
}
