//! OpenAI Chat Completions + Responses adapters.
//!
//! Chat Completions shape:
//!   {"type":"function","function":{"name":..., "description":..., "parameters":..., "strict":true}}
//!
//! Responses API shape (flatter):
//!   {"type":"function","name":..., "description":..., "parameters":..., "strict":true}

use serde_json::{json, Value};

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;

pub fn render_tools(action: &Tool) -> Result<Value> {
    let (name, description, parameters) = definition_parts(action)?;
    let strict = action.strict.unwrap_or(true);
    Ok(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
            "strict": strict,
        }
    }))
}

pub fn render_responses(action: &Tool) -> Result<Value> {
    let (name, description, parameters) = definition_parts(action)?;
    let strict = action.strict.unwrap_or(true);
    Ok(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters,
        "strict": strict,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn tools_shape_has_nested_function_block() {
        let v = render_tools(&sample_def()).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "slack_post_message");
        assert_eq!(v["function"]["parameters"]["type"], "object");
        assert_eq!(v["function"]["strict"], true);
    }

    #[test]
    fn responses_shape_is_flat() {
        let v = render_responses(&sample_def()).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["name"], "slack_post_message");
        assert!(v.get("function").is_none(), "responses API flattens");
    }
}
