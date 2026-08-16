//! Snapshot tests locking the exact bytes each tool-schema provider adapter
//! renders. The adapters are contractually **deterministic** (`tool_schema`
//! module docs: "same input, same bytes"), so these snapshots pin the wire
//! shape of every provider against silent drift. Inputs are fixed (no clock,
//! no randomness).
//!
//! Providers covered (all 9): openai-tools, openai-responses, anthropic-tools,
//! gemini-tools, mcp-tools (JSON) and hermes, llama31, markdown-tools,
//! sml-tools (text).

use areev_core::format::tool_schema::{render_json, render_text, ProviderKind};
use areev_core::{Tool, ToolAnnotations, ToolKind};
use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::json;

/// A rich Slack tool definition: description, nested input schema, output
/// schema, one example, annotations, and `strict = true` — exercises the
/// optional branches in the MCP (outputSchema/annotations), Anthropic
/// (input_examples), and OpenAI (strict) adapters.
fn slack_tool() -> Tool {
    let mut t = Tool::new("slack.post_message").kind(ToolKind::Definition);
    t.tool_description = Some("Post a message to a Slack channel.".to_string());
    t.input_schema = Some(json!({
        "type": "object",
        "properties": {
            "channel": {"type": "string", "description": "Channel ID or name"},
            "text": {"type": "string", "description": "Message body"},
            "thread_ts": {"type": "string", "description": "Optional thread timestamp"}
        },
        "required": ["channel", "text"]
    }));
    t.output_schema = Some(json!({
        "type": "object",
        "properties": {
            "ok": {"type": "boolean"},
            "ts": {"type": "string"}
        }
    }));
    t.examples = Some(vec![json!({"channel": "#ops", "text": "deploy finished"})]);
    t.annotations = Some(ToolAnnotations {
        read_only: false,
        destructive: false,
        idempotent: false,
    });
    t.strict = Some(true);
    t
}

/// A leaner search tool: description + input schema with integer bounds and a
/// string enum, no examples/output-schema/annotations.
fn search_tool() -> Tool {
    let mut t = Tool::new("search.web").kind(ToolKind::Definition);
    t.tool_description = Some("Search the web and return ranked results.".to_string());
    t.input_schema = Some(json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50},
            "safe_search": {"type": "string", "enum": ["off", "moderate", "strict"]}
        },
        "required": ["query"]
    }));
    t
}

#[test]
fn slack_tool_json_providers() {
    let t = slack_tool();
    assert_json_snapshot!(
        "slack_openai_tools",
        render_json(&t, ProviderKind::OpenAiTools).unwrap()
    );
    assert_json_snapshot!(
        "slack_openai_responses",
        render_json(&t, ProviderKind::OpenAiResponses).unwrap()
    );
    assert_json_snapshot!(
        "slack_anthropic_tools",
        render_json(&t, ProviderKind::AnthropicTools).unwrap()
    );
    assert_json_snapshot!(
        "slack_gemini_tools",
        render_json(&t, ProviderKind::GeminiTools).unwrap()
    );
    assert_json_snapshot!(
        "slack_mcp_tools",
        render_json(&t, ProviderKind::McpTools).unwrap()
    );
}

#[test]
fn slack_tool_text_providers() {
    let t = slack_tool();
    assert_snapshot!("slack_hermes", render_text(&t, ProviderKind::Hermes).unwrap());
    assert_snapshot!(
        "slack_llama31",
        render_text(&t, ProviderKind::Llama31).unwrap()
    );
    assert_snapshot!(
        "slack_markdown_tools",
        render_text(&t, ProviderKind::MarkdownTools).unwrap()
    );
    assert_snapshot!(
        "slack_sml_tools",
        render_text(&t, ProviderKind::SmlTools).unwrap()
    );
}

#[test]
fn search_tool_json_providers() {
    let t = search_tool();
    assert_json_snapshot!(
        "search_openai_tools",
        render_json(&t, ProviderKind::OpenAiTools).unwrap()
    );
    assert_json_snapshot!(
        "search_openai_responses",
        render_json(&t, ProviderKind::OpenAiResponses).unwrap()
    );
    assert_json_snapshot!(
        "search_anthropic_tools",
        render_json(&t, ProviderKind::AnthropicTools).unwrap()
    );
    assert_json_snapshot!(
        "search_gemini_tools",
        render_json(&t, ProviderKind::GeminiTools).unwrap()
    );
    assert_json_snapshot!(
        "search_mcp_tools",
        render_json(&t, ProviderKind::McpTools).unwrap()
    );
}

#[test]
fn search_tool_text_providers() {
    let t = search_tool();
    assert_snapshot!(
        "search_hermes",
        render_text(&t, ProviderKind::Hermes).unwrap()
    );
    assert_snapshot!(
        "search_llama31",
        render_text(&t, ProviderKind::Llama31).unwrap()
    );
    assert_snapshot!(
        "search_markdown_tools",
        render_text(&t, ProviderKind::MarkdownTools).unwrap()
    );
    assert_snapshot!(
        "search_sml_tools",
        render_text(&t, ProviderKind::SmlTools).unwrap()
    );
}
