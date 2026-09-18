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

use areev_core::format::tool_schema::{normalize_tool_name, render_json, ProviderKind};
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
        /// The provider's own representation of this assistant turn's
        /// content, opaque to Areev (#284).
        ///
        /// On models that think by default, a response carries `thinking`
        /// blocks (usually empty text plus a signature), and continuing the
        /// conversation requires those blocks to be echoed back UNCHANGED —
        /// within a tool-use turn Anthropic states this as required, and
        /// removing them either 400s on signature/ordering or silently
        /// disables thinking for that request. Either outcome is wrong for a
        /// governed run: the first kills the node, the second changes model
        /// behaviour across the tool boundary with nothing in the journal
        /// saying so.
        ///
        /// A host cannot fix this from its side of the seam: a custom
        /// transport receives the transcript Areev rebuilt, and a
        /// process-local cache does not survive `resume`. So the transcript
        /// itself carries the bytes, and the journal persists them.
        ///
        /// `None` for providers that have no such content — the OpenAI and
        /// Ollama adapters set it to `None` and their bodies are unchanged.
        provider_content: Option<Value>,
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
    /// The assistant turn's content blocks exactly as the provider returned
    /// them, opaque to Areev — see
    /// [`ChatMessage::Assistant::provider_content`] (#284).
    pub provider_content: Option<Value>,
    /// The model the provider reports having SERVED, when it says so (#287).
    ///
    /// An alias or a router that resolved elsewhere is otherwise invisible:
    /// the request says `claude-opus-5`, the journal says `claude-opus-5`,
    /// and which weights actually answered is unrecorded. `None` when the
    /// provider reports nothing.
    pub served_model: Option<String>,
    /// The region the provider reports having served from, where it says so
    /// (#287). `None` when unreported.
    pub served_region: Option<String>,
}

impl ToolCallResponse {
    /// The four fields every transport must supply. Use this rather than a
    /// struct literal: the optional fields are expected to grow, and a
    /// constructor means the next one is not a source break for every host
    /// transport in existence (#284 item 4).
    pub fn new(
        text: Option<String>,
        tool_calls: Vec<ToolCallOut>,
        stop_reason: StopReason,
        usage: Usage,
    ) -> Self {
        Self {
            text,
            tool_calls,
            stop_reason,
            usage,
            provider_content: None,
            served_model: None,
            served_region: None,
        }
    }

    /// Builder form for the provider's opaque content.
    pub fn with_provider_content(mut self, content: Option<Value>) -> Self {
        self.provider_content = content;
        self
    }

    /// Builder form for the served-model/region provenance.
    pub fn with_served(mut self, model: Option<String>, region: Option<String>) -> Self {
        self.served_model = model;
        self.served_region = region;
        self
    }
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
    /// The temperature this transport will ACTUALLY send for `requested`, or
    /// `None` when it will send none at all (#283).
    ///
    /// Telemetry must not claim a parameter that never went out on the wire.
    /// A `gen_ai.request.temperature` attribute of 0.0 on a request that
    /// carried no temperature is worse than a missing attribute: it is an
    /// assertion about the model's configuration that is false, on the
    /// channel an operator uses to explain a run's behaviour.
    ///
    /// Defaulted to "what you asked for", so every host transport keeps
    /// compiling and keeps reporting what it already reported.
    fn effective_temperature(&self, requested: f64) -> Option<f64> {
        Some(requested)
    }
    /// A per-turn price in USD micros for this usage, or `None` when this
    /// transport does not price (#291).
    ///
    /// `None` means UNPRICED, which is not the same as free — the runtime
    /// keeps the two apart so an unpriced run reports `not_measurable`
    /// rather than `$0`, and a cost bound over it never reads as "within".
    ///
    /// The transport is the right place for it: it knows its own model, and
    /// [`Usage`] already carries `cache_read_tokens`. A host that injects its
    /// own transport prices from its own rate card. `Runner` gains no field,
    /// so every struct-literal host keeps compiling.
    fn price_usd_micros(&self, _usage: &Usage) -> Option<u64> {
        None
    }
    /// The region this transport serves from, when it can state one (#287).
    /// Part of a run's model pin: a run parked in one jurisdiction must not
    /// silently finish in another.
    fn region(&self) -> Option<&str> {
        None
    }
    /// An opaque host string identifying THIS transport's configuration
    /// (#287) — a configuration hash, typically.
    ///
    /// Areev never interprets it; it only compares it. That is what lets a
    /// host wrapping its own transport have the engine enforce the host's own
    /// notion of "the same configuration", without the engine having to model
    /// what the host considers significant.
    fn pin_tag(&self) -> Option<&str> {
        None
    }
    /// Content address of this transport's request profile (#285), for the
    /// run's model pin: what was actually SENT is part of what a run ran
    /// under.
    fn request_profile_digest(&self) -> Option<String> {
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

/// Resolve one request's auth headers through the credential seam (#286).
///
/// The body bytes are passed in and returned unchanged: a signing credential
/// hashes them, so the adapter must send exactly what it signed. Serializing
/// again between signing and sending would silently invalidate every
/// signature.
///
/// `Some(headers)` from `authorize` REPLACES the adapter's default auth
/// header entirely — a signer that also got an `x-api-key` alongside its
/// signature would be sending two credentials, one of which it did not
/// intend.
fn resolve_auth_headers(
    cred: &dyn crate::cred::Credential,
    default: impl FnOnce(&str) -> (String, String),
    method: &str,
    url: &str,
    body: &[u8],
) -> ToolCallResult<Vec<(String, String)>> {
    let signed = cred
        .authorize(&crate::cred::AuthRequest { method, url, body })
        .map_err(|e| terminal(e.to_string()))?;
    if let Some(headers) = signed {
        return Ok(headers);
    }
    let token = cred.token().map_err(|e| terminal(e.to_string()))?;
    Ok(vec![default(&token)])
}

/// POST JSON and classify failures for the retry table. Unlike the loop's
/// `post_json`, status codes survive: 429/5xx → retryable, other 4xx →
/// terminal, transport faults → retryable.
fn post_json_classified(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> ToolCallResult<Value> {
    let bytes =
        serde_json::to_vec(body).map_err(|e| terminal(format!("encode request: {e}")))?;
    let owned: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    post_bytes_classified(url, &owned, &bytes)
}

/// The byte-exact form (#286): the caller has already serialized, possibly
/// signed those exact bytes, and must have them sent unchanged.
fn post_bytes_classified(
    url: &str,
    headers: &[(String, String)],
    body_bytes: &[u8],
) -> ToolCallResult<Value> {
    // Status codes are NOT errors at the agent level here, so a 4xx body
    // survives to be read. That body is the only place a provider states
    // *why* structurally — `error.code = "context_length_exceeded"` — and
    // without it an overflow is indistinguishable from any other 400.
    let mut req = crate::agent_reading_error_bodies()
        .post(url)
        .header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let (status, text) = match req.send(body_bytes) {
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
            // `provider_content` is Anthropic-shaped opaque content; the
            // OpenAI-compatible wire format has nowhere to put it, and this
            // adapter never produces one, so it is ignored here and the body
            // stays byte-identical to before #284.
            ChatMessage::Assistant { text, tool_calls, provider_content: _ } => {
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
                                        // Same spelling the tools array
                                        // carries: a transcript replaying a
                                        // dotted Definition by its canonical
                                        // name would name a tool this request
                                        // never offered.
                                        "name": normalize_tool_name(&c.name),
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
        ToolChoice::Named(n) => {
            json!({"type": "function", "function": {"name": normalize_tool_name(n)}})
        }
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
    Ok(ToolCallResponse::new(
        text,
        tool_calls,
        stop_reason,
        require_usage(usage, "openai-compatible")?,
    )
    // The OpenAI wire format echoes the model that answered — which for a
    // router or an alias is not always the one that was asked for (#287).
    .with_served(
        resp.get("model").and_then(|m| m.as_str()).map(str::to_string),
        None,
    ))
}

fn openai_body(
    req: &ToolCallRequest<'_>,
    model: &str,
    profile: &crate::profile::RequestProfile,
) -> ToolCallResult<Value> {
    let mut body = json!({
        "model": model,
        "messages": openai_messages(req),
        "stream": false,
    });
    body[profile.token_limit_key()] = json!(req.max_tokens);
    if profile.sends_temperature() {
        body["temperature"] = json!(req.temperature);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(render_tools(req.tools, ProviderKind::OpenAiTools)?);
        body["tool_choice"] = openai_tool_choice(&req.tool_choice);
    }
    // Merged last, and it can never clobber an owned key — the profile
    // constructor already refused those (#285).
    profile.apply_extra(&mut body);
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
    fn request_profile_digest(&self) -> Option<String> {
        self.profile.digest()
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse> {
        let body = openai_body(req, &self.model, &self.profile)?;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        // Serialize ONCE: a signing credential hashes these exact bytes, and
        // the same bytes are what goes on the wire (#286).
        let bytes =
            serde_json::to_vec(&body).map_err(|e| terminal(format!("encode request: {e}")))?;
        let headers = resolve_auth_headers(
            self.cred.as_ref(),
            |t| ("authorization".to_string(), format!("Bearer {t}")),
            "POST",
            &url,
            &bytes,
        )?;
        openai_parse(&post_bytes_classified(&url, &headers, &bytes)?)
    }
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let mut body = openai_body(req, &self.model, &self.profile)?;
        body["stream"] = json!(true);
        // Without this the final chunk carries no usage — and a usage-less
        // stream is refused (§6.7: budgets need the figures).
        body["stream_options"] = json!({"include_usage": true});
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let bytes =
            serde_json::to_vec(&body).map_err(|e| terminal(format!("encode request: {e}")))?;
        let headers = resolve_auth_headers(
            self.cred.as_ref(),
            |t| ("authorization".to_string(), format!("Bearer {t}")),
            "POST",
            &url,
            &bytes,
        )?;
        let lines = crate::toolcall_stream::post_stream_lines(&url, &headers, &bytes)?;
        crate::toolcall_stream::openai_accumulate(lines, on_token)
    }
}

// ---- Anthropic --------------------------------------------------------------

fn anthropic_messages(req: &ToolCallRequest<'_>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m {
            ChatMessage::User(t) => out.push(json!({"role": "user", "content": t})),
            ChatMessage::Assistant { text, tool_calls, provider_content } => {
                // When the turn carries the provider's own content, replay it
                // BYTE-IDENTICALLY (#284): thinking blocks and their
                // signatures must come back unchanged, and rebuilding the
                // blocks from `text` + `tool_calls` is exactly what dropped
                // them. Rebuilding stays the path for turns Areev itself
                // synthesized and for every other provider.
                if let Some(content) = provider_content {
                    out.push(json!({"role": "assistant", "content": content}));
                    continue;
                }
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for c in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        // Normalized for the same reason the tools array is:
                        // the native API validates a replayed `tool_use.name`
                        // against the names this request offers, and dots are
                        // not legal in either place.
                        "name": normalize_tool_name(&c.name),
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
        ToolChoice::Named(n) => Some(json!({"type": "tool", "name": normalize_tool_name(n)})),
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
    // Capture the whole content array verbatim when it holds anything Areev
    // does not model — today that means `thinking` / `redacted_thinking`
    // blocks and their signatures (#284). A response of only text and
    // tool_use carries no opaque content, so the transcript and the journal
    // stay byte-identical to 1.8.5 for every model that does not think.
    let provider_content = blocks
        .iter()
        .any(|b| {
            !matches!(
                b.get("type").and_then(|t| t.as_str()),
                Some("text") | Some("tool_use")
            )
        })
        .then(|| Value::Array(blocks.clone()));
    Ok(ToolCallResponse::new(
        text,
        tool_calls,
        stop_reason,
        require_usage(usage, "anthropic")?,
    )
    .with_provider_content(provider_content)
    .with_served(
        resp.get("model").and_then(|m| m.as_str()).map(str::to_string),
        None,
    ))
}

/// Whether this Claude model still ACCEPTS a sampling parameter (#283).
///
/// Anthropic removed `temperature`, `top_p` and `top_k` on Claude Opus 4.7,
/// Opus 4.8, Opus 5, Sonnet 5 and the Fable 5 models: a request that sets one
/// returns HTTP 400, which is terminal here (only 429 and 5xx are retried),
/// so an abstract node using any of them died on its first turn, every time.
///
/// The set is CLOSED by construction — every newer model rejects the field —
/// so, unlike a growing per-model table, it cannot go stale. It also fails in
/// the right direction: a model wrongly left out runs at the provider's own
/// default (a quality question), while a model wrongly put in is the dead
/// node this exists to prevent. Same reasoning as [`CLAUDE_CONTEXT_FLOOR`].
fn anthropic_accepts_temperature(model: &str) -> bool {
    // Bedrock and Vertex prefix the model id (`anthropic.claude-…`,
    // `us.anthropic.claude-…`), so match on the family fragment rather than
    // the start of the string.
    const LEGACY_FAMILIES: &[&str] = &[
        "claude-3-",
        "claude-3.",
        "claude-4-0",
        "claude-4-1",
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-sonnet-4-0",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        // `claude-3-` already covers 3.5 and 3.7 — one entry per family that
        // is not a substring of another, so the list stays checkable.
    ];
    LEGACY_FAMILIES.iter().any(|f| model.contains(f))
}

fn anthropic_body(
    req: &ToolCallRequest<'_>,
    model: &str,
    profile: &crate::profile::RequestProfile,
) -> ToolCallResult<Value> {
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "messages": anthropic_messages(req),
    });
    // The model's own contract first (#283: current Claude models 400 on a
    // sampling parameter), then the operator's profile (#285).
    if anthropic_accepts_temperature(model) && profile.sends_temperature() {
        body["temperature"] = json!(req.temperature);
    }
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
    profile.apply_extra(&mut body);
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
    fn effective_temperature(&self, requested: f64) -> Option<f64> {
        anthropic_accepts_temperature(&self.model).then_some(requested)
    }
    fn request_profile_digest(&self) -> Option<String> {
        self.profile.digest()
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> ToolCallResult<ToolCallResponse> {
        let body = anthropic_body(req, &self.model, &self.profile)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let bytes =
            serde_json::to_vec(&body).map_err(|e| terminal(format!("encode request: {e}")))?;
        let scheme = self.auth_scheme;
        let mut headers = resolve_auth_headers(
            self.cred.as_ref(),
            |t| scheme.header(t),
            "POST",
            &url,
            &bytes,
        )?;
        headers.push((
            "anthropic-version".to_string(),
            ANTHROPIC_HEADERS_VERSION.to_string(),
        ));
        anthropic_parse(&post_bytes_classified(&url, &headers, &bytes)?)
    }
    fn call_streaming(
        &self,
        req: &ToolCallRequest<'_>,
        on_token: &mut dyn FnMut(&str),
    ) -> ToolCallResult<ToolCallResponse> {
        let mut body = anthropic_body(req, &self.model, &self.profile)?;
        body["stream"] = json!(true);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let bytes =
            serde_json::to_vec(&body).map_err(|e| terminal(format!("encode request: {e}")))?;
        let scheme = self.auth_scheme;
        let mut headers = resolve_auth_headers(
            self.cred.as_ref(),
            |t| scheme.header(t),
            "POST",
            &url,
            &bytes,
        )?;
        headers.push((
            "anthropic-version".to_string(),
            ANTHROPIC_HEADERS_VERSION.to_string(),
        ));
        let lines = crate::toolcall_stream::post_stream_lines(&url, &headers, &bytes)?;
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
    Ok(ToolCallResponse::new(
        text,
        tool_calls,
        stop_reason,
        require_usage(usage, "ollama")?,
    )
    .with_served(
        resp.get("model").and_then(|m| m.as_str()).map(str::to_string),
        None,
    ))
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
        let bytes =
            serde_json::to_vec(&body).map_err(|e| terminal(format!("encode request: {e}")))?;
        let lines = crate::toolcall_stream::post_stream_lines(&url, &[], &bytes)?;
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

    /// A replayed assistant turn must name its tool the way the `tools` array
    /// does. The runtime's transcript carries the Definition's CANONICAL name
    /// (`receipt.prepare`) so the journal stays addressable; the wire carries
    /// the normalized one, in both places, because a `tool_use` naming a tool
    /// the same request never offered is a 400 — and the loop's second turn is
    /// where that would have landed (#251).
    #[test]
    fn a_replayed_tool_call_is_named_the_way_the_tools_array_names_it() {
        let req = ToolCallRequest {
            system: None,
            messages: vec![
                ChatMessage::User("file it".into()),
                ChatMessage::Assistant {
                    text: None,
                    tool_calls: vec![ToolCallOut {
                        id: "call_0".into(),
                        name: "receipt.prepare".into(),
                        arguments: json!({}),
                        arguments_raw: None,
                    }],
                    provider_content: None,
                },
                ChatMessage::ToolResult {
                    tool_call_id: "call_0".into(),
                    content: "{}".into(),
                    is_error: false,
                },
            ],
            tools: &[],
            tool_choice: ToolChoice::Named("receipt.prepare".into()),
            max_tokens: 256,
            temperature: 0.0,
        };

        let openai = openai_messages(&req);
        assert_eq!(
            openai[1]["tool_calls"][0]["function"]["name"],
            json!("receipt_prepare")
        );
        let anthropic = anthropic_messages(&req);
        assert_eq!(anthropic[1]["content"][0]["name"], json!("receipt_prepare"));

        // Same rule for a forced choice: it names a tool in the array.
        assert_eq!(
            openai_tool_choice(&req.tool_choice)["function"]["name"],
            json!("receipt_prepare")
        );
        assert_eq!(
            anthropic_tool_choice(&req.tool_choice).unwrap()["name"],
            json!("receipt_prepare")
        );
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
