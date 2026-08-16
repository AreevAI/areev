use serde::{Deserialize, Serialize};

use super::grain::{Grain, GrainCommon, GrainType};

/// Author of a chat-style Event message, per OMS 1.2 §6.1 `role` field.
/// Serializes to the canonical lowercase string ("user" | "assistant" |
/// "system" | "tool").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "system" => Some(Role::System),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

/// OMS 1.2 §8.2 wire-format content block, mirroring Anthropic/OpenAI
/// message blocks. Stored exactly as the provider emitted (or produced)
/// so a turn can be replayed byte-identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// OMS 1.2 §8.2 per-turn LLM token accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
}

/// An Event grain — timestamped occurrence, message, or behavioral event.
#[derive(Debug, Clone)]
pub struct Event {
    pub content: String,
    /// Optional subject for triple-store indexing.
    pub subject: Option<String>,
    /// Optional object for triple-store indexing.
    pub object: Option<String>,

    // ── Chat-conversation extensions (OMS 1.2 §8.2) ──
    pub role: Option<Role>,
    pub session_id: Option<String>,
    pub parent_message_id: Option<String>,
    pub content_blocks: Option<Vec<ContentBlock>>,
    pub model_id: Option<String>,
    pub stop_reason: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub run_id: Option<String>,

    pub common: GrainCommon,
}

impl Event {
    pub fn new(content: &str) -> Self {
        Event {
            content: content.to_string(),
            subject: None,
            object: None,
            role: None,
            session_id: None,
            parent_message_id: None,
            content_blocks: None,
            model_id: None,
            stop_reason: None,
            token_usage: None,
            run_id: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn subject(mut self, s: &str) -> Self {
        self.subject = Some(s.to_string());
        self
    }

    pub fn object(mut self, o: &str) -> Self {
        self.object = Some(o.to_string());
        self
    }

    pub fn role(mut self, r: Role) -> Self {
        self.role = Some(r);
        self
    }

    pub fn session(mut self, s: String) -> Self {
        self.session_id = Some(s);
        self
    }

    pub fn parent_message(mut self, h: String) -> Self {
        self.parent_message_id = Some(h);
        self
    }

    pub fn content_blocks(mut self, b: Vec<ContentBlock>) -> Self {
        self.content_blocks = Some(b);
        self
    }

    pub fn model(mut self, m: String) -> Self {
        self.model_id = Some(m);
        self
    }

    pub fn stop_reason(mut self, s: String) -> Self {
        self.stop_reason = Some(s);
        self
    }

    pub fn token_usage(mut self, u: TokenUsage) -> Self {
        self.token_usage = Some(u);
        self
    }

    pub fn run_id(mut self, r: String) -> Self {
        self.run_id = Some(r);
        self
    }
}

impl Grain for Event {
    fn grain_type(&self) -> GrainType {
        GrainType::Event
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        self.content.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serde_roundtrip() {
        for r in [Role::User, Role::Assistant, Role::System, Role::Tool] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(s, format!("\"{}\"", r.as_str()));
            let back: Role = serde_json::from_str(&s).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn role_from_str_rejects_unknown() {
        assert!(Role::from_str("admin").is_none());
        assert!(Role::from_str("").is_none());
        assert!(Role::from_str("User").is_none()); // case-sensitive
    }

    #[test]
    fn content_block_text_roundtrip() {
        let b = ContentBlock::Text {
            text: "hello".into(),
        };
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"type\":\"text\""));
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn content_block_tool_use_roundtrip() {
        let b = ContentBlock::ToolUse {
            id: "toolu_01".into(),
            name: "calculator".into(),
            input: serde_json::json!({"x": 2, "y": 3}),
        };
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"type\":\"tool_use\""));
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn content_block_tool_result_roundtrip() {
        let b = ContentBlock::ToolResult {
            tool_use_id: "toolu_01".into(),
            content: "5".into(),
            is_error: Some(false),
        };
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"type\":\"tool_result\""));
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn token_usage_roundtrip_with_defaults() {
        let tu = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        };
        let s = serde_json::to_string(&tu).unwrap();
        // Cache fields must be skipped when None
        assert!(!s.contains("cache_read_tokens"));
        let back: TokenUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, tu);
    }

    #[test]
    fn event_new_defaults_new_fields_to_none() {
        let ev = Event::new("hi");
        assert!(ev.role.is_none());
        assert!(ev.session_id.is_none());
        assert!(ev.parent_message_id.is_none());
        assert!(ev.content_blocks.is_none());
        assert!(ev.model_id.is_none());
        assert!(ev.stop_reason.is_none());
        assert!(ev.token_usage.is_none());
        assert!(ev.run_id.is_none());
    }

    #[test]
    fn event_builder_sets_all_new_fields() {
        let ev = Event::new("hello")
            .role(Role::Assistant)
            .session("conv-1".into())
            .parent_message("abc123".into())
            .content_blocks(vec![ContentBlock::Text { text: "hi".into() }])
            .model("claude-opus-4.7".into())
            .stop_reason("end_turn".into())
            .token_usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: None,
                cache_creation_tokens: None,
            })
            .run_id("run-7".into());

        assert_eq!(ev.role, Some(Role::Assistant));
        assert_eq!(ev.session_id.as_deref(), Some("conv-1"));
        assert_eq!(ev.parent_message_id.as_deref(), Some("abc123"));
        assert_eq!(ev.content_blocks.as_ref().map(|b| b.len()), Some(1));
        assert_eq!(ev.model_id.as_deref(), Some("claude-opus-4.7"));
        assert_eq!(ev.stop_reason.as_deref(), Some("end_turn"));
        assert!(ev.token_usage.is_some());
        assert_eq!(ev.run_id.as_deref(), Some("run-7"));
    }
}
