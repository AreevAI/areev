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

/// Prepare `parameters` for OpenAI's STRICT function calling, and say whether
/// the claim can honestly be made.
///
/// Strict mode is not a flag the caller may simply assert: OpenAI validates the
/// schema and rejects the whole request with `invalid_function_parameters` when
/// it does not qualify. Two rules matter here.
///
/// **Every object must be closed.** `additionalProperties: false` is required
/// and is added recursively — a tightening the author already asked for by
/// requesting strict, and one that cannot change which calls are valid, only
/// which are rejected as malformed.
///
/// **Every property must be required.** That one is NOT imposed: forcing a
/// genuinely optional argument to be supplied changes what the tool means, and
/// silently rewriting an author's contract is worse than not claiming strict.
/// So a schema with optional properties renders `strict: false` — the call goes
/// out and works, without a claim the provider would refuse.
///
/// The schema is touched ONLY when the claim is kept. Deciding first and
/// mutating second is the whole point: a schema we are about to render
/// unstrict must come out exactly as its author wrote it, or `strict` has
/// become a licence to edit other people's contracts.
///
/// Returns `(parameters, strict_is_honest)`.
fn strictify(mut parameters: Value, want_strict: bool) -> (Value, bool) {
    if !want_strict || !qualifies_for_strict(&parameters) {
        return (parameters, false);
    }
    close_objects(&mut parameters);
    (parameters, true)
}

/// Can strict's all-properties-required rule hold for every object in here?
///
/// Read-only. An optional property disqualifies the schema: OpenAI's strict
/// mode has no notion of one (the documented workaround is a nullable union,
/// which is a change of meaning only the author may make).
fn qualifies_for_strict(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("object") {
                let required: Vec<&str> = map
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|r| r.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let all_required = map
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|p| p.keys().all(|k| required.contains(&k.as_str())))
                    .unwrap_or(true);
                if !all_required {
                    return false;
                }
            }
            map.values().all(qualifies_for_strict)
        }
        Value::Array(items) => items.iter().all(qualifies_for_strict),
        _ => true,
    }
}

/// Add `additionalProperties: false` to every object schema. Only ever called
/// on a schema that already [`qualifies_for_strict`].
fn close_objects(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("object") {
                map.insert("additionalProperties".into(), Value::Bool(false));
            }
            for (_, child) in map.iter_mut() {
                close_objects(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(close_objects),
        _ => {}
    }
}

pub fn render_tools(action: &Tool) -> Result<Value> {
    let (name, description, parameters) = definition_parts(action)?;
    let (parameters, strict) = strictify(parameters, action.strict.unwrap_or(true));
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
    let (parameters, strict) = strictify(parameters, action.strict.unwrap_or(true));
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

    /// The bug this exists for: OpenAI REJECTS a strict function whose schema
    /// omits `additionalProperties: false`, with
    /// `invalid_function_parameters`, and the whole request 400s. Because
    /// `strict` defaults to true, that hit every Definition whose author had
    /// not hand-written the key — which is to say almost all of them. Fixture
    /// tests could not see it; a live provider call found it immediately.
    #[test]
    fn a_strict_function_closes_every_object_in_its_schema() {
        let mut a = sample_def();
        a.strict = Some(true);
        a.input_schema = Some(json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "opts": {
                    "type": "object",
                    "properties": {"pin": {"type": "boolean"}},
                    "required": ["pin"]
                }
            },
            "required": ["channel", "opts"]
        }));
        let v = render_tools(&a).unwrap();
        let p = &v["function"]["parameters"];
        assert_eq!(v["function"]["strict"], true);
        assert_eq!(p["additionalProperties"], false, "top level closed: {p}");
        assert_eq!(
            p["properties"]["opts"]["additionalProperties"], false,
            "nested objects closed too: {p}"
        );
    }

    /// Strict also demands that EVERY property be required. That one is not
    /// imposed — forcing an optional argument changes what the tool means, and
    /// rewriting an author's contract to make a claim true is worse than not
    /// making it. The call still goes out; it just goes out unstrict.
    #[test]
    fn an_optional_property_downgrades_the_claim_rather_than_the_schema() {
        let mut a = sample_def();
        a.strict = Some(true);
        a.input_schema = Some(json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "thread": {"type": "string"}},
            "required": ["channel"]
        }));
        let v = render_tools(&a).unwrap();
        assert_eq!(
            v["function"]["strict"], false,
            "cannot honestly claim strict with an optional property"
        );
        assert_eq!(
            v["function"]["parameters"]["required"],
            json!(["channel"]),
            "and the author's contract is left exactly as written"
        );
        assert!(
            v["function"]["parameters"].get("additionalProperties").is_none(),
            "including no closing we were not entitled to add: {}",
            v["function"]["parameters"]
        );
    }

    /// A no-argument tool — the shape a live run actually died on.
    #[test]
    fn a_no_argument_tool_renders_strict_and_closed() {
        let mut a = sample_def();
        a.strict = None; // the default path
        a.input_schema = Some(json!({"type": "object", "properties": {}}));
        let v = render_tools(&a).unwrap();
        assert_eq!(v["function"]["strict"], true);
        assert_eq!(v["function"]["parameters"]["additionalProperties"], false);
    }

    /// An author who says `strict: false` gets their schema untouched: closing
    /// objects is a consequence of the strict claim, not a house style.
    #[test]
    fn strict_false_leaves_the_schema_alone() {
        let mut a = sample_def();
        a.strict = Some(false);
        a.input_schema = Some(json!({"type": "object", "properties": {}}));
        let v = render_tools(&a).unwrap();
        assert_eq!(v["function"]["strict"], false);
        assert!(
            v["function"]["parameters"].get("additionalProperties").is_none(),
            "untouched: {}",
            v["function"]["parameters"]
        );
    }

    #[test]
    fn responses_shape_is_flat() {
        let v = render_responses(&sample_def()).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["name"], "slack_post_message");
        assert!(v.get("function").is_none(), "responses API flattens");
    }
}
