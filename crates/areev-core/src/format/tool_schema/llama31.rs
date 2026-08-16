//! Llama 3.1 prompt-format text envelope.
//!
//! Shape:
//! ```text
//!   Tool: <name>
//!   Description: <sanitized description>
//!   Parameters: <compact JSON schema>
//! ```
//!
//! SR-F1: `<|python_tag|>`, `<|eom_id|>`, `<|eot_id|>` and similar
//! Llama control tokens stripped from the description to prevent
//! tokenizer-aligned injection.

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;
use super::escape::{compact_json, llama31_sanitize};

pub fn render(action: &Tool) -> Result<String> {
    let (name, description, schema) = definition_parts(action)?;
    let cleaned = llama31_sanitize(&description);
    let schema_str = compact_json(&schema);
    Ok(format!(
        "Tool: {name}\nDescription: {cleaned}\nParameters: {schema_str}"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn envelope_has_three_lines() {
        let s = render(&sample_def()).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Tool: slack_post_message"));
        assert!(lines[1].starts_with("Description: "));
        assert!(lines[2].starts_with("Parameters: {"));
    }

    #[test]
    fn control_tokens_stripped() {
        let mut a = sample_def();
        a.tool_description = Some("Use <|python_tag|>eval<|eom_id|> with care".into());
        let s = render(&a).unwrap();
        assert!(!s.contains("<|"));
        assert!(!s.contains("|>"));
        assert!(s.contains("Use"));
        assert!(s.contains("eval"));
    }
}
