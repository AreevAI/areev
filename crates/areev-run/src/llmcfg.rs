//! The per-node `llm` block of a plan (#391) and the dynamic tools a node
//! reads from the run input (`tools_from`, #392): validated in full at run
//! start, normalized, and FROZEN into the abstract node's pin in the run
//! manifest — exactly as `reads` are — so a superseded plan, or a host that
//! hands a resume a different input, cannot change what a running run sends.
//!
//! ```json
//! "llm": {
//!   "reply": {
//!     "system": "You are …",
//!     "instruction_from": "/prompt",
//!     "state": ["question", "account"],
//!     "tools": ["records_query"],
//!     "dispatch": ["mcp_call"],
//!     "tools_from": "/tools",
//!     "response": {"json_schema": {…}},
//!     "temperature": "omit",
//!     "max_tokens": 2048,
//!     "parallel_tool_calls": false,
//!     "tool_results_into": "tool_results"
//!   }
//! }
//! ```
//!
//! Refusals name the node (and, for a dynamic tool, the entry): a shape
//! defect is `RUN-E019`; a tool name or a dispatch Definition the plan did not
//! pin is `RUN-E004`, the unresolved-reference code a binding gets.

use crate::manifest::PinnedTool;
use areev_core::format::tool_schema::normalize_tool_name;
use areev_core::types::json_schema_subset::{is_valid_tool_name, strip_annotations, SchemaValidator};
use areev_run_core::{DynamicDispatch, DynamicTool, LlmNodeConfig, PlanGraph, RunError};
use serde_json::Value;
use std::collections::BTreeMap;

/// The plan field the block lives under.
pub const LLM_FIELD: &str = "llm";

const BLOCK_KEYS: &[&str] = &[
    "dispatch",
    "system",
    "instruction",
    "instruction_from",
    "state",
    "tools",
    "tools_from",
    "response",
    "temperature",
    "max_tokens",
    "parallel_tool_calls",
    "tool_results_into",
    "narrow",
];

const ENTRY_KEYS: &[&str] = &["name", "description", "input_schema", "dispatch"];
const DISPATCH_KEYS: &[&str] = &["definition", "locked", "args_into"];

/// Validate the plan's `llm` table against the resolved pins and the run
/// input, and return node → frozen block. Every node named must be in the
/// plan and resolve ABSTRACT: the block configures a model loop, and a node
/// that binds a tool runs no model to configure.
///
/// `resolve` pins one `dispatch` entry (a Definition name or content address)
/// for a node — the store-side half, which this module does not hold. The
/// pins it returns come back beside the blocks, for the manifest's
/// `dispatch_pins`: pinned like a bound Definition (code address, runtime,
/// capabilities, locks), never offered to a model on their own.
pub fn parse_llm(
    field: Option<&Value>,
    plan: &PlanGraph,
    pinned: &[PinnedTool],
    input: &Value,
    resolve: &mut dyn FnMut(&str, &str) -> Result<PinnedTool, RunError>,
) -> Result<(BTreeMap<String, LlmNodeConfig>, Vec<PinnedTool>), RunError> {
    let invalid = |why: String| RunError::InvalidPlan { why };
    let table = match field {
        None | Some(Value::Null) => return Ok((BTreeMap::new(), Vec::new())),
        Some(Value::Object(t)) => t,
        Some(_) => return Err(invalid(format!("`{LLM_FIELD}` must be an object of node → block"))),
    };
    // The universe a node's `tools` subset and a dynamic tool's dispatch are
    // drawn from: the plan's pinned HOST Definitions, as an abstract node is
    // offered them (decision nodes, reads, clients and subgraphs are never
    // model-callable).
    let host: Vec<&PinnedTool> = pinned.iter().filter(|p| p.executor == "host").collect();
    let mut out = BTreeMap::new();
    let mut dispatch_pins: Vec<PinnedTool> = Vec::new();
    for (node, raw) in table {
        if !plan.nodes.iter().any(|n| n == node) {
            return Err(invalid(format!("`{LLM_FIELD}` names unknown node '{node}'")));
        }
        let pin = pinned.iter().find(|p| &p.node == node);
        if pin.map(|p| p.executor.as_str()) != Some("abstract") {
            return Err(invalid(format!(
                "`{LLM_FIELD}` configures node '{node}', which resolves to a {} executor — the \
                 block configures an abstract (LLM) node, and this one runs no model",
                pin.map(|p| p.executor.as_str()).unwrap_or("missing")
            )));
        }
        // `dispatch` (#392): Definitions this node's dynamic tools may reach
        // that no node binds — the blessed `mcp.call`, typically. Resolved
        // first, so the entries below can name them.
        let mut mine: Vec<PinnedTool> = Vec::new();
        match raw.get("dispatch") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                for e in list {
                    let Some(entry) = e.as_str().filter(|s| !s.is_empty()) else {
                        return Err(invalid(format!(
                            "node '{node}' llm: `dispatch` lists {e}, which is not a Definition \
                             name or content address"
                        )));
                    };
                    let pin = resolve(node, entry)?;
                    if !mine.iter().any(|p| p.tool_hash == pin.tool_hash) {
                        mine.push(pin);
                    }
                }
            }
            Some(other) => {
                return Err(invalid(format!(
                    "node '{node}' llm: `dispatch` must be a list of Definition names or \
                     content addresses, not {other}"
                )))
            }
        }
        let mut universe: Vec<&PinnedTool> = mine.iter().collect();
        universe.extend(host.iter().copied());
        let mut cfg = parse_block(node, raw, &host, &universe, input)?;
        cfg.dispatch = mine.iter().map(|p| p.tool_name.clone()).collect();
        out.insert(node.clone(), cfg);
        dispatch_pins.extend(mine);
    }
    Ok((out, dispatch_pins))
}

fn parse_block(
    node: &str,
    raw: &Value,
    host: &[&PinnedTool],
    dispatchable: &[&PinnedTool],
    input: &Value,
) -> Result<LlmNodeConfig, RunError> {
    let bad = |why: String| RunError::InvalidPlan { why: format!("node '{node}' llm: {why}") };
    let Value::Object(b) = raw else {
        return Err(bad("the block must be an object".into()));
    };
    if let Some(k) = b.keys().find(|k| !BLOCK_KEYS.contains(&k.as_str())) {
        return Err(bad(format!("unknown key {k:?} (accepted: {})", BLOCK_KEYS.join(", "))));
    }
    let string = |k: &str| -> Result<Option<String>, RunError> {
        match b.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(bad(format!("`{k}` must be a string, not {other}"))),
        }
    };
    let mut cfg = LlmNodeConfig {
        system: string("system")?,
        instruction: string("instruction")?,
        instruction_from: string("instruction_from")?,
        ..Default::default()
    };
    if cfg.instruction.is_some() && cfg.instruction_from.is_some() {
        return Err(bad("`instruction` and `instruction_from` are exclusive — name one".into()));
    }
    if let Some(p) = &cfg.instruction_from {
        check_pointer(p).map_err(|why| bad(format!("`instruction_from` {why}")))?;
    }

    cfg.state = match b.get("state") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == "all" || s == "none" => Some(Value::String(s.clone())),
        Some(Value::Array(keys))
            if keys.iter().all(|k| k.as_str().is_some_and(|s| !s.is_empty())) =>
        {
            Some(Value::Array(keys.clone()))
        }
        Some(other) => {
            return Err(bad(format!(
                "`state` must be \"all\", \"none\" or a list of state keys, not {other}"
            )))
        }
    };

    // `tools`: a subset of the pinned host Definitions, by canonical name or
    // the provider-safe spelling the model sees; stored canonical, in
    // manifest order, once each.
    cfg.tools = match b.get("tools") {
        None | Some(Value::Null) => None,
        Some(Value::Array(names)) => {
            let mut picked: Vec<String> = Vec::new();
            for n in names {
                let Some(name) = n.as_str() else {
                    return Err(bad(format!("`tools` lists {n}, which is not a tool name")));
                };
                let Some(p) = host
                    .iter()
                    .find(|p| p.tool_name == name)
                    .or_else(|| host.iter().find(|p| normalize_tool_name(&p.tool_name) == name))
                else {
                    return Err(RunError::UnresolvedRef {
                        what: format!(
                            "node '{node}' llm.tools names '{name}', which is not one of the \
                             plan's pinned host tools ({})",
                            pinned_names(host)
                        ),
                    });
                };
                if !picked.contains(&p.tool_name) {
                    picked.push(p.tool_name.clone());
                }
            }
            let mut ordered: Vec<String> = Vec::new();
            for p in host {
                if picked.contains(&p.tool_name) && !ordered.contains(&p.tool_name) {
                    ordered.push(p.tool_name.clone());
                }
            }
            Some(ordered)
        }
        Some(other) => return Err(bad(format!("`tools` must be a list of tool names, not {other}"))),
    };

    if let Some(ptr) = string("tools_from")? {
        check_pointer(&ptr).map_err(|why| bad(format!("`tools_from` {why}")))?;
        cfg.dynamic_tools = parse_dynamic(node, &ptr, input, host, dispatchable)?;
        cfg.tools_from = Some(ptr);
    }

    cfg.response = match b.get("response") {
        None | Some(Value::Null) => None,
        Some(Value::Object(r)) if r.len() == 1 && r.get("json") == Some(&Value::Bool(true)) => {
            Some(serde_json::json!({"json": true}))
        }
        Some(Value::Object(r)) if r.len() == 1 && r.contains_key("json_schema") => {
            let mut schema = r["json_schema"].clone();
            strip_annotations(&mut schema);
            SchemaValidator::tool_schema().validate(&schema).map_err(|e| {
                bad(format!("`response.json_schema` is not a usable schema: {e}"))
            })?;
            if let Some(why) = strict_mode_violation(&schema, "") {
                return Err(bad(format!(
                    "`response.json_schema` is sent in strict mode, which needs every object \
                     closed and every property required: {why}"
                )));
            }
            Some(serde_json::json!({"json_schema": schema}))
        }
        Some(other) => {
            return Err(bad(format!(
                "`response` must be {{\"json_schema\": {{…}}}} or {{\"json\": true}}, not {other}"
            )))
        }
    };

    cfg.temperature = match b.get("temperature") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == "omit" => Some(Value::String("omit".into())),
        Some(Value::Number(n)) if n.as_f64().is_some_and(|t| t.is_finite() && t >= 0.0) => {
            Some(Value::Number(n.clone()))
        }
        Some(other) => {
            return Err(bad(format!(
                "`temperature` must be a non-negative number or \"omit\", not {other}"
            )))
        }
    };

    cfg.max_tokens = match b.get("max_tokens") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64().filter(|n| *n >= 1).and_then(|n| u32::try_from(n).ok()) {
            Some(n) => Some(n),
            None => {
                return Err(bad(format!(
                    "`max_tokens` must be a positive integer (at most {}), not {v}",
                    u32::MAX
                )))
            }
        },
    };

    cfg.parallel_tool_calls = match b.get("parallel_tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(p)) => Some(*p),
        Some(other) => return Err(bad(format!("`parallel_tool_calls` must be a boolean, not {other}"))),
    };

    cfg.tool_results_into = match string("tool_results_into")? {
        None => None,
        Some(k) if !k.is_empty() && !k.starts_with('$') => Some(k),
        Some(k) => {
            return Err(bad(format!(
                "`tool_results_into` must be a non-empty state key not starting with '$', got {k:?}"
            )))
        }
    };
    // `narrow` (#396): whether, and to how many, a calibrated decision
    // backend narrows this node's offer. `true` is the run-wide rule and is
    // stored as nothing, so it changes no manifest's bytes.
    cfg.narrow = match b.get("narrow") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => None,
        Some(Value::Bool(false)) => Some(Value::Bool(false)),
        Some(Value::Object(o))
            if o.len() == 1
                && o.get("top_k").and_then(Value::as_u64).is_some_and(|k| (1..=254).contains(&k)) =>
        {
            Some(serde_json::json!({"top_k": o["top_k"]}))
        }
        Some(other) => {
            return Err(bad(format!(
                "`narrow` must be true, false or {{\"top_k\": 1-254}}, not {other}"
            )))
        }
    };
    Ok(cfg)
}

/// A JSON pointer as RFC 6901 writes one: empty (the whole document) or
/// starting with `/`.
fn check_pointer(p: &str) -> Result<(), String> {
    if p.is_empty() || p.starts_with('/') {
        Ok(())
    } else {
        Err(format!("{p:?} is not a JSON pointer (it must start with '/')"))
    }
}

fn pinned_names(host: &[&PinnedTool]) -> String {
    let mut names: Vec<&str> = host.iter().map(|p| p.tool_name.as_str()).collect();
    names.dedup();
    if names.is_empty() {
        "the plan pins none".into()
    } else {
        format!("pinned: {}", names.join(", "))
    }
}

/// Resolve `tools_from` against the run input and pin every entry.
///
/// `host` is what a dynamic name must not collide with (the tools a model may
/// be offered); `dispatchable` is what an entry may dispatch through — the
/// node's `dispatch` pins, then the plan's host pins.
fn parse_dynamic(
    node: &str,
    ptr: &str,
    input: &Value,
    host: &[&PinnedTool],
    dispatchable: &[&PinnedTool],
) -> Result<Vec<DynamicTool>, RunError> {
    let bad = |why: String| RunError::InvalidPlan { why: format!("node '{node}' llm.tools_from: {why}") };
    let list = match input.pointer(ptr) {
        Some(Value::Array(list)) => list,
        Some(other) => {
            return Err(bad(format!("{ptr:?} resolves to {other} in the run input, not a list of tools")))
        }
        None => return Err(bad(format!("{ptr:?} resolves to nothing in the run input"))),
    };
    let mut out: Vec<DynamicTool> = Vec::with_capacity(list.len());
    for (n, entry) in list.iter().enumerate() {
        let label = entry
            .get("name")
            .and_then(Value::as_str)
            .map(|s| format!("entry {n} ('{s}')"))
            .unwrap_or_else(|| format!("entry {n}"));
        let ebad = |why: String| bad(format!("{label}: {why}"));
        let Value::Object(e) = entry else {
            return Err(ebad("must be an object".into()));
        };
        if let Some(k) = e.keys().find(|k| !ENTRY_KEYS.contains(&k.as_str())) {
            return Err(ebad(format!("unknown key {k:?} (accepted: {})", ENTRY_KEYS.join(", "))));
        }
        let name = match e.get("name").and_then(Value::as_str) {
            Some(s) if is_valid_tool_name(s) => s.to_string(),
            _ => {
                return Err(ebad(
                    "`name` must be a valid tool name (1-64 chars of [A-Za-z0-9_.-])".into(),
                ))
            }
        };
        // A provider sees `a.b` as `a_b`, so two names that normalize alike
        // would be one name on the wire — refused like an exact duplicate.
        let wire = normalize_tool_name(&name);
        if out.iter().any(|d| d.name == name || normalize_tool_name(&d.name) == wire) {
            return Err(ebad(format!("duplicate tool name '{name}'")));
        }
        if host.iter().any(|p| p.tool_name == name || normalize_tool_name(&p.tool_name) == wire) {
            return Err(ebad(format!(
                "'{name}' collides with a tool the plan pins — a dynamic tool needs a name of \
                 its own"
            )));
        }
        let description = match e.get("description") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => return Err(ebad(format!("`description` must be a string, not {other}"))),
        };
        let input_schema = match e.get("input_schema") {
            None | Some(Value::Null) => serde_json::json!({"type": "object", "properties": {}}),
            Some(s) => {
                // Annotations an MCP server publishes (`$schema`, `$id`, …)
                // are accepted and dropped, never a refusal (#397); the
                // pinned, offered and validating schema is the clean form.
                let mut s = s.clone();
                strip_annotations(&mut s);
                SchemaValidator::tool_schema()
                    .validate(&s)
                    .map_err(|err| ebad(format!("`input_schema` is not a usable schema: {err}")))?;
                s
            }
        };
        let Some(Value::Object(d)) = e.get("dispatch") else {
            return Err(ebad("`dispatch` must be an object naming a pinned Definition".into()));
        };
        if let Some(k) = d.keys().find(|k| !DISPATCH_KEYS.contains(&k.as_str())) {
            return Err(ebad(format!(
                "unknown dispatch key {k:?} (accepted: {})",
                DISPATCH_KEYS.join(", ")
            )));
        }
        let Some(target) = d.get("definition").and_then(Value::as_str) else {
            return Err(ebad("`dispatch.definition` must name a pinned Definition".into()));
        };
        let Some(def) = dispatchable
            .iter()
            .find(|p| p.tool_name == target)
            .or_else(|| dispatchable.iter().find(|p| p.tool_hash.eq_ignore_ascii_case(target)))
            .or_else(|| dispatchable.iter().find(|p| normalize_tool_name(&p.tool_name) == target))
        else {
            return Err(RunError::UnresolvedRef {
                what: format!(
                    "node '{node}' llm.tools_from {label} dispatches through '{target}', which \
                     is neither in the node's `dispatch` list nor a host tool the plan pins \
                     ({}) — a dynamic tool may only reach a Definition the plan's reviewer saw",
                    pinned_names(dispatchable)
                ),
            });
        };
        let locked = match d.get("locked") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(Value::Object(o.clone())),
            Some(other) => return Err(ebad(format!("`dispatch.locked` must be an object, not {other}"))),
        };
        let args_into = match d.get("args_into") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(other) => {
                return Err(ebad(format!(
                    "`dispatch.args_into` must be a non-empty key, not {other}"
                )))
            }
        };
        // Locks are TOP-LEVEL keys of the Definition's input. A dotted key
        // would be inserted literally (`"a.b"`, not `a` → `b`), and a lock on
        // the `args_into` key would replace the model's arguments wholesale —
        // the Definition's own `locked_params` included, which merge last.
        let lock_keys = locked
            .iter()
            .chain(def.locked_params.iter())
            .filter_map(Value::as_object)
            .flat_map(|o| o.keys());
        for k in lock_keys {
            if k.contains('.') {
                return Err(ebad(format!(
                    "lock key '{k}' is dotted — locks set top-level keys of the Definition's \
                     input only"
                )));
            }
            if args_into.as_deref() == Some(k.as_str()) {
                return Err(ebad(format!(
                    "lock key '{k}' is the `args_into` key, so it would replace the model's \
                     arguments"
                )));
            }
        }
        out.push(DynamicTool {
            name,
            description,
            input_schema,
            dispatch: DynamicDispatch {
                definition: def.tool_name.clone(),
                hash: def.tool_hash.clone(),
                locked,
                args_into,
                // Filled from the store by the caller, which holds it.
                definition_schema: None,
            },
        });
    }
    Ok(out)
}

/// `over` merged over `base` at the top level, `over` winning — how a
/// Definition's own `locked_params` sit over a dynamic entry's `locked`.
pub(crate) fn merge_locks(base: Option<&Value>, over: Option<&Value>) -> Option<Value> {
    match (base, over) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(Value::Object(b)), Some(Value::Object(o))) => {
            let mut m = b.clone();
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
            Some(Value::Object(m))
        }
        (_, Some(o)) => Some(o.clone()),
    }
}

/// Remove locked keys from a schema the model is OFFERED (#393): from
/// `properties` and from `required`. The pure rule lives in run-core, which
/// validates the model's arguments against the same stripped form.
pub(crate) fn strip_locked(schema: &mut Value, locked: &Value) {
    areev_run_core::types::strip_locked_keys(schema, locked);
}

/// OpenAI's strict structured output (`"strict": true`, which the runtime
/// sends) accepts a schema only when every object closes itself
/// (`additionalProperties: false`) and lists every property in `required`.
/// Checked at start, naming the first violation by JSON pointer, rather than
/// learnt from a 400 on the node's first turn.
fn strict_mode_violation(schema: &Value, at: &str) -> Option<String> {
    let Value::Object(o) = schema else { return None };
    let is_object = o.get("type").and_then(Value::as_str) == Some("object") || o.contains_key("properties");
    if is_object {
        if o.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Some(format!("{} must set \"additionalProperties\": false", if at.is_empty() { "/" } else { at }));
        }
        let required: Vec<&str> = o
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if let Some(Value::Object(props)) = o.get("properties") {
            for (k, sub) in props {
                if !required.contains(&k.as_str()) {
                    return Some(format!(
                        "{}: property '{k}' must be listed in \"required\"",
                        if at.is_empty() { "/" } else { at }
                    ));
                }
                if let Some(v) = strict_mode_violation(sub, &format!("{at}/properties/{k}")) {
                    return Some(v);
                }
            }
        }
    }
    if let Some(items) = o.get("items") {
        return strict_mode_violation(items, &format!("{at}/items"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn locked_keys_leave_the_offered_schema_and_its_required_list() {
        let mut schema = json!({"type": "object",
            "properties": {"url": {"type": "string"}, "body": {"type": "string"}},
            "required": ["url", "body"]});
        strip_locked(&mut schema, &json!({"url": "https://a.example/x"}));
        assert_eq!(
            schema,
            json!({"type": "object", "properties": {"body": {"type": "string"}}, "required": ["body"]})
        );
    }

    #[test]
    fn a_definitions_own_lock_wins_over_a_dynamic_entrys() {
        assert_eq!(
            merge_locks(Some(&json!({"url": "a", "tool": "t"})), Some(&json!({"url": "b"}))),
            Some(json!({"url": "b", "tool": "t"}))
        );
        assert_eq!(merge_locks(None, None), None);
    }
}
