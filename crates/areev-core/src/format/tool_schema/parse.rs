//! Parse-back helper — Phase 3 of action-grain unification (2026-04-20).
//!
//! Inverse of the Phase 2 adapters: takes raw LLM output in one of 5
//! text/JSON formats and extracts structured tool-call records.
//!
//! Provider coverage:
//! - `hermes`: `<tool_call>{...}</tool_call>`
//! - `llama31`: `<|python_tag|>{...}<|eom_id|>`
//! - `anthropic-tools`: `<function_calls>…<invoke name="…"><parameter name="…">…</parameter>…</invoke></function_calls>`
//! - `openai-tools`: assistant message JSON with `tool_calls: [...]`
//! - `markdown-tools`: ```` ```json {...} ``` ````
//!
//! All regexes are compiled via `regex::RegexBuilder::size_limit(1 MB)`
//! to defend against ReDoS / oversized-pattern attacks, following the
//! Phase-1 validator precedent.

use std::sync::OnceLock;

use serde_json::Value;

use super::ProviderKind;

/// A successfully parsed tool call.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct ParsedToolCall {
    /// Call identifier — populated from provider output when available,
    /// otherwise generated as `call_{index}`.
    pub id: String,
    /// Tool name (as emitted by the provider; caller may re-normalize).
    pub name: String,
    /// Parsed arguments object — never a JSON-encoded string, even for
    /// OpenAI (which emits arguments as a JSON string on the wire).
    #[cfg_attr(feature = "http", schema(value_type = serde_json::Value))]
    pub arguments: Value,
}

/// A non-fatal parse error — a malformed tool call that was skipped.
/// Multiple may appear alongside successfully-parsed calls.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct ParseError {
    pub reason: String,
    /// Byte offset of the start of the malformed region when known.
    pub position: Option<usize>,
}

/// Parse the LLM's raw output for the given provider.
///
/// Returns the parsed calls plus any non-fatal errors encountered. A
/// fully-malformed input yields `Ok((vec![], errors))` rather than an
/// `Err` — the caller (typically the harness loop or external agent)
/// decides whether to abort or retry.
pub fn parse(provider: ProviderKind, raw_output: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    match provider {
        ProviderKind::Hermes => parse_hermes(raw_output),
        ProviderKind::Llama31 => parse_llama31(raw_output),
        ProviderKind::AnthropicTools => parse_anthropic(raw_output),
        ProviderKind::OpenAiTools | ProviderKind::OpenAiResponses => parse_openai(raw_output),
        ProviderKind::MarkdownTools => parse_markdown(raw_output),
        ProviderKind::GeminiTools | ProviderKind::McpTools | ProviderKind::SmlTools => (
            vec![],
            vec![ParseError {
                reason: format!("parse is not supported for provider {}", provider.as_str()),
                position: None,
            }],
        ),
    }
}

static HERMES_RE: OnceLock<regex::Regex> = OnceLock::new();
static LLAMA31_RE: OnceLock<regex::Regex> = OnceLock::new();
static MARKDOWN_RE: OnceLock<regex::Regex> = OnceLock::new();

fn bounded(pat: &'static str) -> regex::Regex {
    regex::RegexBuilder::new(pat)
        .size_limit(1 << 20)
        .dot_matches_new_line(true)
        .build()
        .expect("compile-time-valid pattern")
}

fn parse_hermes(raw: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    let re = HERMES_RE.get_or_init(|| bounded(r"<tool_call>\s*(\{.*?\})\s*</tool_call>"));
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for (idx, cap) in re.captures_iter(raw).enumerate() {
        let m = cap.get(1).unwrap();
        match serde_json::from_str::<Value>(m.as_str()) {
            Ok(v) => {
                if let Some(c) = normalize_call(idx, &v) {
                    calls.push(c);
                } else {
                    errors.push(ParseError {
                        reason: "hermes tool_call missing name/arguments".into(),
                        position: Some(m.start()),
                    });
                }
            }
            Err(e) => errors.push(ParseError {
                reason: format!("hermes tool_call JSON: {e}"),
                position: Some(m.start()),
            }),
        }
    }
    (calls, errors)
}

fn parse_llama31(raw: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    // Llama 3.1 emits <|python_tag|>{...}<|eom_id|> OR bare {...}<|eot_id|>.
    let re = LLAMA31_RE
        .get_or_init(|| bounded(r"(?:<\|python_tag\|>)?\s*(\{.*?\})\s*<\|e(?:om|ot)_id\|>"));
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for (idx, cap) in re.captures_iter(raw).enumerate() {
        let m = cap.get(1).unwrap();
        match serde_json::from_str::<Value>(m.as_str()) {
            Ok(v) => {
                if let Some(c) = normalize_call(idx, &v) {
                    calls.push(c);
                } else {
                    errors.push(ParseError {
                        reason: "llama31 call missing name".into(),
                        position: Some(m.start()),
                    });
                }
            }
            Err(e) => errors.push(ParseError {
                reason: format!("llama31 call JSON: {e}"),
                position: Some(m.start()),
            }),
        }
    }
    (calls, errors)
}

fn parse_anthropic(raw: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    // Native Anthropic API returns tool_use blocks in the message content.
    // This parser handles both the native `{"type":"tool_use","id":...,"name":...,"input":{...}}`
    // array shape AND the legacy pre-native XML envelope.
    let mut calls = Vec::new();
    let mut errors = Vec::new();

    // Try JSON content block first.
    if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(raw) {
        for (idx, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{idx}"));
                let name = match block.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n.to_string(),
                    None => {
                        errors.push(ParseError {
                            reason: "anthropic tool_use missing name".into(),
                            position: None,
                        });
                        continue;
                    }
                };
                let arguments = block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                calls.push(ParsedToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
        if !calls.is_empty() || !errors.is_empty() {
            return (calls, errors);
        }
    }

    // Fall through to XML envelope parsing.
    static ANTHROPIC_INVOKE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let invoke_re = ANTHROPIC_INVOKE_RE
        .get_or_init(|| bounded(r#"<invoke\s+name="([^"]+)"\s*>(.*?)</invoke>"#));
    static PARAM_RE: OnceLock<regex::Regex> = OnceLock::new();
    let param_re =
        PARAM_RE.get_or_init(|| bounded(r#"<parameter\s+name="([^"]+)"\s*>(.*?)</parameter>"#));

    for (idx, cap) in invoke_re.captures_iter(raw).enumerate() {
        let name = cap.get(1).unwrap().as_str().to_string();
        let inner = cap.get(2).unwrap().as_str();
        let mut args = serde_json::Map::new();
        for pcap in param_re.captures_iter(inner) {
            let k = pcap.get(1).unwrap().as_str().to_string();
            let v = pcap.get(2).unwrap().as_str();
            // Parameter values are free-form; try JSON first, else string.
            let value = serde_json::from_str::<Value>(v.trim())
                .unwrap_or_else(|_| Value::String(v.to_string()));
            args.insert(k, value);
        }
        calls.push(ParsedToolCall {
            id: format!("call_{idx}"),
            name,
            arguments: Value::Object(args),
        });
    }
    (calls, errors)
}

fn parse_openai(raw: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    // OpenAI tool_calls: either the full assistant message or just the
    // tool_calls array. Handle both shapes.
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                calls,
                vec![ParseError {
                    reason: format!("openai JSON parse: {e}"),
                    position: None,
                }],
            )
        }
    };
    let tool_calls = parsed
        .get("tool_calls")
        .or_else(|| {
            parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("tool_calls"))
        })
        .or(Some(&parsed)) // caller already isolated the array
        .and_then(|v| v.as_array());
    let Some(arr) = tool_calls else {
        return (
            calls,
            vec![ParseError {
                reason: "openai response has no tool_calls".into(),
                position: None,
            }],
        );
    };
    for (idx, tc) in arr.iter().enumerate() {
        // Responses API: {type:"function_call", call_id, name, arguments}
        // Chat API:      {id, type, function:{name, arguments}}
        let id = tc
            .get("id")
            .or_else(|| tc.get("call_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{idx}"));
        let (name, args_raw) = match tc.get("function") {
            Some(f) => (
                f.get("name").and_then(|v| v.as_str()).map(str::to_string),
                f.get("arguments").cloned(),
            ),
            None => (
                tc.get("name").and_then(|v| v.as_str()).map(str::to_string),
                tc.get("arguments").cloned(),
            ),
        };
        let Some(name) = name else {
            errors.push(ParseError {
                reason: "openai tool_call missing name".into(),
                position: None,
            });
            continue;
        };
        // OpenAI always emits arguments as a JSON-encoded string; parse.
        let arguments = match args_raw {
            Some(Value::String(s)) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
            Some(other) => other,
            None => Value::Object(Default::default()),
        };
        calls.push(ParsedToolCall {
            id,
            name,
            arguments,
        });
    }
    (calls, errors)
}

fn parse_markdown(raw: &str) -> (Vec<ParsedToolCall>, Vec<ParseError>) {
    // Markdown convention: fenced JSON with `tool_call`/`name`+`arguments`.
    let re = MARKDOWN_RE.get_or_init(|| bounded(r"```(?:json)?\s*(\{.*?\})\s*```"));
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for (idx, cap) in re.captures_iter(raw).enumerate() {
        let m = cap.get(1).unwrap();
        match serde_json::from_str::<Value>(m.as_str()) {
            Ok(v) => {
                if let Some(c) = normalize_call(idx, &v) {
                    calls.push(c);
                } else {
                    errors.push(ParseError {
                        reason: "markdown fenced JSON has no name".into(),
                        position: Some(m.start()),
                    });
                }
            }
            Err(e) => errors.push(ParseError {
                reason: format!("markdown fenced JSON: {e}"),
                position: Some(m.start()),
            }),
        }
    }
    (calls, errors)
}

/// Extract name + arguments from a single {name, arguments?} JSON object.
fn normalize_call(idx: usize, v: &Value) -> Option<ParsedToolCall> {
    let name = v.get("name").and_then(|x| x.as_str())?.to_string();
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{idx}"));
    let arguments = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v.get("input"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let arguments = match arguments {
        // Hermes sometimes quotes its arguments as a JSON string; unquote.
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    Some(ParsedToolCall {
        id,
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hermes_single_call() {
        let raw = r##"<tool_call>{"name":"slack_post_message","arguments":{"channel":"#ops","text":"hi"}}</tool_call>"##;
        let (calls, errs) = parse(ProviderKind::Hermes, raw);
        assert!(errs.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "slack_post_message");
        assert_eq!(calls[0].arguments, json!({"channel":"#ops","text":"hi"}));
    }

    #[test]
    fn hermes_multiple_calls() {
        let raw = r##"chat<tool_call>{"name":"a","arguments":{}}</tool_call><tool_call>{"name":"b","arguments":{}}</tool_call>end"##;
        let (calls, _) = parse(ProviderKind::Hermes, raw);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
    }

    #[test]
    fn hermes_malformed_emits_error_not_panic() {
        let raw = r##"<tool_call>{"name":}</tool_call>"##;
        let (calls, errs) = parse(ProviderKind::Hermes, raw);
        assert!(calls.is_empty());
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn llama31_python_tag() {
        let raw = r##"<|python_tag|>{"name":"calc","arguments":{"x":2}}<|eom_id|>"##;
        let (calls, _) = parse(ProviderKind::Llama31, raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "calc");
    }

    #[test]
    fn openai_chat_completions_shape() {
        let raw = r##"{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{\"x\":1}"}}]}"##;
        let (calls, _) = parse(ProviderKind::OpenAiTools, raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].arguments, json!({"x":1}));
    }

    #[test]
    fn anthropic_native_tool_use_block() {
        let raw = r##"[{"type":"tool_use","id":"toolu_1","name":"f","input":{"x":1}}]"##;
        let (calls, _) = parse(ProviderKind::AnthropicTools, raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "f");
    }

    #[test]
    fn markdown_fenced_json() {
        let raw = "Here:\n```json\n{\"name\":\"f\",\"arguments\":{\"x\":1}}\n```";
        let (calls, _) = parse(ProviderKind::MarkdownTools, raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, json!({"x":1}));
    }

    #[test]
    fn unsupported_provider_returns_error() {
        let (calls, errs) = parse(ProviderKind::GeminiTools, "whatever");
        assert!(calls.is_empty());
        assert_eq!(errs.len(), 1);
    }
}
