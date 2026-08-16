//! Streaming for the tool-calling seam (§6.11 [R3]): `stream: true`
//! request bodies, a hand-rolled SSE/NDJSON line reader (no new
//! dependency), and one delta-accumulator per provider schema. The
//! accumulators are pure functions over a line iterator, so fixture tests
//! exercise them without a socket; the HTTP layer only produces lines.
//!
//! Contract with the caller: `on_token` receives text deltas AS THEY
//! ARRIVE; the returned [`ToolCallResponse`] is byte-equivalent to what the
//! non-streaming call would have produced — streaming is observational,
//! never a different answer (§6.10's posture, applied to the transport).

use crate::toolcall::{
    StopReason, ToolCallError, ToolCallOut, ToolCallResponse, ToolCallResult, Usage,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::BufRead;

fn terminal(msg: impl Into<String>) -> ToolCallError {
    ToolCallError { retryable: false, message: msg.into() }
}

fn transport(msg: impl Into<String>) -> ToolCallError {
    ToolCallError { retryable: true, message: msg.into() }
}

/// POST and return the response body as a line iterator. Status
/// classification matches `post_json_classified`: 429/5xx retryable, other
/// 4xx terminal, transport faults retryable.
pub(crate) fn post_stream_lines(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> ToolCallResult<impl Iterator<Item = std::io::Result<String>>> {
    let mut req = crate::stream_agent().post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body_text = serde_json::to_string(body)
        .map_err(|e| terminal(format!("encode request: {e}")))?;
    match req.send(&body_text) {
        Ok(resp) => {
            let reader = std::io::BufReader::new(resp.into_body().into_reader());
            Ok(reader.lines())
        }
        Err(ureq::Error::StatusCode(code)) => Err(ToolCallError {
            retryable: code == 429 || (500..=599).contains(&code),
            message: format!("{url}: HTTP {code}"),
        }),
        Err(e) => Err(transport(format!("{url}: {e}"))),
    }
}

/// Parse one accumulated OpenAI-style argument string into the
/// (arguments, arguments_raw) pair — same posture as the non-streaming
/// parser: unparseable or non-object payloads surface raw.
fn args_pair(raw: String) -> (Value, Option<String>) {
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v.is_object() => (v, None),
        Ok(_) | Err(_) => (Value::Null, Some(raw)),
    }
}

// ---- OpenAI-compatible (SSE `data:` lines) ---------------------------------

pub(crate) fn openai_accumulate(
    lines: impl Iterator<Item = std::io::Result<String>>,
    on_token: &mut dyn FnMut(&str),
) -> ToolCallResult<ToolCallResponse> {
    let mut text = String::new();
    // index → (id, name, argument fragments)
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Usage> = None;
    for line in lines {
        let line = line.map_err(|e| transport(format!("stream read: {e}")))?;
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
            continue; // blank keep-alives, `event:`/comment lines
        };
        if payload == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(payload)
            .map_err(|e| terminal(format!("stream chunk does not parse: {e}")))?;
        if let Some(delta) = chunk.pointer("/choices/0/delta") {
            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                if !t.is_empty() {
                    on_token(t);
                    text.push_str(t);
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                    let entry = calls.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        entry.0 = id.to_string();
                    }
                    if let Some(n) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                        entry.1.push_str(n);
                    }
                    if let Some(a) = tc.pointer("/function/arguments").and_then(|a| a.as_str())
                    {
                        entry.2.push_str(a);
                    }
                }
            }
        }
        if let Some(f) = chunk.pointer("/choices/0/finish_reason").and_then(|f| f.as_str()) {
            finish = Some(f.to_string());
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            usage = Some(Usage {
                input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read_tokens: u
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(|c| c.as_u64()),
            });
        }
    }
    let tool_calls: Vec<ToolCallOut> = calls
        .into_values()
        .map(|(id, name, raw)| {
            let (arguments, arguments_raw) = args_pair(raw);
            ToolCallOut { id, name, arguments, arguments_raw }
        })
        .collect();
    let stop_reason = match finish.as_deref() {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("unknown".into()),
    };
    let usage = usage.ok_or_else(|| {
        terminal(
            "stream ended without usage — the request must set \
             stream_options.include_usage (a backend that cannot report \
             usage cannot serve budgeted runs)",
        )
    })?;
    Ok(ToolCallResponse {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        stop_reason,
        usage,
    })
}

// ---- Anthropic (SSE, typed data events) ------------------------------------

pub(crate) fn anthropic_accumulate(
    lines: impl Iterator<Item = std::io::Result<String>>,
    on_token: &mut dyn FnMut(&str),
) -> ToolCallResult<ToolCallResponse> {
    #[derive(Default)]
    struct Block {
        kind: String,
        id: String,
        name: String,
        text: String,
        partial_json: String,
    }
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read: Option<u64> = None;
    let mut saw_usage = false;
    for line in lines {
        let line = line.map_err(|e| transport(format!("stream read: {e}")))?;
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        let ev: Value = serde_json::from_str(payload)
            .map_err(|e| terminal(format!("stream event does not parse: {e}")))?;
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(u) = ev.pointer("/message/usage") {
                    input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    cache_read =
                        u.get("cache_read_input_tokens").and_then(|v| v.as_u64());
                    saw_usage = true;
                }
            }
            Some("content_block_start") => {
                let idx = ev.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let b = blocks.entry(idx).or_default();
                if let Some(cb) = ev.get("content_block") {
                    b.kind = cb
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string();
                    b.id = cb.get("id").and_then(|i| i.as_str()).unwrap_or_default().into();
                    b.name =
                        cb.get("name").and_then(|n| n.as_str()).unwrap_or_default().into();
                    if let Some(t) = cb.get("text").and_then(|t| t.as_str()) {
                        if !t.is_empty() {
                            on_token(t);
                            b.text.push_str(t);
                        }
                    }
                }
            }
            Some("content_block_delta") => {
                let idx = ev.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let b = blocks.entry(idx).or_default();
                match ev.pointer("/delta/type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = ev.pointer("/delta/text").and_then(|t| t.as_str()) {
                            on_token(t);
                            b.text.push_str(t);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(j) =
                            ev.pointer("/delta/partial_json").and_then(|j| j.as_str())
                        {
                            b.partial_json.push_str(j);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(s) = ev.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    stop_reason = Some(s.to_string());
                }
                if let Some(o) = ev.pointer("/usage/output_tokens").and_then(|v| v.as_u64()) {
                    output_tokens = o;
                    saw_usage = true;
                }
            }
            Some("error") => {
                return Err(terminal(format!(
                    "stream error event: {}",
                    ev.get("error").map(|e| e.to_string()).unwrap_or_default()
                )));
            }
            Some("message_stop") => break,
            _ => {} // ping, content_block_stop
        }
    }
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for b in blocks.into_values() {
        match b.kind.as_str() {
            "text" => text.push_str(&b.text),
            "tool_use" => {
                // An empty accumulated input is a no-argument call ({}).
                let raw = if b.partial_json.is_empty() { "{}".into() } else { b.partial_json };
                let (arguments, arguments_raw) = args_pair(raw);
                tool_calls.push(ToolCallOut { id: b.id, name: b.name, arguments, arguments_raw });
            }
            _ => {}
        }
    }
    let stop_reason = match stop_reason.as_deref() {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("unknown".into()),
    };
    if !saw_usage {
        return Err(terminal(
            "anthropic stream carried no usage — cannot serve budgeted runs",
        ));
    }
    Ok(ToolCallResponse {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        stop_reason,
        usage: Usage { input_tokens, output_tokens, cache_read_tokens: cache_read },
    })
}

// ---- Ollama (NDJSON) -------------------------------------------------------

pub(crate) fn ollama_accumulate(
    lines: impl Iterator<Item = std::io::Result<String>>,
    on_token: &mut dyn FnMut(&str),
) -> ToolCallResult<ToolCallResponse> {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCallOut> = Vec::new();
    let mut done_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;
    for line in lines {
        let line = line.map_err(|e| transport(format!("stream read: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&line)
            .map_err(|e| terminal(format!("stream line does not parse: {e}")))?;
        if let Some(t) = chunk.pointer("/message/content").and_then(|c| c.as_str()) {
            if !t.is_empty() {
                on_token(t);
                text.push_str(t);
            }
        }
        if let Some(calls) = chunk.pointer("/message/tool_calls").and_then(|c| c.as_array()) {
            for c in calls {
                let i = tool_calls.len();
                tool_calls.push(ToolCallOut {
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
                    arguments: c.pointer("/function/arguments").cloned().unwrap_or(Value::Null),
                    arguments_raw: None,
                });
            }
        }
        if chunk.get("done").and_then(|d| d.as_bool()) == Some(true) {
            done_reason = chunk
                .get("done_reason")
                .and_then(|d| d.as_str())
                .map(str::to_string);
            if let (Some(i), Some(o)) = (
                chunk.get("prompt_eval_count").and_then(|v| v.as_u64()),
                chunk.get("eval_count").and_then(|v| v.as_u64()),
            ) {
                usage = Some(Usage {
                    input_tokens: i,
                    output_tokens: o,
                    cache_read_tokens: None,
                });
            }
            break;
        }
    }
    let stop_reason = match done_reason.as_deref() {
        Some("stop") if !tool_calls.is_empty() => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None if !tool_calls.is_empty() => StopReason::ToolUse,
        None => StopReason::EndTurn,
    };
    let usage = usage
        .ok_or_else(|| terminal("ollama stream carried no eval counts — cannot serve budgeted runs"))?;
    Ok(ToolCallResponse {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> impl Iterator<Item = std::io::Result<String>> + '_ {
        s.lines().map(|l| Ok(l.to_string()))
    }

    #[test]
    fn openai_stream_accumulates_text_tools_and_usage() {
        let body = r#"
data: {"choices":[{"delta":{"content":"Hel"}}]}

data: {"choices":[{"delta":{"content":"lo"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"search","arguments":"{\"q\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]
"#;
        let mut tokens = String::new();
        let resp = openai_accumulate(lines(body), &mut |t| tokens.push_str(t)).unwrap();
        assert_eq!(tokens, "Hello", "deltas arrived incrementally");
        assert_eq!(resp.text.as_deref(), Some("Hello"));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_9");
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[0].arguments, serde_json::json!({"q": "rust"}));
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.usage.cache_read_tokens, Some(4));
    }

    #[test]
    fn openai_stream_without_usage_is_refused() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n";
        let err = openai_accumulate(lines(body), &mut |_| {}).unwrap_err();
        assert!(!err.retryable);
        assert!(err.message.contains("usage"), "{}", err.message);
    }

    #[test]
    fn anthropic_stream_accumulates_blocks_in_order() {
        let body = r#"
event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":30,"cache_read_input_tokens":10}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"thinking "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"aloud"}}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"fetch"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"ht"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"tps://x\"}"}}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}

data: {"type":"message_stop"}
"#;
        let mut tokens = String::new();
        let resp = anthropic_accumulate(lines(body), &mut |t| tokens.push_str(t)).unwrap();
        assert_eq!(tokens, "thinking aloud");
        assert_eq!(resp.text.as_deref(), Some("thinking aloud"));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "toolu_1");
        assert_eq!(resp.tool_calls[0].name, "fetch");
        assert_eq!(
            resp.tool_calls[0].arguments,
            serde_json::json!({"url": "https://x"})
        );
        assert_eq!(resp.usage.input_tokens, 30);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.usage.cache_read_tokens, Some(10));
    }

    #[test]
    fn anthropic_malformed_tool_json_surfaces_raw() {
        let body = r#"
data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{broken"}}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}
"#;
        let resp = anthropic_accumulate(lines(body), &mut |_| {}).unwrap();
        assert_eq!(resp.tool_calls[0].arguments, Value::Null);
        assert_eq!(resp.tool_calls[0].arguments_raw.as_deref(), Some("{broken"));
    }

    #[test]
    fn ollama_ndjson_accumulates_and_reads_final_counts() {
        let body = r#"{"message":{"content":"par"},"done":false}
{"message":{"content":"tial"},"done":false}
{"message":{"content":"","tool_calls":[{"function":{"name":"grep","arguments":{"q":"x"}}}]},"done":false}
{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":21,"eval_count":5}
"#;
        let mut tokens = String::new();
        let resp = ollama_accumulate(lines(body), &mut |t| tokens.push_str(t)).unwrap();
        assert_eq!(tokens, "partial");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_0", "deterministic synthesized id");
        assert_eq!(resp.stop_reason, StopReason::ToolUse, "tool calls override stop");
        assert_eq!(resp.usage.input_tokens, 21);
        assert_eq!(resp.usage.output_tokens, 5);
    }
}
