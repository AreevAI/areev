//! ToolCallLlm conformance fixtures (§6.11) — real HTTP round-trips against a
//! canned std-`TcpListener` server, no live keys, no new dependencies. Each
//! test pins one contract point: request encoding (what the provider MUST
//! receive), response decoding (what the caller MUST get back), the
//! retryable/terminal error classification, and the usage-is-required rule.

use areev_core::types::{Tool, ToolKind};
use areev_llm::{
    Anthropic, ChatMessage, Ollama, OpenAiCompat, StopReason, ToolCallLlm, ToolCallRequest,
    ToolChoice,
};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// Serve exactly one canned HTTP response, capturing the request body AND
/// its headers (#286: which header carried the credential is a contract
/// point, not an implementation detail).
type CapturedRequest = (Value, Vec<(String, String)>);

fn one_shot_server_capturing(
    status: u16,
    body: Value,
) -> (String, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end;
        loop {
            let n = stream.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }
        let headers_raw = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let headers: Vec<(String, String)> = headers_raw
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = stream.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
        }
        let raw_body = buf[header_end..header_end + content_length].to_vec();
        let req_body: Value = serde_json::from_slice(&raw_body).unwrap_or(Value::Null);
        let _ = tx.send((req_body, headers));
        let resp_body = body.to_string();
        let resp = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
            resp_body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (format!("http://{addr}"), rx)
}

/// Serve exactly one canned HTTP response, capturing the request body.
/// Returns (base_url, receiver-for-the-captured-body).
fn one_shot_server(status: u16, body: Value) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Read headers, then exactly Content-Length body bytes.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end;
        loop {
            let n = stream.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length: usize = headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = stream.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
        }
        let req_body: Value =
            serde_json::from_slice(&buf[header_end..header_end + content_length])
                .unwrap_or(Value::Null);
        let _ = tx.send(req_body);
        let resp_body = body.to_string();
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "X",
        };
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
            resp_body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (format!("http://{addr}"), rx)
}

/// A Definition grain for a weather lookup — what an abstract node offers.
fn weather_tool() -> Tool {
    Tool::new("get_weather")
        .kind(ToolKind::Definition)
        .tool_description("Look up current weather for a city")
        .input_schema(json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }))
        .strict(true)
}

fn req<'a>(tools: &'a [Tool]) -> ToolCallRequest<'a> {
    ToolCallRequest {
        system: Some("You are a runtime node.".into()),
        messages: vec![ChatMessage::User("weather in Chennai?".into())],
        tools,
        tool_choice: ToolChoice::Auto,
        max_tokens: 256,
        temperature: 0.0,
    }
}

// ── OpenAI-compatible ───────────────────────────────────────────────────────

#[test]
fn openai_clean_tool_call_round_trips() {
    let (base, rx) = one_shot_server(
        200,
        json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Chennai\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 42, "completion_tokens": 7,
                "prompt_tokens_details": {"cached_tokens": 30}
            }
        }),
    );
    let llm = OpenAiCompat::new(&base, "k", "test-model");
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();

    // What the provider received — encoding is part of the contract.
    let sent = rx.recv().unwrap();
    assert_eq!(sent["model"], "test-model");
    assert_eq!(sent["max_tokens"], 256);
    assert_eq!(sent["temperature"], 0.0);
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
    assert!(sent["tools"][0]["function"]["parameters"].is_object());
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][1]["role"], "user");

    // What the caller got back.
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
    let call = &resp.tool_calls[0];
    assert_eq!(call.id, "call_abc");
    assert_eq!(call.name, "get_weather");
    assert_eq!(call.arguments, json!({"city": "Chennai"}));
    assert!(call.arguments_raw.is_none());
    assert_eq!(resp.usage.input_tokens, 42);
    assert_eq!(resp.usage.output_tokens, 7);
    assert_eq!(resp.usage.cache_read_tokens, Some(30));
}

/// Malformed argument JSON surfaces as `arguments_raw`, never a silent `{}` —
/// the runtime's schema-validation re-prompt path needs to SEE the garbage.
#[test]
fn openai_malformed_arguments_surface_raw() {
    let (base, _rx) = one_shot_server(
        200,
        json!({
            "choices": [{
                "message": {"tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\": Chen"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    );
    let llm = OpenAiCompat::new(&base, "k", "m");
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();
    let call = &resp.tool_calls[0];
    assert_eq!(call.arguments, Value::Null);
    assert_eq!(call.arguments_raw.as_deref(), Some("{\"city\": Chen"));
}

#[test]
fn openai_parallel_tool_calls_parse_in_order() {
    let (base, _rx) = one_shot_server(
        200,
        json!({
            "choices": [{
                "message": {"tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"a\"}"}},
                    {"id": "call_2", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"b\"}"}}
                ]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    );
    let llm = OpenAiCompat::new(&base, "k", "m");
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();
    let ids: Vec<&str> = resp.tool_calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["call_1", "call_2"], "provider order is journal order");
}

/// The retry table's input: 429/5xx retryable, other 4xx terminal — carried
/// structurally, never re-parsed from message strings.
#[test]
fn http_status_classification_feeds_the_retry_table() {
    for (status, retryable) in [(429u16, true), (500, true), (400, false)] {
        let (base, _rx) = one_shot_server(status, json!({"error": "x"}));
        let llm = OpenAiCompat::new(&base, "k", "m");
        let tools = [weather_tool()];
        let err = llm.call(&req(&tools)).unwrap_err();
        assert_eq!(
            err.retryable, retryable,
            "HTTP {status} must classify retryable={retryable}: {err}"
        );
    }
}

/// A response without usage is an ERROR — a backend that cannot report usage
/// cannot serve budgeted runs, by construction (§6.7 budgets are pure
/// functions of the journal).
#[test]
fn missing_usage_is_a_terminal_error() {
    let (base, _rx) = one_shot_server(
        200,
        json!({"choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}]}),
    );
    let llm = OpenAiCompat::new(&base, "k", "m");
    let tools = [weather_tool()];
    let err = llm.call(&req(&tools)).unwrap_err();
    assert!(!err.retryable);
    assert!(err.message.contains("usage"), "{err}");
}

// ── Anthropic ───────────────────────────────────────────────────────────────

#[test]
fn anthropic_tool_use_blocks_round_trip_with_history() {
    let (base, rx) = one_shot_server(
        200,
        json!({
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "Chennai"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 9, "cache_read_input_tokens": 40}
        }),
    );
    let llm = Anthropic::new("k", "test-model").with_base_url(&base);
    let tools = [weather_tool()];
    // Include a prior assistant tool call + its result: the history encoding
    // (tool_use block / user-role tool_result block) is part of the contract.
    let request = ToolCallRequest {
        system: Some("You are a runtime node.".into()),
        messages: vec![
            ChatMessage::User("weather in Chennai?".into()),
            ChatMessage::Assistant {
                text: None,
                tool_calls: vec![areev_llm::ToolCallOut {
                    id: "toolu_0".into(),
                    name: "get_weather".into(),
                    arguments: json!({"city": "Chennai"}),
                    arguments_raw: None,
                }],
                provider_content: None,
            },
            ChatMessage::ToolResult {
                tool_call_id: "toolu_0".into(),
                content: "31C, humid".into(),
                is_error: false,
            },
        ],
        tools: &tools,
        tool_choice: ToolChoice::Required,
        max_tokens: 512,
        temperature: 0.0,
    };
    let resp = llm.call(&request).unwrap();

    let sent = rx.recv().unwrap();
    assert_eq!(sent["max_tokens"], 512);
    assert_eq!(sent["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert!(sent["tools"][0]["input_schema"].is_object());
    assert_eq!(sent["tool_choice"]["type"], "any", "Required maps to 'any'");
    // History encoding: assistant tool_use block, then user-role tool_result.
    assert_eq!(sent["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(sent["messages"][2]["role"], "user");
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(sent["messages"][2]["content"][0]["tool_use_id"], "toolu_0");

    assert_eq!(resp.text.as_deref(), Some("Checking."));
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.tool_calls[0].id, "toolu_1");
    assert_eq!(resp.tool_calls[0].arguments, json!({"city": "Chennai"}));
    assert_eq!(resp.usage.cache_read_tokens, Some(40));
}

// ── Ollama ──────────────────────────────────────────────────────────────────

/// Ollama returns argument OBJECTS and no call ids: ids are synthesized
/// deterministically from response order, so journal keys stay reproducible.
#[test]
fn ollama_synthesizes_deterministic_ids_and_reads_eval_counts() {
    let (base, rx) = one_shot_server(
        200,
        json!({
            "message": {"content": "", "tool_calls": [
                {"function": {"name": "get_weather", "arguments": {"city": "a"}}},
                {"function": {"name": "get_weather", "arguments": {"city": "b"}}}
            ]},
            "done_reason": "stop",
            "prompt_eval_count": 12,
            "eval_count": 5
        }),
    );
    let llm = Ollama::new(&base, "local-model");
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();

    let sent = rx.recv().unwrap();
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["options"]["num_predict"], 256);
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");

    assert_eq!(resp.tool_calls.len(), 2);
    assert_eq!(resp.tool_calls[0].id, "call_0");
    assert_eq!(resp.tool_calls[1].id, "call_1");
    assert_eq!(resp.tool_calls[0].arguments, json!({"city": "a"}));
    assert_eq!(resp.stop_reason, StopReason::ToolUse, "tool calls imply tool use");
    assert_eq!(resp.usage.input_tokens, 12);
    assert_eq!(resp.usage.output_tokens, 5);
}

/// Serve one canned RAW response body (SSE/NDJSON) over real HTTP.
fn one_shot_raw_server(body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos + 4]).to_string();
                let len: usize = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + len {
                    break;
                }
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    format!("http://{addr}")
}

/// Real streaming over real HTTP (§6.11 [R3]): the OpenAI-compat override
/// requests `stream: true`, parses SSE deltas incrementally, and returns
/// the same response shape the blocking call would have.
#[test]
fn openai_streaming_parses_sse_over_http() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
               data: [DONE]\n";
    let base = one_shot_raw_server(sse);
    let llm = OpenAiCompat::new(&base, "k", "m");
    let tools: [Tool; 0] = [];
    let mut chunks: Vec<String> = Vec::new();
    let resp = llm
        .call_streaming(&req(&tools), &mut |t| chunks.push(t.to_string()))
        .unwrap();
    assert_eq!(chunks, ["hel", "lo"], "deltas arrive incrementally");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.text.as_deref(), Some("hello"));
    assert_eq!(resp.usage.input_tokens, 3);
    assert_eq!(resp.usage.output_tokens, 2);
}

// ── #283: current Claude models reject sampling parameters ─────────────────

fn anthropic_ok_body() -> Value {
    json!({
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

#[test]
fn anthropic_omits_temperature_for_models_that_reject_it() {
    // Opus 4.7+, Sonnet 5 and the Fable models return HTTP 400 for a request
    // that sets `temperature` — terminal here, so the node died on turn one.
    for model in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-fable-5-1",
        "some-unrecognised-model",
    ] {
        let (base, rx) = one_shot_server(200, anthropic_ok_body());
        let llm = Anthropic::new("k", model).with_base_url(&base);
        let tools = [weather_tool()];
        llm.call(&req(&tools)).unwrap();
        let sent = rx.recv().unwrap();
        assert!(
            sent.get("temperature").is_none(),
            "{model} must send no temperature, got {sent}"
        );
        // Everything else the request needs is still there.
        assert_eq!(sent["max_tokens"], 256, "{model}");
        assert!(sent["messages"].is_array(), "{model}");
        assert!(sent["tools"].is_array(), "{model}");
    }
}

#[test]
fn anthropic_keeps_temperature_for_the_legacy_set() {
    // Nothing moves for existing users on models that still accept it.
    for model in ["claude-haiku-4-5", "claude-sonnet-4-6", "claude-3-5-sonnet-20241022"] {
        let (base, rx) = one_shot_server(200, anthropic_ok_body());
        let llm = Anthropic::new("k", model).with_base_url(&base);
        let tools = [weather_tool()];
        llm.call(&req(&tools)).unwrap();
        let sent = rx.recv().unwrap();
        assert_eq!(sent["temperature"], 0.0, "{model} must still send it");
    }
}

#[test]
fn effective_temperature_never_claims_what_was_not_sent() {
    // Telemetry reads this; an attribute asserting 0.0 on a request that
    // carried none is a false statement about the model's configuration.
    let modern = Anthropic::new("k", "claude-opus-5");
    assert_eq!(modern.effective_temperature(0.0), None);
    let legacy = Anthropic::new("k", "claude-haiku-4-5");
    assert_eq!(legacy.effective_temperature(0.3), Some(0.3));
    // A transport that does not override the method keeps reporting.
    let oai = OpenAiCompat::new("http://x", "k", "gpt-4");
    assert_eq!(oai.effective_temperature(0.7), Some(0.7));
}

// ── #284: thinking blocks survive the tool boundary ────────────────────────

#[test]
fn anthropic_captures_thinking_blocks_as_opaque_provider_content() {
    let (base, rx) = one_shot_server(
        200,
        json!({
            "content": [
                {"type": "thinking", "thinking": "", "signature": "sig-abc"},
                {"type": "tool_use", "id": "toolu_0", "name": "get_weather",
                 "input": {"city": "Chennai"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 3}
        }),
    );
    let llm = Anthropic::new("k", "claude-opus-5").with_base_url(&base);
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();
    let _ = rx.recv().unwrap();

    let pc = resp
        .provider_content
        .as_ref()
        .expect("a thinking block makes the turn opaque");
    let blocks = pc.as_array().unwrap();
    assert_eq!(blocks.len(), 2, "both blocks, in order");
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "sig-abc");
    assert_eq!(blocks[1]["type"], "tool_use");
    // The modelled view is unchanged.
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
}

#[test]
fn a_replayed_assistant_turn_sends_the_provider_content_byte_identically() {
    // The API requires thinking blocks to be echoed unchanged within a
    // tool-use turn; rebuilding them from text + tool_calls is exactly what
    // dropped them.
    let content = json!([
        {"type": "thinking", "thinking": "", "signature": "sig-abc"},
        {"type": "tool_use", "id": "toolu_0", "name": "get_weather",
         "input": {"city": "Chennai"}}
    ]);
    let (base, rx) = one_shot_server(200, anthropic_ok_body());
    let llm = Anthropic::new("k", "claude-opus-5").with_base_url(&base);
    let tools = [weather_tool()];
    let request = ToolCallRequest {
        system: None,
        messages: vec![
            ChatMessage::User("weather?".into()),
            ChatMessage::Assistant {
                text: None,
                tool_calls: vec![areev_llm::ToolCallOut {
                    id: "toolu_0".into(),
                    name: "get_weather".into(),
                    arguments: json!({"city": "Chennai"}),
                    arguments_raw: None,
                }],
                provider_content: Some(content.clone()),
            },
            ChatMessage::ToolResult {
                tool_call_id: "toolu_0".into(),
                content: "31C".into(),
                is_error: false,
            },
        ],
        tools: &tools,
        tool_choice: ToolChoice::Auto,
        max_tokens: 256,
        temperature: 0.0,
    };
    llm.call(&request).unwrap();
    let sent = rx.recv().unwrap();
    let msgs = sent["messages"].as_array().unwrap();
    let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["content"], content, "echoed unchanged");
    // And the tool_result still follows it.
    let last = msgs.last().unwrap();
    assert_eq!(last["content"][0]["type"], "tool_result");
}

#[test]
fn a_turn_without_opaque_content_journals_none() {
    // Every model that does not think, and every other provider, must stay
    // byte-identical to 1.8.5.
    let (base, rx) = one_shot_server(200, anthropic_ok_body());
    let llm = Anthropic::new("k", "claude-haiku-4-5").with_base_url(&base);
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();
    let _ = rx.recv().unwrap();
    assert!(resp.provider_content.is_none());
}

#[test]
fn openai_and_ollama_carry_no_provider_content() {
    let (base, rx) = one_shot_server(
        200,
        json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    );
    let llm = OpenAiCompat::new(&base, "k", "m");
    let tools = [weather_tool()];
    let resp = llm.call(&req(&tools)).unwrap();
    let _ = rx.recv().unwrap();
    assert!(resp.provider_content.is_none());
}

// ── #285: request profiles ─────────────────────────────────────────────────

#[test]
fn the_default_profile_produces_todays_body() {
    use areev_llm::profile::RequestProfile;
    let (base, rx) = one_shot_server(
        200,
        json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    );
    let llm = OpenAiCompat::new(&base, "k", "m").with_request_profile(RequestProfile::default());
    let tools = [weather_tool()];
    llm.call(&req(&tools)).unwrap();
    let sent = rx.recv().unwrap();
    assert_eq!(sent["max_tokens"], 256);
    assert_eq!(sent["temperature"], 0.0);
    assert!(sent.get("max_completion_tokens").is_none());
}

#[test]
fn a_reasoning_profile_swaps_the_token_field_and_drops_temperature() {
    use areev_llm::profile::{RequestProfile, TemperatureMode, TokenLimitField};
    let (base, rx) = one_shot_server(
        200,
        json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    );
    let profile = RequestProfile::new()
        .token_limit(TokenLimitField::MaxCompletionTokens)
        .temperature(TemperatureMode::Omit)
        .extra_body_json(r#"{"store":false,"reasoning_effort":"low"}"#)
        .unwrap();
    let llm = OpenAiCompat::new(&base, "k", "m").with_request_profile(profile);
    let tools = [weather_tool()];
    llm.call(&req(&tools)).unwrap();
    let sent = rx.recv().unwrap();
    assert_eq!(sent["max_completion_tokens"], 256);
    assert!(sent.get("max_tokens").is_none());
    assert!(sent.get("temperature").is_none());
    assert_eq!(sent["store"], false);
    assert_eq!(sent["reasoning_effort"], "low");
}

#[test]
fn an_anthropic_profile_passes_vendor_fields_through_at_the_top_level() {
    use areev_llm::profile::RequestProfile;
    let (base, rx) = one_shot_server(200, anthropic_ok_body());
    let profile = RequestProfile::new()
        .extra_body_json(
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},"inference_geo":"us"}"#,
        )
        .unwrap();
    let llm = Anthropic::new("k", "claude-opus-5")
        .with_base_url(&base)
        .with_request_profile(profile);
    let tools = [weather_tool()];
    llm.call(&req(&tools)).unwrap();
    let sent = rx.recv().unwrap();
    assert_eq!(sent["thinking"]["type"], "adaptive");
    assert_eq!(sent["output_config"]["effort"], "medium");
    assert_eq!(sent["inference_geo"], "us");
}

// ── #286: credential seam ──────────────────────────────────────────────────

#[test]
fn anthropic_sends_x_api_key_by_default_and_bearer_when_asked() {
    use areev_llm::cred::AuthScheme;
    let (base, rx) = one_shot_server_capturing(200, anthropic_ok_body());
    let llm = Anthropic::new("k", "claude-opus-5").with_base_url(&base);
    let tools = [weather_tool()];
    llm.call(&req(&tools)).unwrap();
    let (_, headers) = rx.recv().unwrap();
    assert!(headers.iter().any(|(k, v)| k == "x-api-key" && v == "k"));
    assert!(headers.iter().any(|(k, _)| k == "anthropic-version"));

    let (base, rx) = one_shot_server_capturing(200, anthropic_ok_body());
    let llm = Anthropic::new("k", "claude-opus-5")
        .with_base_url(&base)
        .with_auth_scheme(AuthScheme::Bearer);
    llm.call(&req(&tools)).unwrap();
    let (_, headers) = rx.recv().unwrap();
    assert!(headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == "Bearer k"));
    assert!(
        !headers.iter().any(|(k, _)| k == "x-api-key"),
        "the bearer scheme replaces the default header, never adds to it"
    );
}

#[test]
fn a_signing_credential_sees_the_exact_bytes_that_are_sent() {
    use areev_llm::cred::{AuthRequest, Credential};
    use std::sync::{Arc, Mutex};

    struct Signer(Arc<Mutex<Option<Vec<u8>>>>);
    impl Credential for Signer {
        fn token(&self) -> areev_loop::Result<String> {
            panic!("a signing credential must never be asked for a bare token")
        }
        fn kind(&self) -> &'static str {
            "sigv4-ish"
        }
        fn authorize(
            &self,
            req: &AuthRequest<'_>,
        ) -> areev_loop::Result<Option<Vec<(String, String)>>> {
            assert_eq!(req.method, "POST");
            assert!(req.url.contains("/v1/messages"));
            *self.0.lock().unwrap() = Some(req.body.to_vec());
            Ok(Some(vec![
                ("authorization".into(), "SIG".into()),
                ("x-amz-date".into(), "D".into()),
            ]))
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let (base, rx) = one_shot_server_capturing(200, anthropic_ok_body());
    let llm = Anthropic::with_credential(Box::new(Signer(seen.clone())), "claude-opus-5")
        .with_base_url(&base);
    let tools = [weather_tool()];
    llm.call(&req(&tools)).unwrap();
    let (body, headers) = rx.recv().unwrap();

    assert!(headers.iter().any(|(k, v)| k == "authorization" && v == "SIG"));
    assert!(headers.iter().any(|(k, v)| k == "x-amz-date" && v == "D"));
    assert!(
        !headers.iter().any(|(k, _)| k == "x-api-key"),
        "no default auth header alongside a signature"
    );
    // The bytes it signed are the bytes the server received.
    let signed = seen.lock().unwrap().clone().expect("authorize was called");
    let signed_json: Value = serde_json::from_slice(&signed).unwrap();
    assert_eq!(signed_json, body);
}

#[test]
fn an_authorize_error_is_terminal_and_sends_nothing() {
    use areev_llm::cred::{AuthRequest, Credential};
    struct Broken;
    impl Credential for Broken {
        fn token(&self) -> areev_loop::Result<String> {
            Ok("unused".into())
        }
        fn kind(&self) -> &'static str {
            "broken"
        }
        fn authorize(
            &self,
            _req: &AuthRequest<'_>,
        ) -> areev_loop::Result<Option<Vec<(String, String)>>> {
            Err(areev_loop::Error::LlmBackend("no identity available".into()))
        }
    }
    // No server: if anything were sent, the call would fail as a transport
    // error instead of the terminal credential error.
    let llm = Anthropic::with_credential(Box::new(Broken), "claude-opus-5")
        .with_base_url("http://127.0.0.1:1");
    let tools = [weather_tool()];
    let err = llm.call(&req(&tools)).unwrap_err();
    assert!(!err.retryable, "a credential failure is terminal");
    assert!(err.message.contains("no identity available"), "{}", err.message);
}

#[test]
fn a_default_credential_still_uses_the_adapters_own_header() {
    // `StaticKey` and `GoogleAdc` do not implement `authorize`, so nothing
    // moves for them.
    use areev_llm::cred::{Credential, StaticKey};
    let k = StaticKey::new("secret");
    assert!(k
        .authorize(&areev_llm::cred::AuthRequest { method: "POST", url: "u", body: b"" })
        .unwrap()
        .is_none());
}
