//! Anthropic Messages API tool adapter.
//!
//! Shape: {"name":..., "description":..., "input_schema":...}
//!
//! Optional: `input_examples` carries grain's `examples` array verbatim
//! when present.

use serde_json::Value;

use crate::error::Result;
use crate::types::Tool;

use super::definition_parts;

pub fn render(action: &Tool) -> Result<Value> {
    let (name, description, input_schema) = definition_parts(action)?;
    let mut obj = serde_json::Map::new();
    obj.insert("name".to_string(), Value::String(name));
    obj.insert("description".to_string(), Value::String(description));
    obj.insert("input_schema".to_string(), input_schema);
    if let Some(ref examples) = action.examples {
        obj.insert("input_examples".to_string(), Value::Array(examples.clone()));
    }
    Ok(Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn shape_has_input_schema() {
        let v = render(&sample_def()).unwrap();
        assert_eq!(v["name"], "slack_post_message");
        assert_eq!(v["input_schema"]["type"], "object");
        assert!(v.get("parameters").is_none(), "anthropic uses input_schema");
    }

    #[test]
    fn examples_surface_when_present() {
        let mut a = sample_def();
        a.examples = Some(vec![serde_json::json!({"channel": "#ops", "text": "hi"})]);
        let v = render(&a).unwrap();
        assert!(v["input_examples"].is_array());
    }
}
