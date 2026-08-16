//! Google Gemini function-declaration adapter.
//!
//! Shape: {"name":..., "description":..., "parameters":...}
//! Gemini uses the OpenAPI 3.0 subset — the Phase-1 validator already
//! rejects keywords outside that subset (`$ref`, `oneOf`, etc.), so the
//! schema passed through here is already compatible by construction.

use serde_json::{json, Value};

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;

pub fn render(action: &Tool) -> Result<Value> {
    let (name, description, parameters) = definition_parts(action)?;
    Ok(json!({
        "name": name,
        "description": description,
        "parameters": parameters,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn shape_matches_gemini_wire() {
        let v = render(&sample_def()).unwrap();
        assert_eq!(v["name"], "slack_post_message");
        assert_eq!(v["parameters"]["type"], "object");
        // Gemini uses `parameters`, not `input_schema`.
        assert!(v.get("input_schema").is_none());
    }
}
