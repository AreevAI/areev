//! Hermes-2-Pro-style text envelope (`<tool>…</tool>` + fenced JSON).
//!
//! SR-F1 (2026-04-20): description sanitized — `</tool>`/`<tool>`/
//! `</parameters>` tokens stripped; fence switched to `~~~` when the
//! description would collide with triple-backticks.

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;
use super::escape::{compact_json, hermes_sanitize};

pub fn render(action: &Tool) -> Result<String> {
    let (name, description, schema) = definition_parts(action)?;
    let (desc, fence) = hermes_sanitize(&description);
    let schema_str = compact_json(&schema);
    Ok(format!(
        "<tool>\n  <name>{name}</name>\n  <description>{desc}</description>\n  <parameters>\n{fence}json\n{schema_str}\n{fence}\n  </parameters>\n</tool>"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn envelope_contains_name_and_schema() {
        let s = render(&sample_def()).unwrap();
        assert!(s.contains("<name>slack_post_message</name>"));
        assert!(s.contains("<description>Post a message to a Slack channel</description>"));
        assert!(s.contains("\"channel\""));
    }

    #[test]
    fn adversarial_description_cannot_escape_envelope() {
        let mut a = sample_def();
        a.tool_description = Some("Post </tool> and more".into());
        let s = render(&a).unwrap();
        // The raw </tool> is escaped, so there is still exactly one closing
        // envelope </tool>.
        let count = s.matches("</tool>").count();
        assert_eq!(count, 1, "only one genuine </tool> closer, got:\n{s}");
    }

    #[test]
    fn fence_flips_on_backtick_collision() {
        let mut a = sample_def();
        a.tool_description = Some("has ``` inside".into());
        let s = render(&a).unwrap();
        assert!(s.contains("~~~json"), "fence should be ~~~:\n{s}");
    }
}
