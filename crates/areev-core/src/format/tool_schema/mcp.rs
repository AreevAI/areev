//! MCP `tools/list` adapter — MCP 2025-06-18 spec.
//!
//! Shape: {"name":..., "description":..., "inputSchema":..., "outputSchema"?:...,
//!         "annotations"?: {"readOnlyHint":bool, "destructiveHint":bool, "idempotentHint":bool}}

use serde_json::{json, Value};

use crate::error::Result;
use crate::types::{Tool, ToolAnnotations};

use super::definition_parts;

pub fn render(action: &Tool) -> Result<Value> {
    let (name, description, input_schema) = definition_parts(action)?;
    let mut obj = serde_json::Map::new();
    obj.insert("name".to_string(), Value::String(name));
    obj.insert("description".to_string(), Value::String(description));
    obj.insert("inputSchema".to_string(), input_schema);
    if let Some(ref o) = action.output_schema {
        obj.insert("outputSchema".to_string(), o.clone());
    }
    if let Some(ref a) = action.annotations {
        obj.insert("annotations".to_string(), mcp_annotations(a));
    }
    Ok(Value::Object(obj))
}

fn mcp_annotations(a: &ToolAnnotations) -> Value {
    json!({
        "readOnlyHint": a.read_only,
        "destructiveHint": a.destructive,
        "idempotentHint": a.idempotent,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample_def;
    use super::*;

    #[test]
    fn shape_uses_camelcase_mcp_names() {
        let v = render(&sample_def()).unwrap();
        assert_eq!(v["name"], "slack_post_message");
        assert!(v.get("inputSchema").is_some());
        assert!(v.get("input_schema").is_none(), "MCP uses camelCase");
    }

    #[test]
    fn annotations_carry_through() {
        let mut a = sample_def();
        a.annotations = Some(ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: true,
        });
        let v = render(&a).unwrap();
        assert_eq!(v["annotations"]["idempotentHint"], true);
    }
}
