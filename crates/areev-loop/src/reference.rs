//! An in-memory reference substrate: a grain map plus a deliberately naive CAL
//! subset. Engine CI runs the full suite against it with zero Areev, so the
//! portability claim stays testable, and it doubles as the conformance kit for
//! third-party substrates (proposal §10).
//!
//! The CAL subset understands exactly the statements built-in analyzers emit
//! (ADD / SUPERSEDE / FORGET / RETRACT) plus read verbs as no-ops. It is not a
//! general CAL engine — a real substrate (Areev) provides that.

use crate::error::{Error, Result};
use crate::model::GrainRecord;
use crate::substrate::{
    Capabilities, GrainSpec, HeadGroup, OmsSubstrate, ReadOpts, SubstrateRead, TelemetryView,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

#[derive(Default)]
pub struct ReferenceSubstrate {
    grains: Vec<GrainRecord>,
    by_hash: HashMap<String, usize>,
    caps: Capabilities,
    state: Value,
    next_id: u64,
    clock: i64,
    /// Entity → competing head hashes (fork surfacing input).
    heads_index: HashMap<String, Vec<String>>,
    /// Injected recall-telemetry snapshot (turns on the `telemetry` capability).
    telemetry: Option<TelemetryView>,
    /// Saved definitions by key (`query:<name>` / `template:<name>`) → the
    /// statement that would restore them. The reference implementation of the
    /// host-metadata registry a real substrate keeps in the file.
    definitions: HashMap<String, String>,
}

impl ReferenceSubstrate {
    pub fn new() -> Self {
        ReferenceSubstrate {
            state: Value::Null,
            // Declared because they are implemented below — the reference
            // substrate is the conformance kit, so the seam it advertises is
            // the seam an implementer must fill.
            caps: Capabilities {
                plans: true,
                code: true,
                ..Capabilities::default()
            },
            ..Default::default()
        }
    }

    pub fn set_capabilities(&mut self, caps: Capabilities) {
        self.caps = caps;
    }

    /// Register a fork (turns on the `forks` capability).
    pub fn register_fork(&mut self, entity: &str, heads: &[&str]) {
        self.caps.forks = true;
        self.heads_index.insert(
            entity.to_string(),
            heads.iter().map(|s| s.to_string()).collect(),
        );
    }

    /// Inject a telemetry snapshot (turns on the `telemetry` capability).
    pub fn set_telemetry(&mut self, view: TelemetryView) {
        self.caps.telemetry = true;
        self.telemetry = Some(view);
    }

    /// Insert a fully-formed grain record; returns its assigned hash.
    pub fn insert(&mut self, mut record: GrainRecord) -> String {
        let hash = if record.hash.is_empty() {
            self.mint_hash()
        } else {
            record.hash.clone()
        };
        record.hash = hash.clone();
        let idx = self.grains.len();
        self.by_hash.insert(hash.clone(), idx);
        self.grains.push(record);
        hash
    }

    fn mint_hash(&mut self) -> String {
        let h = format!("ref-{:08}", self.next_id);
        self.next_id += 1;
        h
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn record_from_spec(&mut self, spec: &GrainSpec) -> GrainRecord {
        let created = self.tick();
        let namespace = spec
            .fields
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or(&spec.namespace)
            .to_string();
        let valid_to_ms = spec.fields.get("valid_to_ms").and_then(Value::as_i64);
        GrainRecord {
            hash: String::new(),
            grain_type: spec.grain_type.clone(),
            namespace,
            created_at_ms: created,
            valid_to_ms,
            superseded_by: None,
            fields: spec.fields.clone(),
        }
    }
}

impl SubstrateRead for ReferenceSubstrate {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    /// A MINIMAL plan check: every node named once, every edge between known
    /// nodes, at least one node. Deliberately weaker than the Areev adapter's,
    /// which hands the body to the runtime's own `PlanGraph::build` — this
    /// crate carries no Areev dependency, and the seam's contract is "refuse a
    /// plan you cannot vouch for", not "reimplement the scheduler".
    fn validate_plan(&self, workflow: &Value) -> Result<()> {
        let bad = |m: &str| Err(Error::Substrate(m.to_string()));
        let Some(fields) = workflow.as_object() else {
            return bad("a workflow body must be a JSON object");
        };
        let Some(Value::Array(nodes)) = fields.get("nodes") else {
            return bad("a workflow needs a 'nodes' array");
        };
        let mut seen = std::collections::BTreeSet::new();
        for n in nodes {
            match n.as_str() {
                Some(id) if seen.insert(id) => {}
                Some(id) => return bad(&format!("duplicate node {id:?}")),
                None => return bad("every workflow node must be a string"),
            }
        }
        if seen.is_empty() {
            return bad("a workflow needs at least one node");
        }
        if let Some(v) = fields.get("edges") {
            let Some(edges) = v.as_array() else {
                return bad("workflow 'edges' must be an array");
            };
            for e in edges {
                for end in ["src", "dst"] {
                    match e.get(end).and_then(Value::as_str) {
                        Some(id) if seen.contains(id) => {}
                        Some(id) => return bad(&format!("edge {end} {id:?} is not a node")),
                        None => return bad(&format!("every edge needs a '{end}'")),
                    }
                }
            }
        }
        Ok(())
    }

    /// Rule E1's pin, from the tool's own live definition grain.
    fn tool_evalset(&self, tool: &str) -> Result<Option<String>> {
        Ok(self
            .grains
            .iter()
            .filter(|g| {
                g.grain_type == crate::model::grain_type::TOOL
                    && g.is_live()
                    && g.str_field("kind") == Some("definition")
                    && g.tool_name() == Some(tool)
            })
            .find_map(|g| {
                g.str_field("evalset_hash")
                    .filter(|h| !h.trim().is_empty())
                    .map(str::to_string)
            }))
    }

    fn grains_of_type(
        &self,
        grain_type: &str,
        namespace: Option<&str>,
        opts: ReadOpts,
    ) -> Result<Vec<GrainRecord>> {
        Ok(self
            .grains
            .iter()
            .filter(|g| g.grain_type == grain_type)
            .filter(|g| namespace.is_none_or(|ns| g.namespace == ns))
            .filter(|g| !opts.live_only || g.is_live())
            .filter(|g| opts.since_ms.is_none_or(|s| g.created_at_ms >= s))
            .cloned()
            .collect())
    }

    fn grain(&self, hash: &str) -> Result<Option<GrainRecord>> {
        Ok(self.by_hash.get(hash).map(|&i| self.grains[i].clone()))
    }

    fn heads(&self, _namespace: Option<&str>) -> Result<Vec<HeadGroup>> {
        if !self.caps.forks {
            return Err(Error::CapabilityMissing("forks".into()));
        }
        let mut groups: Vec<HeadGroup> = self
            .heads_index
            .iter()
            .map(|(entity, heads)| HeadGroup {
                entity: entity.clone(),
                heads: heads.clone(),
            })
            .collect();
        groups.sort_by(|a, b| a.entity.cmp(&b.entity));
        Ok(groups)
    }

    fn telemetry(&self, _namespace: Option<&str>) -> Result<Option<TelemetryView>> {
        Ok(self.telemetry.clone())
    }
}

impl OmsSubstrate for ReferenceSubstrate {
    fn put_grain(&mut self, spec: &GrainSpec) -> Result<String> {
        let record = self.record_from_spec(spec);
        Ok(self.insert(record))
    }

    fn supersede(
        &mut self,
        target_hash: &str,
        spec: &GrainSpec,
        _justification: &str,
    ) -> Result<String> {
        let record = self.record_from_spec(spec);
        let new_hash = self.insert(record);
        let idx = *self
            .by_hash
            .get(target_hash)
            .ok_or_else(|| Error::Substrate(format!("supersede target {target_hash} not found")))?;
        self.grains[idx].superseded_by = Some(new_hash.clone());
        Ok(new_hash)
    }

    fn retract(&mut self, hash: &str, reason: &str) -> Result<()> {
        let idx = *self
            .by_hash
            .get(hash)
            .ok_or_else(|| Error::Substrate(format!("retract target {hash} not found")))?;
        self.grains[idx].superseded_by = Some("retracted".to_string());
        self.grains[idx]
            .fields
            .insert("verification_status".into(), json!("retracted"));
        self.grains[idx]
            .fields
            .insert("retract_reason".into(), json!(reason));
        Ok(())
    }

    fn execute_cal(&mut self, cal: &str) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        for line in cal.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (keyword, rest) = split_keyword(line);
            match keyword.to_ascii_uppercase().as_str() {
                "FORGET" => {
                    let hash = rest.trim();
                    if let Some(&idx) = self.by_hash.get(hash) {
                        self.grains[idx].superseded_by = Some("forgotten".to_string());
                    }
                }
                "RETRACT" => {
                    let hash = rest.trim();
                    self.retract(hash, "cal retract")?;
                }
                "ADD" => {
                    let (grain_type, fields) = parse_type_and_json(rest)?;
                    let spec = GrainSpec {
                        grain_type,
                        namespace: String::new(),
                        fields,
                    };
                    let h = self.put_grain(&spec)?;
                    rows.push(json!({ "hash": h }));
                }
                "SUPERSEDE" => {
                    // SUPERSEDE <hash> WITH <type> {json}
                    let (target, after_with) = rest.split_once(" WITH ").ok_or_else(|| {
                        Error::CalUnsupported(format!("malformed SUPERSEDE: {line}"))
                    })?;
                    let (grain_type, fields) = parse_type_and_json(after_with)?;
                    let spec = GrainSpec {
                        grain_type,
                        namespace: String::new(),
                        fields,
                    };
                    let h = self.supersede(target.trim(), &spec, "cal supersede")?;
                    rows.push(json!({ "hash": h }));
                }
                "DEFINE" => {
                    let (key, _) = definition_key(line).ok_or_else(|| {
                        Error::CalUnsupported(format!("malformed DEFINE: {line}"))
                    })?;
                    self.definitions.insert(key, line.to_string());
                }
                "DROP" => {
                    if let Some((key, _)) = definition_key(line) {
                        self.definitions.remove(&key);
                    }
                }
                // Read verbs: no-ops in the reference substrate (no metric value).
                "RECALL" | "ASSEMBLE" | "EXPLAIN" | "HISTORY" => {}
                other => {
                    return Err(Error::CalUnsupported(format!(
                        "unknown statement {other:?}"
                    )));
                }
            }
        }
        Ok(rows)
    }

    fn validate_cal(&self, cal: &str) -> Result<()> {
        for line in cal.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (keyword, _) = split_keyword(line);
            match keyword.to_ascii_uppercase().as_str() {
                "ADD" | "SUPERSEDE" | "FORGET" | "RETRACT" | "RECALL" | "ASSEMBLE" | "EXPLAIN"
                | "HISTORY" => {}
                "DEFINE" | "DROP" => {
                    if definition_key(line).is_none() {
                        return Err(Error::CalUnsupported(format!(
                            "malformed definition statement: {line}"
                        )));
                    }
                }
                other => {
                    return Err(Error::CalUnsupported(format!(
                        "unknown statement {other:?}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// The statement restoring the CURRENT definition, or a `DROP` when there
    /// is none — so an apply can always record an inverse and a ROLLBACK
    /// always undoes something.
    fn definition_inverse(&self, statement: &str) -> Result<Option<String>> {
        let Some((key, drop_stmt)) = definition_key(statement) else {
            return Ok(None);
        };
        Ok(Some(
            self.definitions.get(&key).cloned().unwrap_or(drop_stmt),
        ))
    }

    fn load_state(&self) -> Result<Value> {
        Ok(self.state.clone())
    }

    fn store_state(&mut self, state: &Value) -> Result<()> {
        self.state = state.clone();
        Ok(())
    }
}

/// Parse a `DEFINE`/`DROP` statement into its registry key and the `DROP`
/// that would remove it. `None` for anything else — this is a reader of two
/// statement shapes, not a CAL parser.
fn definition_key(line: &str) -> Option<(String, String)> {
    let rest = {
        let (kw, rest) = split_keyword(line.trim());
        match kw.to_ascii_uppercase().as_str() {
            "DEFINE" | "DROP" => rest,
            _ => return None,
        }
    };
    let (kind, rest) = split_keyword(rest);
    let kind = kind.to_ascii_uppercase();
    let (name, quoted) = match kind.as_str() {
        "QUERY" => {
            let rest = rest.trim().strip_prefix('"')?;
            (rest.split('"').next()?, true)
        }
        "TEMPLATE" => (split_keyword(rest.trim()).0, false),
        _ => return None,
    };
    if name.is_empty() {
        return None;
    }
    let lower = kind.to_ascii_lowercase();
    let drop_stmt = if quoted {
        format!("DROP {kind} \"{name}\"")
    } else {
        format!("DROP {kind} {name}")
    };
    Some((format!("{lower}:{name}"), drop_stmt))
}

fn split_keyword(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((k, rest)) => (k, rest.trim_start()),
        None => (line, ""),
    }
}

/// Parse `<type> {json}` → (type, fields).
fn parse_type_and_json(s: &str) -> Result<(String, Map<String, Value>)> {
    let brace = s
        .find('{')
        .ok_or_else(|| Error::CalUnsupported(format!("missing JSON object in {s:?}")))?;
    let grain_type = s[..brace].trim().to_string();
    if grain_type.is_empty() {
        return Err(Error::CalUnsupported(format!(
            "missing grain type in {s:?}"
        )));
    }
    let value: Value = serde_json::from_str(s[brace..].trim())
        .map_err(|e| Error::CalUnsupported(format!("bad JSON in {s:?}: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| Error::CalUnsupported(format!("JSON not an object in {s:?}")))?
        .clone();
    Ok((grain_type, obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_makes_grain_not_live() {
        let mut sub = ReferenceSubstrate::new();
        let h = sub
            .put_grain(&GrainSpec::new("fact", "ns").with_field("subject", "x"))
            .unwrap();
        assert_eq!(
            sub.grains_of_type("fact", None, ReadOpts::default())
                .unwrap()
                .len(),
            1
        );
        sub.execute_cal(&format!("FORGET {h}")).unwrap();
        assert!(sub
            .grains_of_type("fact", None, ReadOpts::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn add_returns_hash_and_stores() {
        let mut sub = ReferenceSubstrate::new();
        let rows = sub
            .execute_cal(r#"ADD fact {"subject":"acme","relation":"tier","object":"ent"}"#)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].get("hash").is_some());
        assert_eq!(
            sub.grains_of_type("fact", None, ReadOpts::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn validate_rejects_unknown_statement() {
        let sub = ReferenceSubstrate::new();
        assert!(sub.validate_cal("DROP TABLE").is_err());
        assert!(sub.validate_cal("ADD fact {}").is_ok());
    }

    #[test]
    fn state_round_trips() {
        let mut sub = ReferenceSubstrate::new();
        assert!(sub.load_state().unwrap().is_null());
        sub.store_state(&json!({"k": 1})).unwrap();
        assert_eq!(sub.load_state().unwrap(), json!({"k": 1}));
    }
}
