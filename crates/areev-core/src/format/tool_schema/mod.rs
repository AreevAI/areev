//! Tool-catalog renderer — Phase 2 of action-grain unification (2026-04-20).
//!
//! Renders `Tool` grains with `kind = Definition` into the wire format
//! of a target LLM provider (OpenAI, Anthropic, Gemini, MCP, Hermes,
//! Llama 3.1, Markdown, SML). Every adapter accepts a Phase-1-validated
//! definition and produces output that is legal for its target provider
//! by construction.
//!
//! Adapter outputs are **deterministic** — same input, same bytes — so
//! CAL template rendering is stable across replicas.
//!
//! See `docs/facts/tool-formats.md` for output shape examples.

use serde_json::Value;

use crate::error::{AreevError, Result};
use crate::types::Tool;

pub mod anthropic;
pub mod escape;
pub mod gemini;
pub mod hermes;
pub mod llama31;
pub mod markdown;
pub mod mcp;
pub mod openai;
pub mod parse;
pub mod sml;

pub use parse::{parse, ParseError, ParsedToolCall};

/// Target provider for tool-catalog rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    OpenAiTools,
    OpenAiResponses,
    AnthropicTools,
    GeminiTools,
    McpTools,
    Hermes,
    Llama31,
    MarkdownTools,
    SmlTools,
}

impl ProviderKind {
    /// Parse from a lowercase-hyphen wire name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai-tools" => Some(Self::OpenAiTools),
            "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic-tools" => Some(Self::AnthropicTools),
            "gemini-tools" => Some(Self::GeminiTools),
            "mcp-tools" => Some(Self::McpTools),
            "hermes" => Some(Self::Hermes),
            "llama31" => Some(Self::Llama31),
            "markdown-tools" => Some(Self::MarkdownTools),
            "sml-tools" => Some(Self::SmlTools),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiTools => "openai-tools",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicTools => "anthropic-tools",
            Self::GeminiTools => "gemini-tools",
            Self::McpTools => "mcp-tools",
            Self::Hermes => "hermes",
            Self::Llama31 => "llama31",
            Self::MarkdownTools => "markdown-tools",
            Self::SmlTools => "sml-tools",
        }
    }

    /// True for text-shaped providers (Hermes, Llama31, Markdown, SML).
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            Self::Hermes | Self::Llama31 | Self::MarkdownTools | Self::SmlTools
        )
    }

    /// Known lowercase-hyphen names, in stable order.
    pub const ALL: &'static [&'static str] = &[
        "openai-tools",
        "openai-responses",
        "anthropic-tools",
        "gemini-tools",
        "mcp-tools",
        "hermes",
        "llama31",
        "markdown-tools",
        "sml-tools",
    ];
}

/// Render an Tool into provider-native JSON.
///
/// Returns `AreevError::ToolRenderUnsupported` (MEM-E107) when the action
/// lacks fields the provider requires (e.g. Gemini needs `input_schema`
/// with `type: "object"`) or when the provider is text-shaped.
pub fn render_json(action: &Tool, provider: ProviderKind) -> Result<Value> {
    if provider.is_text() {
        return Err(AreevError::ToolRenderUnsupported(format!(
            "provider {} is text-shaped; use render_text",
            provider.as_str()
        )));
    }
    match provider {
        ProviderKind::OpenAiTools => openai::render_tools(action),
        ProviderKind::OpenAiResponses => openai::render_responses(action),
        ProviderKind::AnthropicTools => anthropic::render(action),
        ProviderKind::GeminiTools => gemini::render(action),
        ProviderKind::McpTools => mcp::render(action),
        _ => unreachable!("is_text covered above"),
    }
}

/// Render an Tool into provider-native text (Hermes, Llama31, Markdown, SML).
///
/// Returns `AreevError::ToolRenderUnsupported` (MEM-E107) when the
/// provider is JSON-shaped.
pub fn render_text(action: &Tool, provider: ProviderKind) -> Result<String> {
    if !provider.is_text() {
        return Err(AreevError::ToolRenderUnsupported(format!(
            "provider {} is JSON-shaped; use render_json",
            provider.as_str()
        )));
    }
    match provider {
        ProviderKind::Hermes => hermes::render(action),
        ProviderKind::Llama31 => llama31::render(action),
        ProviderKind::MarkdownTools => markdown::render(action),
        ProviderKind::SmlTools => sml::render(action),
        _ => unreachable!("!is_text covered above"),
    }
}

/// Convenience wrapper — returns `Value::String` for text providers,
/// native JSON for JSON providers. Useful for CAL format dispatch.
pub fn render_any(action: &Tool, provider: ProviderKind) -> Result<Value> {
    if provider.is_text() {
        Ok(Value::String(render_text(action, provider)?))
    } else {
        render_json(action, provider)
    }
}

/// Normalize `tool_name` for provider-facing use: replace `.` with `_`
/// to satisfy name-regex constraints (Anthropic/OpenAI forbid dots).
/// Identity preserved via the grain's `tool_name` field; the invoker
/// reverse-maps on the return path.
pub(crate) fn normalize_tool_name(name: &str) -> String {
    name.replace('.', "_")
}

/// Fetch the shared `(name, description, input_schema)` triple that
/// every JSON adapter uses. Returns MEM-E107 if the action is not a
/// definition or lacks `input_schema`.
pub(crate) fn definition_parts(action: &Tool) -> Result<(String, String, Value)> {
    if action.kind != crate::types::ToolKind::Definition {
        return Err(AreevError::ToolRenderUnsupported(format!(
            "action {} is not kind=definition",
            action.tool_name
        )));
    }
    let description = action
        .tool_description
        .clone()
        .or_else(|| action.content.clone())
        .unwrap_or_default();
    let schema = action.input_schema.clone().ok_or_else(|| {
        AreevError::ToolRenderUnsupported(format!(
            "action {} missing input_schema",
            action.tool_name
        ))
    })?;
    Ok((normalize_tool_name(&action.tool_name), description, schema))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolKind};
    use serde_json::json;

    pub(crate) fn sample_def() -> Tool {
        let mut a = Tool::new("slack.post_message").kind(ToolKind::Definition);
        a.tool_description = Some("Post a message to a Slack channel".to_string());
        a.input_schema = Some(json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "text": {"type": "string"}
            },
            "required": ["channel", "text"]
        }));
        a
    }

    #[test]
    fn provider_kind_round_trip() {
        for name in ProviderKind::ALL {
            let p = ProviderKind::parse(name).unwrap();
            assert_eq!(p.as_str(), *name);
        }
    }

    #[test]
    fn render_json_rejects_text_provider() {
        let a = sample_def();
        let err = render_json(&a, ProviderKind::Hermes).unwrap_err();
        assert!(matches!(err, AreevError::ToolRenderUnsupported(_)));
    }

    #[test]
    fn render_text_rejects_json_provider() {
        let a = sample_def();
        let err = render_text(&a, ProviderKind::AnthropicTools).unwrap_err();
        assert!(matches!(err, AreevError::ToolRenderUnsupported(_)));
    }

    #[test]
    fn render_any_text_yields_string() {
        let a = sample_def();
        let v = render_any(&a, ProviderKind::MarkdownTools).unwrap();
        assert!(v.is_string());
    }

    #[test]
    fn normalize_replaces_dots() {
        assert_eq!(
            normalize_tool_name("slack.post_message"),
            "slack_post_message"
        );
        assert_eq!(normalize_tool_name("gmail.send"), "gmail_send");
    }

    #[test]
    fn definition_parts_rejects_execution_kind() {
        let a = Tool::new("x"); // Default Execution
        let err = definition_parts(&a).unwrap_err();
        assert!(matches!(err, AreevError::ToolRenderUnsupported(_)));
    }
}
