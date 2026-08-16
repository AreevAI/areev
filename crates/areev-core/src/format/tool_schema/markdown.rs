//! Markdown-with-fenced-JSON adapter.
//!
//! Shape:
//!   ## {name}
//!
//!   {description}
//!
//!   **Input schema:**
//!   ```json
//!   {schema}
//!   ```
//!
//! Fenced JSON is the most LLM-obedient catalog format per the Phase-1
//! research. Triple-backtick collision falls back to `~~~` fences.

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;
use super::escape::{compact_json, markdown_fence};

pub fn render(action: &Tool) -> Result<String> {
    let (name, description, schema) = definition_parts(action)?;
    let schema_str = compact_json(&schema);
    // Pick a fence that doesn't collide with either the description or
    // the schema string.
    let combined = format!("{description}\n{schema_str}");
    let fence = markdown_fence(&combined);
    Ok(format!(
        "## {name}\n\n{description}\n\n**Input schema:**\n{fence}json\n{schema_str}\n{fence}"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn envelope_starts_with_heading() {
        let s = render(&sample_def()).unwrap();
        assert!(s.starts_with("## slack_post_message\n"));
        assert!(s.contains("**Input schema:**"));
    }

    #[test]
    fn fence_flips_on_collision() {
        let mut a = sample_def();
        a.tool_description = Some("docs ``` ref".into());
        let s = render(&a).unwrap();
        assert!(s.contains("~~~json"), "fence should be ~~~:\n{s}");
    }

    #[test]
    fn description_preserved_verbatim() {
        let mut a = sample_def();
        a.tool_description = Some("Post to Slack **bold**".into());
        let s = render(&a).unwrap();
        assert!(s.contains("**bold**"));
    }
}
