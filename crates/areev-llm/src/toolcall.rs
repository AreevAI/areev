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
#[derive(Debug, Clone)]
pub struct ToolCallError {
    /// True for HTTP 429/5xx and transport faults (timeout, connect, IO) —
    /// the [`areev_core::types::FailureCause::Timeout`]/`ExecutorError`
    /// retryable family. False for everything the caller must not blindly
    /// retry (auth failures, malformed requests, missing usage).
    pub retryable: bool,
    pub message: String,
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
    ToolCallError { retryable: false, message: msg.into() }
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
    let mut req = crate::agent().post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body_text = serde_json::to_string(body)
        .map_err(|e| terminal(format!("encode request: {e}")))?;
    let text = match req.send(&body_text) {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| ToolCallError { retryable: true, message: format!("read response: {e}") })?,
        Err(ureq::Error::StatusCode(code)) => {
            return Err(ToolCallError {
                retryable: code == 429 || (500..=599).contains(&code),
                message: format!("{url}: HTTP {code}"),
            })
        }
        Err(e) => {
            // Everything else at the transport layer (connect, timeout, TLS,
            // IO) is worth one more try from the caller's side.
            return Err(ToolCallError { retryable: true, message: format!("{url}: {e}") });
        }
    };
    serde_json::from_str(&text).map_err(|e| terminal(format!("decode response: {e}")))
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

impl ToolCallLlm for crate::Anthropic {
    fn model(&self) -> &str {
        &self.model
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
