//! Out-of-box LLM provider backends for Areev Loop (design: `loop-reflection.md`
//! §9). Three adapters implement `areev_loop::LlmBackend` over a small **blocking**
//! HTTP client (`ureq`) — no tokio/reqwest, matching the tree's dependency-light
//! posture; the HTTP surface lives in this opt-in crate so `areev-loop`/core stay
//! serde-only:
//!
//! - [`OpenAiCompat`] — the workhorse. One `POST {base_url}/chat/completions`
//!   shape reaches ~90% of providers: OpenAI, Groq, DeepSeek, xAI, Together,
//!   Mistral, **Gemini's OpenAI-compat endpoint**, OpenRouter, LiteLLM, vLLM,
//!   LM Studio, and `llama.cpp` server.
//! - [`Anthropic`] — native `/v1/messages`.
//! - [`Ollama`] — native `/api/chat`, local, no key.
//!
//! [`resolve`] turns `--model provider:name` + environment keys into a boxed
//! backend, so the feature lights up with zero config when a standard key is
//! present. `--llm-cmd` (`areev_loop::CommandLlm`) remains the zero-dependency
//! escape hatch for anything these three don't cover.
//!
//! Each adapter translates the Areev Loop wire protocol (a JSON request whose
//! `instructions` field is the fixed engine prompt, kept separate from the
//! evidence data) into a chat request: `instructions` → the **system** message,
//! the remaining request JSON → the **user** message. Output is requested as
//! JSON; Areev Loop's parsers tolerate anything malformed (dropping that stage's
//! contribution), so a stray wrapper is safe.
//!
//! [`extract`] rides the same protocol for a second consumer: turning the free
//! text passed to `remember()` into Fact drafts, so the extraction seam is
//! reachable from the CLI and the bindings and not just from in-process Rust.

pub mod extract;
pub mod llm_detect;
pub mod pseudonymize;
pub mod toolcall;
mod toolcall_stream;

pub use extract::{extract_facts, extract_pipeline, ground_facts, ExtractedFact, Extraction};
pub use llm_detect::LlmDetector;
pub use pseudonymize::PseudonymizingBackend;
pub use toolcall::{
    ChatMessage, StopReason, ToolCallError, ToolCallLlm, ToolCallOut, ToolCallRequest,
    ToolCallResponse, ToolChoice, Usage,
};

use serde_json::{json, Value};
use std::time::Duration;
use areev_loop::{Error, LlmBackend, Result};

const CONNECT_SECS: u64 = 30;
const READ_SECS: u64 = 120; // the reflection loop is async/batchy — a slow call is fine

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(CONNECT_SECS)))
        // ureq 2's single read timeout maps to two phases in ureq 3: waiting
        // for the response head (the LLM thinking) and streaming the body.
        .timeout_recv_response(Some(Duration::from_secs(READ_SECS)))
        .timeout_recv_body(Some(Duration::from_secs(READ_SECS)))
        .build()
        .into()
}

/// The streaming agent (§6.11 [R3]): the whole-body `READ_SECS` cap would
/// kill any long stream, so the BODY phase is uncapped — liveness comes
/// from time-to-first-byte (`timeout_recv_response`) plus the provider's
/// own keep-alive events; a genuinely dead peer surfaces as a socket error.
pub(crate) fn stream_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(CONNECT_SECS)))
        .timeout_recv_response(Some(Duration::from_secs(READ_SECS)))
        .timeout_recv_body(None)
        .build()
        .into()
}

/// POST a JSON body and return the parsed JSON response. `headers` is a slice of
/// (name, value). Maps transport / non-2xx / decode faults to `Error::LlmBackend`.
fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
    let mut req = agent().post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = serde_json::to_string(body)
        .map_err(|e| Error::LlmBackend(format!("encode request: {e}")))?;
    let text = req
        .send(&body)
        .map_err(|e| Error::LlmBackend(format!("{url}: {e}")))?
        .body_mut()
        // Same 10 MiB cap as ureq 2's into_string(), but overflow is now an
        // error instead of silent truncation.
        .read_to_string()
        .map_err(|e| Error::LlmBackend(format!("read response: {e}")))?;
    serde_json::from_str(&text).map_err(|e| Error::LlmBackend(format!("decode response: {e}")))
}

/// Split an Areev Loop protocol request into (system, user): `instructions` become
/// the system prompt; the remaining fields (op/findings/evidence/claims) become
/// the user content, so the fixed instruction never interleaves with the
/// (possibly attacker-influenced) evidence text.
fn split_request(request: &str) -> (String, String) {
    let mut v: Value = serde_json::from_str(request).unwrap_or(Value::Null);
    let system = v
        .get("instructions")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("instructions");
    }
    let user = serde_json::to_string(&v).unwrap_or_else(|_| request.to_string());
    (system, user)
}

/// Answer the construction-time probe (`{"op":"probe"}`) locally — HTTP adapters
/// know their model, so no network call is needed.
fn probe_reply(request: &str, model: &str) -> Option<String> {
    let v: Value = serde_json::from_str(request).ok()?;
    (v.get("op").and_then(|o| o.as_str()) == Some("probe"))
        .then(|| json!({ "model": model }).to_string())
}

/// The `op` of a request (`discover`/`ground`/`verify`/`enrich`).
fn request_op(request: &str) -> String {
    serde_json::from_str::<Value>(request)
        .ok()
        .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// The exact JSON schema for a stage's response (§9 structured output). Authored
/// to the cross-provider intersection: flat, every field `required`,
/// `additionalProperties:false`, no numeric/string constraints — so one schema
/// is portable across OpenAI strict mode and local grammar decoders. `None` for
/// ops (like probe) that have no schema.
fn op_schema(op: &str) -> Option<Value> {
    let obj = |req: Value, props: Value| {
        json!({"type":"object","additionalProperties":false,"required":req,"properties":props})
    };
    let arr = |items: Value| json!({"type":"array","items":items});
    let s = json!({"type":"string"});
    let num = json!({"type":"number"});
    match op {
        "discover" => Some(obj(
            json!(["recommendations"]),
            json!({"recommendations": arr(obj(
                json!(["summary","target","guidance","evidence","confidence"]),
                json!({"summary":s,"target":s,"guidance":s,"evidence":arr(s.clone()),"confidence":num}),
            ))}),
        )),
        "ground" => Some(obj(
            json!(["results"]),
            json!({"results": arr(obj(
                json!(["id","supported","reason"]),
                json!({"id":{"type":"integer"},"supported":{"type":"boolean"},"reason":s}),
            ))}),
        )),
        "verify" => Some(obj(
            json!(["results"]),
            json!({"results": arr(obj(
                json!(["id","keep","confidence","reason"]),
                json!({"id":{"type":"integer"},"keep":{"type":"boolean"},"confidence":num,"reason":s}),
            ))}),
        )),
        "enrich" => Some(obj(
            json!(["notes"]),
            json!({"notes": arr(obj(json!(["target","guidance"]), json!({"target":s,"guidance":s})))}),
        )),
        // The `remember()` extraction seam (`extract.rs`). GROUND for extracted
        // facts reuses the "ground" arm above unchanged.
        "extract" => Some(obj(
            json!(["facts"]),
            json!({"facts": arr(obj(
                json!(["subject","relation","object","confidence"]),
                json!({"subject":s,"relation":s,"object":s,"confidence":num}),
            ))}),
        )),
        _ => None,
    }
}

// ---- OpenAI-compatible (the workhorse / universal fallback) -----------------

pub struct OpenAiCompat {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiCompat { base_url: base_url.into(), api_key: api_key.into(), model: model.into() }
    }

    fn build_body(&self, system: &str, user: &str, op: &str, use_schema: bool) -> Value {
        // Schema-constrained decoding (guaranteed-valid JSON) when the op has a
        // schema; otherwise plain json_object. The instruction still carries the
        // shape, and Areev Loop's parsers tolerate deviation.
        let response_format = match (use_schema, op_schema(op)) {
            (true, Some(schema)) => json!({
                "type": "json_schema",
                "json_schema": {"name": "loop", "strict": true, "schema": schema}
            }),
            _ => json!({"type": "json_object"}),
        };
        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "response_format": response_format,
            "temperature": 0
        })
    }

    fn extract(resp: &Value) -> String {
        resp.pointer("/choices/0/message/content")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for OpenAiCompat {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let op = request_op(request);
        let (system, user) = split_request(request);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        // Prefer schema-constrained decoding; fall back to plain json_object if
        // the endpoint/model rejects json_schema (gateways/local models vary).
        match post_json(&url, &[("Authorization", &auth)], &self.build_body(&system, &user, &op, true)) {
            Ok(resp) => Ok(Self::extract(&resp)),
            Err(_) if op_schema(&op).is_some() => {
                let body = self.build_body(&system, &user, &op, false);
                Ok(Self::extract(&post_json(&url, &[("Authorization", &auth)], &body)?))
            }
            Err(e) => Err(e),
        }
    }
}

// ---- Anthropic (native /v1/messages) ---------------------------------------

pub struct Anthropic {
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) base_url: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Anthropic {
            api_key: api_key.into(),
            model: model.into(),
            base_url: "https://api.anthropic.com".into(),
        }
    }

    /// Override the endpoint (gateways, fixtures). Production callers never
    /// need this; the default is the public API.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn build_body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "max_tokens": 4096,
            // Cache the (stable, per-op) instruction prefix — a cross-run cost
            // win on Anthropic's GA prompt caching. OpenAI/OpenRouter auto-cache
            // long prefixes with no code; Ollama is local.
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": user}]
        })
    }

    fn extract(resp: &Value) -> String {
        // {"content":[{"type":"text","text":"..."}], ...}
        resp.get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .find_map(|b| b.get("text").and_then(|t| t.as_str()))
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for Anthropic {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let (system, user) = split_request(request);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = post_json(
            &url,
            &[("x-api-key", &self.api_key), ("anthropic-version", "2023-06-01")],
            &self.build_body(&system, &user),
        )?;
        Ok(Self::extract(&resp))
    }
}

// ---- Ollama (native /api/chat, local) --------------------------------------

pub struct Ollama {
    pub(crate) host: String,
    pub(crate) model: String,
}

impl Ollama {
    pub fn new(host: impl Into<String>, model: impl Into<String>) -> Self {
        Ollama { host: host.into(), model: model.into() }
    }

    fn build_body(&self, system: &str, user: &str, op: &str) -> Value {
        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "stream": false,
            // The schema (converted to a GBNF grammar locally) when the op has
            // one; else plain "json". Ollama's native `format` is reliable where
            // its OpenAI-compat `/v1` ignores `response_format`.
            "format": op_schema(op).unwrap_or_else(|| json!("json"))
        })
    }

    fn extract(resp: &Value) -> String {
        resp.pointer("/message/content")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for Ollama {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let op = request_op(request);
        let (system, user) = split_request(request);
        let url = format!("{}/api/chat", self.host.trim_end_matches('/'));
        Ok(Self::extract(&post_json(&url, &[], &self.build_body(&system, &user, &op))?))
    }
}

// ---- the factory -----------------------------------------------------------

/// The environment variable a provider reads its key from by default.
fn default_key_env(provider: &str) -> &'static str {
    match provider {
        "anthropic" | "claude" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

fn read_key(key_env: Option<&str>, provider: &str) -> Result<String> {
    let var = key_env.unwrap_or_else(|| default_key_env(provider));
    let k = std::env::var(var).map_err(|_| {
        Error::LlmBackend(format!(
            "${var} is not set — export your API key (or pass --llm-api-key-env)"
        ))
    })?;
    if k.trim().is_empty() {
        return Err(Error::LlmBackend(format!("${var} is empty")));
    }
    Ok(k)
}

/// Split `spec` into (provider, model). `provider:model` is explicit; a bare
/// name is routed by a prefix heuristic, then by which key is present, else to
/// local Ollama.
fn split_spec(spec: &str) -> (String, String) {
    if let Some((p, m)) = spec.split_once(':') {
        return (p.trim().to_lowercase(), m.trim().to_string());
    }
    let low = spec.to_lowercase();
    let provider = if low.starts_with("claude") {
        "anthropic"
    } else if low.starts_with("gpt") || low.starts_with('o') && low.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        "openai"
    } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        "anthropic"
    } else if std::env::var("OPENAI_API_KEY").is_ok() {
        "openai"
    } else {
        "ollama"
    };
    (provider.to_string(), spec.to_string())
}

/// One resolved concrete provider — the shared product of `resolve` (the
/// loop's `LlmBackend`) and `resolve_toolcall` (the runtime's `ToolCallLlm`);
/// every concrete provider implements both traits.
enum Provider {
    Anthropic(Anthropic),
    OpenAi(OpenAiCompat),
    Ollama(Ollama),
}

fn build_provider(spec: &str, base_url: Option<&str>, key_env: Option<&str>) -> Result<Provider> {
    let (provider, model) = split_spec(spec);
    if model.is_empty() {
        return Err(Error::LlmBackend("--model: empty model name".into()));
    }
    match provider.as_str() {
        "anthropic" | "claude" => Ok(Provider::Anthropic(Anthropic::new(read_key(key_env, "anthropic")?, model))),
        "openai" | "gpt" | "openai-compat" | "compat" => {
            let base = base_url
                .map(str::to_string)
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            Ok(Provider::OpenAi(OpenAiCompat::new(base, read_key(key_env, "openai")?, model)))
        }
        "ollama" | "local" => {
            let host = base_url
                .map(str::to_string)
                .or_else(|| std::env::var("OLLAMA_HOST").ok())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Provider::Ollama(Ollama::new(host, model)))
        }
        // OpenRouter is OpenAI-compatible (one key → hundreds of models,
        // named `vendor/model`, e.g. `openrouter:openai/gpt-4o-mini`).
        "openrouter" => {
            let base = base_url
                .map(str::to_string)
                .or_else(|| std::env::var("OPENROUTER_BASE_URL").ok())
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let key = read_key(key_env.or(Some("OPENROUTER_API_KEY")), "openai")?;
            Ok(Provider::OpenAi(OpenAiCompat::new(base, key, model)))
        }
        other => Err(Error::LlmBackend(format!(
            "unknown provider {other:?} (use anthropic|openai|ollama, or provider:model, or --llm-cmd)"
        ))),
    }
}

/// Resolve `--model <spec>` into a backend. `base_url` overrides the endpoint
/// (or `$OPENAI_BASE_URL` / `$OLLAMA_HOST`); `key_env` overrides which env var
/// the key is read from. Keys are read from the environment, never taken on the
/// command line.
pub fn resolve(spec: &str, base_url: Option<&str>, key_env: Option<&str>) -> Result<Box<dyn LlmBackend>> {
    Ok(match build_provider(spec, base_url, key_env)? {
        Provider::Anthropic(p) => Box::new(p),
        Provider::OpenAi(p) => Box::new(p),
        Provider::Ollama(p) => Box::new(p),
    })
}

/// Resolve `--model <spec>` into a [`toolcall::ToolCallLlm`] — the same spec
/// grammar and key discipline as [`resolve`], for the `areev run` abstract-node
/// seam.
pub fn resolve_toolcall(
    spec: &str,
    base_url: Option<&str>,
    key_env: Option<&str>,
) -> Result<std::sync::Arc<dyn toolcall::ToolCallLlm>> {
    Ok(match build_provider(spec, base_url, key_env)? {
        Provider::Anthropic(p) => std::sync::Arc::new(p),
        Provider::OpenAi(p) => std::sync::Arc::new(p),
        Provider::Ollama(p) => std::sync::Arc::new(p),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_request_isolates_instructions() {
        let (sys, user) = split_request(
            r#"{"loop":1,"op":"discover","instructions":"be careful","evidence":[{"hash":"h1"}]}"#,
        );
        assert_eq!(sys, "be careful");
        assert!(user.contains("\"op\":\"discover\""));
        assert!(user.contains("h1"));
        assert!(!user.contains("be careful"), "instructions must not leak into user content");
    }

    #[test]
    fn probe_is_answered_locally() {
        assert_eq!(
            probe_reply(r#"{"op":"probe"}"#, "m-1").as_deref(),
            Some(r#"{"model":"m-1"}"#)
        );
        assert!(probe_reply(r#"{"op":"discover"}"#, "m-1").is_none());
    }

    #[test]
    fn openai_body_uses_schema_with_json_object_fallback() {
        let a = OpenAiCompat::new("https://x/v1", "k", "gpt-x");
        let b = a.build_body("sys", "usr", "discover", true);
        assert_eq!(b["model"], "gpt-x");
        assert_eq!(b["response_format"]["type"], "json_schema");
        assert_eq!(b["response_format"]["json_schema"]["strict"], true);
        assert_eq!(b["messages"][0]["content"], "sys");
        // Fallback (use_schema=false) and a schemaless op both use json_object.
        assert_eq!(a.build_body("s", "u", "discover", false)["response_format"]["type"], "json_object");
        assert_eq!(a.build_body("s", "u", "probe", true)["response_format"]["type"], "json_object");
        let r = json!({"choices":[{"message":{"content":"{\"ok\":1}"}}]});
        assert_eq!(OpenAiCompat::extract(&r), "{\"ok\":1}");
    }

    #[test]
    fn op_schemas_are_strict_and_flat() {
        for op in ["discover", "ground", "verify", "enrich", "extract"] {
            let s = op_schema(op).unwrap_or_else(|| panic!("{op} has a schema"));
            assert_eq!(s["additionalProperties"], false, "{op}");
            assert!(!s["required"].as_array().unwrap().is_empty(), "{op}");
        }
        assert!(op_schema("probe").is_none());
    }

    #[test]
    fn anthropic_extract_reads_text_block() {
        let r = json!({"content":[{"type":"text","text":"hi"}]});
        assert_eq!(Anthropic::extract(&r), "hi");
    }

    #[test]
    fn ollama_body_uses_schema_or_json() {
        let a = Ollama::new("http://localhost:11434", "llama3.1");
        assert_eq!(a.build_body("s", "u", "discover")["format"]["type"], "object");
        assert_eq!(a.build_body("s", "u", "probe")["format"], "json");
        let r = json!({"message":{"content":"{}"}});
        assert_eq!(Ollama::extract(&r), "{}");
    }

    #[test]
    fn resolve_routes_by_prefix_and_explicit_provider() {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        assert_eq!(resolve("claude-sonnet", None, None).unwrap().model(), "claude-sonnet");
        assert_eq!(resolve("anthropic:my-model", None, None).unwrap().model(), "my-model");
        // Ollama needs no key.
        assert_eq!(resolve("ollama:llama3.1", None, None).unwrap().model(), "llama3.1");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    /// `post_json` end-to-end over a real loopback socket (regression for the
    /// ureq 2→3 migration): the request line, Content-Type, custom headers,
    /// and body must arrive as sent; the JSON response must round-trip; and a
    /// non-2xx status must surface as an `Error::LlmBackend` naming the URL.
    #[test]
    fn post_json_round_trips_over_loopback() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut reqs = Vec::new();
            for (status_line, body) in [
                ("HTTP/1.1 200 OK", r#"{"pong":true}"#),
                ("HTTP/1.1 401 Unauthorized", r#"{"error":"bad key"}"#),
            ] {
                let (mut sock, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).unwrap();
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..pos]).to_ascii_lowercase();
                        let cl: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if req.len() >= pos + 4 + cl {
                            break;
                        }
                    }
                }
                reqs.push(String::from_utf8_lossy(&req).to_string());
                let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            reqs
        });

        let url = format!("http://{addr}/v1/chat/completions");

        // 200: the response JSON round-trips.
        let v = post_json(&url, &[("Authorization", "Bearer k")], &json!({"ping": 1})).unwrap();
        assert_eq!(v["pong"], true);

        // 401: surfaced as an LlmBackend error naming the URL.
        let e = match post_json(&url, &[], &json!({})) {
            Err(e) => e,
            Ok(v) => panic!("expected a status error, got {v}"),
        };
        assert!(e.to_string().contains(&url), "{e}");

        let reqs = server.join().unwrap();
        assert!(reqs[0].starts_with("POST /v1/chat/completions"), "{}", reqs[0]);
        let lower = reqs[0].to_ascii_lowercase();
        assert!(lower.contains("content-type: application/json"), "{}", reqs[0]);
        assert!(lower.contains("authorization: bearer k"), "{}", reqs[0]);
        assert!(reqs[0].ends_with(r#"{"ping":1}"#), "{}", reqs[0]);
    }

    #[test]
    fn resolve_reports_missing_key() {
        // `Box<dyn LlmBackend>` isn't Debug, so match rather than unwrap_err.
        let e = match resolve("openai:gpt-x", None, Some("DEFINITELY_UNSET_VAR_XYZ")) {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-key error"),
        };
        assert!(e.to_string().contains("DEFINITELY_UNSET_VAR_XYZ"));
    }
}
