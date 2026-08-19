//! JSON → typed grain construction (the engine write path).
//! Used by the CAL facade for ADD / SUPERSEDE tier-1 statements.

use std::collections::BTreeMap;
use areev_core::error::{AreevError, Hash, Result};
use areev_core::types::*;

const COMMON_KNOWN_FIELDS: &[&str] = &[
    "namespace",
    "user_id",
    "confidence",
    "source_type",
    "created_at",
    "importance",
    "embedding_text",
    "tags",
    "structural_tags",
    "temporal_type",
    "valid_from",
    "valid_to",
    "system_valid_from",
    "system_valid_to",
    "author_did",
    "origin_did",
    "origin_namespace",
    "derived_from",
    "consolidation_level",
    "success_count",
    "failure_count",
    "superseded_by",
    "verification_status",
    "context",
    "invalidation_policy",
    "content_refs",
    "embedding_refs",
    "provenance_chain",
    "related_to",
    "supersession_justification",
    "supersession_auth",
    "type",
    "grain_type",
];

/// The fields [`build_grain_from_json`] refuses to build a grain without.
///
/// This is the *declaration* of what the builder arms below enforce, exposed so
/// `DESCRIBE <type>` can answer "what does this type need" without a caller
/// discovering it one `VAL-E001` at a time. The two are pinned together by
/// `required_fields_match_the_validator` — add a `require_str` and the test
/// fails until this table agrees.
///
/// `goal` is the one inexact row: it accepts `object` as a fallback for
/// `description`, so the entry names the primary spelling.
/// `state`/`workflow`/`reasoning`/`consensus` require nothing by design — they
/// are host-shaped containers, so an empty one is legal.
pub fn required_fields(grain_type: &str) -> &'static [&'static str] {
    match grain_type {
        "fact" => &["subject", "relation", "object"],
        "event" => &["content"],
        "tool" => &["tool_name"],
        "observation" => &["content"],
        "goal" => &["description"],
        "consent" => &["subject_did", "user_id"],
        "skill" => &["name", "description"],
        // Unconditional only. A trigger also has *kind-specific* requirements
        // (an interval trigger needs `interval_secs`, a schedule one needs
        // `cron`, a composite one needs members and a predicate) which cannot
        // be expressed in a flat list; `Trigger::incoherence` enforces those and
        // names the missing piece.
        "trigger" => &["kind", "workflow"],
        _ => &[],
    }
}

/// Why `build_grain_from_json` refused a type name.
///
/// The builder used to answer both cases with one sentence — "unknown grain
/// type: '<name>'" followed by a hardcoded list of eleven. For `recommendation`
/// that was simply false: `DESCRIBE` lists twelve types and CAL `ADD` explains
/// that this one is engine-emitted, so `add()` was the only surface claiming the
/// type does not exist. A caller cannot act on a wrong reason.
fn unbuildable_type_message(grain_type: &str) -> String {
    use areev_core::types::registry;
    let addable: Vec<&str> = registry::host_addable_names().collect();
    match registry::from_str(grain_type).map(registry::meta) {
        // A real OMS type, deliberately not host-authored.
        Some(m) if !m.host_addable => format!(
            "grain type '{}' is engine-authored and cannot be created by a host. \
             It is emitted by an analyzer and moved through review by the audit path — \
             query it with RECALL {}. Host-addable types: {}.",
            grain_type,
            m.plural,
            addable.join(", ")
        ),
        // A real type marked addable with no builder arm behind it. Unreachable
        // while `builder_accepts_exactly_the_host_addable_types` is green, so
        // say what is actually wrong rather than inventing a policy reason.
        Some(m) => format!(
            "grain type '{}' is declared host-addable but has no builder — this is \
             an Areev bug, please report it. Types that do build: {}.",
            m.name,
            addable.join(", ")
        ),
        None => format!(
            "unknown grain type: '{}'. Valid types: {}.",
            grain_type,
            addable.join(", ")
        ),
    }
}

/// Per-grain-type known fields (type-specific, not in common).
fn type_known_fields(grain_type: &str) -> &'static [&'static str] {
    match grain_type {
        "fact" => &["subject", "relation", "object"],
        "event" => &[
            "content",
            "subject",
            "object",
            "role",
            "session_id",
            "parent_message_id",
            "content_blocks",
            "model_id",
            "stop_reason",
            "token_usage",
            "run_id",
        ],
        "state" => &["data", "context_data", "plan", "history"],
        // `name`/`status` are deliberately absent: OMS §8.4 has no such Workflow
        // fields, so they fall through to `extra_fields` and serialize verbatim at
        // top level, where the templates' `{{name}}`/`{{status}}` can find them.
        // Listing them here routed both into `common.context`, where `field_str`
        // (top-level only) never looked.
        "workflow" => &["nodes", "edges", "bindings", "retries", "trigger"],
        "trigger" => &[
            "kind",
            "workflow",
            "connector",
            "scope",
            "enabled",
            "dedup_key",
            "interval_secs",
            "cron",
            "at_ms",
            "predicate",
            "members",
            "correlate",
            "window_ms",
            "concurrency",
            "catchup",
            "config",
        ],
        "tool" => &[
            "tool_name",
            "input",
            "content",
            "is_error",
            "error",
            "duration_ms",
            "parent_task_id",
            "output_schema",
            // A first-class Tool field (compact key `tcid`, queryable per the
            // registry) that the builder arm below now sets. Listing it keeps
            // `collect_extra_fields` from also copying it into `extra_fields`.
            "tool_call_id",
            // Definition vocabulary + async execution lifecycle. All of these
            // are typed `Tool` fields with compact keys since Phase 1, but no
            // builder arm consumed them, so a Definition written through
            // `add()`/CAL/MCP/the bindings carried its schema and executor as
            // fields the typed readers never saw — and, being "known", they
            // were excluded from `extra_fields` too, i.e. silently dropped.
            // Each now has a strict builder arm below (unknown enum strings
            // are errors naming the accepted values, never a silent default).
            "kind",
            "tool_description",
            "input_schema",
            "strict",
            "executor_uri",
            "locked_params",
            "examples",
            "annotations",
            "spec_hash",
            "executor_kind",
            "async_mode",
            "status",
            "correlation_id",
            "call_batch_id",
            "expires_at_sec",
            "failure_cause",
            "failure_detail",
            "actor_execution_environment",
        ],
        "observation" => &[
            "content",
            "observer_id",
            "observer_type",
            "subject",
            "object",
            "observer_model",
            "frame_id",
            "sync_group",
            "observation_mode",
            "observation_scope",
            "compression_ratio",
        ],
        "goal" => &[
            "description",
            "goal_state",
            "subject",
            "object",
            "priority",
            "criteria",
            "criteria_structured",
            "parent_goals",
            "state_reason",
            "satisfaction_evidence",
            "progress",
            "delegate_to",
            "delegate_from",
            "expiry_policy",
            "recurrence",
            "evidence_required",
            "rollback_on_failure",
            "allowed_transitions",
        ],
        "reasoning" => &[
            "premises",
            "conclusion",
            "inference_method",
            "alternatives_considered",
            "thinking_content",
            "thinking_redacted",
        ],
        "consensus" => &[
            "participating_observers",
            "threshold",
            "agreement_count",
            "dissent_count",
            "dissent_grains",
            "agreed_content",
        ],
        "consent" => &[
            "subject_did",
            "grantee_did",
            "scope",
            "is_withdrawal",
            "basis",
            "jurisdiction",
            "prior_consent",
            "witness_dids",
        ],
        "skill" => &[
            "name",
            "description",
            "instructions",
            "when_to_use",
            "version",
            "allowed_tools",
            "resources",
            "dependencies",
            "input_modalities",
            "output_modalities",
            "domain",
            "holder_did",
            "proficiency",
            "practice_count",
            "last_practiced_at",
            "strategies",
            "transferable",
        ],
        _ => &[],
    }
}


fn collect_extra_fields(
    fields: &serde_json::Map<String, serde_json::Value>,
    grain_type: &str,
) -> BTreeMap<String, serde_json::Value> {
    let type_known = type_known_fields(grain_type);
    fields
        .iter()
        .filter(|(k, v)| {
            !v.is_null()
                && !COMMON_KNOWN_FIELDS.contains(&k.as_str())
                && !type_known.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build a `Workflow` from JSON fields, parsing nodes, edges, bindings, retries,
/// and trigger. Validates referential integrity of all graph components.
/// Build a [`Trigger`] from JSON fields.
///
/// Refuses an incoherent declaration here rather than letting it be stored and
/// discovered later as silence — a trigger that can never fire is the failure
/// mode nobody notices, because its symptom is nothing happening.
fn build_trigger_from_json(
    fields: &serde_json::Map<String, serde_json::Value>,
    get_str: &dyn Fn(&str) -> Option<String>,
) -> Result<Trigger> {
    let kind_str = get_str("kind").ok_or_else(|| {
        AreevError::Validation("ADD trigger requires 'kind'".into())
    })?;
    let kind = TriggerKind::parse(&kind_str).ok_or_else(|| {
        AreevError::Validation(format!(
            "unknown trigger kind '{kind_str}' (interval|schedule|once|polling|memory|webhook|manual|composite)"
        ))
    })?;
    let workflow = get_str("workflow")
        .ok_or_else(|| AreevError::Validation("ADD trigger requires 'workflow'".into()))?;

    let strings = |field: &str| -> Vec<String> {
        fields
            .get(field)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };

    let mut t = Trigger::new(kind, &workflow);
    t.connector = get_str("connector");
    t.scope = get_str("scope");
    t.enabled = fields.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    t.dedup_key = strings("dedup_key");
    t.interval_secs = fields
        .get("interval_secs")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok());
    t.cron = get_str("cron");
    t.at_ms = fields.get("at_ms").and_then(|v| v.as_i64());
    t.predicate = fields.get("predicate").cloned();
    t.members = fields
        .get("members")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    t.correlate = get_str("correlate");
    t.window_ms = fields.get("window_ms").and_then(|v| v.as_i64());
    if let Some(c) = get_str("concurrency") {
        t.concurrency = Concurrency::parse(&c).ok_or_else(|| {
            AreevError::Validation(format!("unknown concurrency '{c}' (forbid|allow|replace)"))
        })?;
    }
    if let Some(c) = get_str("catchup") {
        t.catchup = Catchup::parse(&c).ok_or_else(|| {
            AreevError::Validation(format!("unknown catchup '{c}' (last|none|all)"))
        })?;
    }
    t.config = fields.get("config").cloned();

    if let Some(why) = t.incoherence() {
        return Err(AreevError::Validation(why));
    }
    Ok(t)
}

fn build_workflow_from_json(
    fields: &serde_json::Map<String, serde_json::Value>,
    get_str: &dyn Fn(&str) -> Option<String>,
) -> Result<Workflow> {
    // nodes — required, array of strings
    let nodes: Vec<String> = fields
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Validate: node IDs must be unique
    {
        let mut seen = std::collections::HashSet::with_capacity(nodes.len());
        for n in &nodes {
            if !seen.insert(n.as_str()) {
                return Err(AreevError::Validation(format!(
                    "workflow nodes contain duplicate ID: '{}'",
                    n
                )));
            }
        }
    }

    let mut wf = Workflow::new(nodes.clone());

    // edges — optional, array of objects {src, dst, cond?, max_cycles?}
    if let Some(edges_arr) = fields.get("edges").and_then(|v| v.as_array()) {
        for (i, edge_val) in edges_arr.iter().enumerate() {
            let obj = edge_val.as_object().ok_or_else(|| {
                AreevError::Validation(format!("workflow edges[{}] must be an object", i))
            })?;
            let src = obj.get("src").and_then(|v| v.as_str()).ok_or_else(|| {
                AreevError::Validation(format!("workflow edges[{}] requires string 'src'", i))
            })?;
            let dst = obj.get("dst").and_then(|v| v.as_str()).ok_or_else(|| {
                AreevError::Validation(format!("workflow edges[{}] requires string 'dst'", i))
            })?;
            // Validate src/dst reference existing nodes
            if !nodes.iter().any(|n| n == src) {
                return Err(AreevError::Validation(format!(
                    "workflow edges[{}].src '{}' does not reference a known node",
                    i, src
                )));
            }
            if !nodes.iter().any(|n| n == dst) {
                return Err(AreevError::Validation(format!(
                    "workflow edges[{}].dst '{}' does not reference a known node",
                    i, dst
                )));
            }
            let cond = obj.get("cond").and_then(|v| v.as_str());
            let max_cycles = obj
                .get("max_cycles")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            wf.edges.push(WorkflowEdge {
                src: src.to_string(),
                dst: dst.to_string(),
                cond: cond.map(|s| s.to_string()),
                max_cycles,
            });
        }
    }

    // bindings — optional, object mapping node names to hash strings
    if let Some(bindings_obj) = fields.get("bindings").and_then(|v| v.as_object()) {
        for (key, val) in bindings_obj {
            if !nodes.iter().any(|n| n == key) {
                return Err(AreevError::Validation(format!(
                    "workflow bindings key '{}' does not reference a known node",
                    key
                )));
            }
            if let Some(hash) = val.as_str() {
                wf.bindings.insert(key.clone(), hash.to_string());
            }
        }
    }

    // retries — optional, object mapping node names to integers
    if let Some(retries_obj) = fields.get("retries").and_then(|v| v.as_object()) {
        for (key, val) in retries_obj {
            if !nodes.iter().any(|n| n == key) {
                return Err(AreevError::Validation(format!(
                    "workflow retries key '{}' does not reference a known node",
                    key
                )));
            }
            if let Some(max) = val.as_u64() {
                wf.retries.insert(key.clone(), max as u32);
            }
        }
    }

    // trigger — optional string
    if let Some(trigger) = get_str("trigger") {
        wf = wf.trigger(&trigger);
    }

    Ok(wf)
}

/// Build a [`Skill`] (OMS 1.4) from a JSON field map. Shared by the `add`
/// (`build_grain_from_json`) and `supersede` (`supersede_from_json`) arms so
/// the per-field mapping exists in exactly one place. Required: `name` +
/// `description`. `proficiency` aliases confidence (D3); an explicitly
/// out-of-range value is rejected. The held-skill `user_id` requirement (BC1)
/// is enforced downstream by the policy gate.
fn build_skill_from_json(
    fields: &serde_json::Map<String, serde_json::Value>,
    get_str: &dyn Fn(&str) -> Option<String>,
    get_f64: &dyn Fn(&str) -> Option<f64>,
    get_i64: &dyn Fn(&str) -> Option<i64>,
) -> Result<Skill> {
    let name = get_str("name")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AreevError::Validation("skill requires non-empty 'name'".into()))?;
    let description = get_str("description")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AreevError::Validation("skill requires non-empty 'description'".into()))?;
    let mut s = Skill::new(&name, &description);
    if let Some(instr) = get_str("instructions") {
        s.instructions = Some(instr);
    }
    if let Some(wtu) = get_str("when_to_use") {
        s.when_to_use = Some(wtu);
    }
    if let Some(v) = get_str("version") {
        s.version = Some(v);
    }
    if let Some(d) = get_str("domain") {
        s.domain = Some(d);
    }
    let str_array = |key: &str| -> Vec<String> {
        fields
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    s.allowed_tools = str_array("allowed_tools");
    s.resources = str_array("resources");
    s.dependencies = str_array("dependencies");
    s.input_modalities = str_array("input_modalities");
    s.output_modalities = str_array("output_modalities");
    if let Some(hdid) = get_str("holder_did") {
        s.holder_did = Some(hdid);
    }
    if let Some(p) = get_f64("proficiency") {
        if !(0.0..=1.0).contains(&p) {
            return Err(AreevError::Validation(format!(
                "proficiency must be between 0.0 and 1.0, got {p}"
            )));
        }
        s = s.with_proficiency(p);
    }
    if let Some(pc) = get_i64("practice_count") {
        if pc >= 0 {
            s.practice_count = Some(pc as u32);
        }
    }
    if let Some(lpa) = get_i64("last_practiced_at") {
        s.last_practiced_at = Some(lpa);
    }
    if let Some(strat_val) = fields.get("strategies") {
        s.strategies = serde_json::from_value(strat_val.clone())
            .map_err(|e| AreevError::Validation(format!("skill 'strategies' invalid: {e}")))?;
    }
    if let Some(x) = fields.get("transferable").and_then(|v| v.as_bool()) {
        s.transferable = Some(x);
    }
    Ok(s)
}
/// Cap on `related_to` links accepted through the JSON surface. The Rust
/// builders have no cap (trusted callers), but this path takes untrusted
/// input and every link costs `triples`/`osp` index rows on add. 64 is far
/// above any legitimate grain observed (a `step_action` execution record
/// plus a handful of similarity links).
const MAX_RELATED_TO_LINKS: usize = 64;

/// Parse and strictly validate a `related_to` array from JSON fields.
///
/// Returns `Ok(None)` when the key is absent, `Ok(Some(links))` when every
/// entry validates, and an error otherwise — malformed input is refused,
/// never silently dropped or partially accepted. Each entry must be an
/// object with exactly `hash` (a 64-hex content address), `relation_type`
/// (non-empty, ≤256 bytes, no control characters), and optionally a finite
/// numeric `weight`. `mg:step_action:` relations must carry a non-empty
/// node id — a link to "node nothing" would index but never resolve.
fn parse_related_to(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<Vec<RelatedTo>>> {
    let Some(value) = fields.get("related_to") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let arr = value.as_array().ok_or_else(|| {
        AreevError::Validation(
            "'related_to' must be an array of {hash, relation_type, weight?} objects".into(),
        )
    })?;
    if arr.len() > MAX_RELATED_TO_LINKS {
        return Err(AreevError::Validation(format!(
            "'related_to' has {} entries; the maximum is {MAX_RELATED_TO_LINKS}",
            arr.len()
        )));
    }
    let mut links = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            AreevError::Validation(format!(
                "'related_to[{i}]' must be an object with 'hash' and 'relation_type'"
            ))
        })?;
        for key in obj.keys() {
            if !matches!(key.as_str(), "hash" | "relation_type" | "weight") {
                return Err(AreevError::Validation(format!(
                    "'related_to[{i}]' has unknown key '{key}'; \
                     accepted keys: hash, relation_type, weight"
                )));
            }
        }
        let hash_str = obj.get("hash").and_then(|v| v.as_str()).ok_or_else(|| {
            AreevError::Validation(format!("'related_to[{i}]' requires a string 'hash'"))
        })?;
        Hash::from_hex(hash_str).map_err(|_| {
            AreevError::Validation(format!(
                "'related_to[{i}].hash' is not a valid content address \
                 (expected 64 hex characters)"
            ))
        })?;
        let relation = obj
            .get("relation_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AreevError::Validation(format!(
                    "'related_to[{i}]' requires a string 'relation_type'"
                ))
            })?;
        if relation.trim().is_empty() {
            return Err(AreevError::Validation(format!(
                "'related_to[{i}].relation_type' must be non-empty"
            )));
        }
        if relation.len() > 256 {
            return Err(AreevError::Validation(format!(
                "'related_to[{i}].relation_type' exceeds 256 bytes"
            )));
        }
        if relation.chars().any(|c| c.is_control()) {
            return Err(AreevError::Validation(format!(
                "'related_to[{i}].relation_type' contains control characters"
            )));
        }
        if relation.starts_with(STEP_ACTION_PREFIX)
            && step_action_node(relation).is_none_or(|n| n.trim().is_empty())
        {
            return Err(AreevError::Validation(format!(
                "'related_to[{i}].relation_type' is an mg:step_action link \
                 with no node id"
            )));
        }
        let weight = match obj.get("weight") {
            None | Some(serde_json::Value::Null) => None,
            Some(w) => {
                let f = w.as_f64().filter(|f| f.is_finite()).ok_or_else(|| {
                    AreevError::Validation(format!(
                        "'related_to[{i}].weight' must be a finite number"
                    ))
                })?;
                Some(f)
            }
        };
        links.push(RelatedTo {
            hash: hash_str.to_string(),
            relation_type: relation.to_string(),
            weight,
        });
    }
    Ok(Some(links))
}

/// Object-safe-ish sink: what to do with the constructed grain.
pub trait GrainSink {
    type Out;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out>;
}

const ADD_JSON_KNOWN_FIELDS: &[&str] = &[
        "grain_type",
        "type",
        "content",
        "subject",
        "relation",
        "object",
        "namespace",
        "user_id",
        "confidence",
        "source_type",
        "created_at",
        // The validity bounds now have builder arms in `apply_common!`, so they
        // must leave the `ctx` sweep — a field that lands in a typed slot AND in
        // `common.context` is stored twice, and the `ctx` copy comes back under
        // a compacted key nothing reads.
        "valid_from",
        "valid_to",
        "system_valid_from",
        "system_valid_to",
        "importance",
        "embedding_text",
        "tags",
        "structural_tags",
        "trigger",
        "steps",
        // Workflow graph fields (OMS §8.4). Without these, `collect_context_extras`
        // swept them into `common.context` as well, writing the whole graph twice —
        // once typed at top level, once verbatim inside `ctx`. Deliberately NOT
        // adding `name`/`status` here: `name` is a Skill field (§6.11) and `status`
        // is claimed by no builder, so listing them would move them out of `ctx`
        // for every grain type and change those types' content addresses.
        "nodes",
        "edges",
        "bindings",
        "retries",
        // State (OMS §8.3).
        "plan",
        "history",
        // Event (OMS §8.2) — consumed by the typed builder, so it must not also
        // be swept into `common.context`.
        "role",
        "session_id",
        "parent_message_id",
        "content_blocks",
        "model_id",
        "stop_reason",
        "token_usage",
        "run_id",
        "tool_name",
        "tool_call_id",
        "input",
        "output",
        "is_error",
        "data",
        "context_data",
        "objective",
        "description",
        "observer_id",
        "observer_type",
        "inference_method",
        "threshold",
        "subject_did",
        "grantee_did",
        "purpose",
        "scope",
        "expiry",
        "source_text",
        "reason",
        "supersession_justification",
        "context",
        "derived_from",
        // Consumed by the `related_to` arm in `apply_common!`. It sat in
        // COMMON_KNOWN_FIELDS with no builder arm, so `collect_context_extras`
        // swept a caller's links into `common.context` under a compacted key
        // nothing reads — the `mg:step_action` run↔plan link was writable only
        // from the Rust builders while `step_actions`/`run_trace` existed on
        // every surface.
        "related_to",
        // Write attribution — same recovered shape as `related_to`.
        "author_did",
    ];

/// The §6.1 common context built from input keys no builder consumes.
///
/// Disjoint from [`collect_extra_fields`] by construction, and that is the
/// point: the two used to overlap, so any key neither list claimed was written
/// **twice** — once verbatim at top level and once inside `ctx`. The `ctx` copy
/// was the harmful one: `common.context` compacts its keys on write and the
/// reader only reverses the restricted `int:*` profile set, so it came back as
/// an unreadable short code — `priority` as `pri`, `name` as `skname`, `status`
/// as `ast`. Nothing could read those, and every grain paid the bytes.
///
/// A key now lands in exactly one place: the typed field or `extra_fields` if a
/// builder claims it, `ctx` only for the common-field names that no builder
/// consumes (validity bounds, ref lists, and the like), which is where they
/// already were.
fn collect_context_extras(
    fields: &serde_json::Map<String, serde_json::Value>,
    grain_type: &str,
) -> Option<serde_json::Value> {
    let type_known = type_known_fields(grain_type);
    let extras: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .filter(|(k, v)| {
            !v.is_null()
                && COMMON_KNOWN_FIELDS.contains(&k.as_str())
                && !ADD_JSON_KNOWN_FIELDS.contains(&k.as_str())
                && !type_known.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if extras.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(extras))
    }
}

pub fn build_grain_from_json<S: GrainSink>(
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
        sink: S,
    ) -> Result<S::Out> {
        let get_str = |key: &str| -> Option<String> {
            fields
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let require_str = |key: &str, grain: &str| -> Result<String> {
            get_str(key)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    AreevError::Validation(format!("{grain} requires non-empty '{key}'"))
                })
        };
        let get_f64 = |key: &str| -> Option<f64> { fields.get(key).and_then(|v| v.as_f64()) };
        // Accept an integral float too: JSON has one number type, so a caller
        // sending `1700000000000.0` (or any JS number, which is always a
        // double) means the same timestamp as `1700000000000`.
        let get_i64 = |key: &str| -> Option<i64> {
            fields.get(key).and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            })
        };

        macro_rules! apply_common {
            ($grain:expr) => {{
                let mut g = $grain;
                if let Some(ns) = get_str("namespace") {
                    g = g.namespace(&ns);
                }
                if let Some(uid) = get_str("user_id") {
                    g = g.user_id(&uid);
                }
                if let Some(c) = get_f64("confidence") {
                    if !(0.0..=1.0).contains(&c) {
                        return Err(AreevError::Validation(format!(
                            "confidence must be between 0.0 and 1.0, got {}",
                            c
                        )));
                    }
                    g = g.confidence(c);
                }
                if let Some(st) = get_str("source_type") {
                    g = g.source_type(&st);
                }
                if let Some(ts) = get_i64("created_at") {
                    g = g.created_at(ts);
                }
                // Bi-temporal validity bounds are first-class common fields
                // (`GrainCommon::valid_from/valid_to/...`), but no builder arm
                // claimed them, so `collect_context_extras` swept them into
                // `common.context` — where the write path compacts the key, and
                // `valid_to` came back as `{"context": {"vt": …}}`. Nothing
                // reads that: `loop.staleness` looks for a top-level
                // `valid_to`, and so does the store's world-time (`vf`/`vt`)
                // column projection. So a bindings user setting expiry the
                // documented way got a fact that silently never expires and
                // never participates in an as-of query.
                if let Some(ts) = get_i64("valid_from") {
                    g.common_mut().valid_from = Some(ts);
                }
                if let Some(ts) = get_i64("valid_to") {
                    g.common_mut().valid_to = Some(ts);
                }
                if let Some(ts) = get_i64("system_valid_from") {
                    g.common_mut().system_valid_from = Some(ts);
                }
                if let Some(ts) = get_i64("system_valid_to") {
                    g.common_mut().system_valid_to = Some(ts);
                }
                if let Some(imp) = get_f64("importance") {
                    g = g.importance(imp);
                }
                if let Some(et) = get_str("embedding_text") {
                    g.common_mut().embedding_text = Some(et);
                }
                // Provenance: route `derived_from` to the first-class common
                // field (mirrors copy_common_fields_from_deserialized) so it
                // serializes top-level and populates provenance_idx — needed
                // by both the add (Add) and supersede (Supersede) applier paths.
                if let Some(df) = get_str("derived_from") {
                    g.common_mut().derived_from = Some(df);
                }
                // `related_to` links (incl. `mg:step_action:<node>` execution
                // records, the run↔plan join). Strictly validated — see
                // `parse_related_to` for why absence of this arm was a bug.
                if let Some(links) = parse_related_to(fields)? {
                    g.common_mut().related_to = links;
                }
                // Write attribution (`PrincipalSession` stamps this; hosts may
                // set it directly). Same recovered-from-ctx-sweep shape as
                // `related_to`: in COMMON_KNOWN_FIELDS with no arm, a caller's
                // author landed compacted in `context` where nothing reads it.
                if let Some(did) = get_str("author_did") {
                    g.common_mut().author_did = Some(did);
                }
                // DX-P1-2: Also accept "structural_tags" key (used by REST add_grain handler).
                let tag_source = fields.get("tags").or_else(|| fields.get("structural_tags"));
                if let Some(tags) = tag_source.and_then(|v| v.as_array()) {
                    let tag_strs: Vec<String> = tags
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    if !tag_strs.is_empty() {
                        g = g.tags(tag_strs);
                    }
                }
                if let Some(extra) = collect_context_extras(fields, grain_type) {
                    let mut ctx = match g.common().context.clone() {
                        Some(serde_json::Value::Object(m)) => m,
                        _ => serde_json::Map::new(),
                    };
                    if let serde_json::Value::Object(m) = extra {
                        ctx.extend(m);
                    }
                    g.common_mut().context = Some(serde_json::Value::Object(ctx));
                }
                g
            }};
        }

        // The run-correlation contract (proposal §2 item 3): a top-level
        // `run_id` on ANY grain type is indexed into `run_idx`, so it is
        // validated on every type — not just Event, where it happens to be a
        // typed field. A non-string `run_id` is refused rather than ignored.
        match fields.get("run_id") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(r)) => validate_run_id(r)?,
            Some(_) => {
                return Err(AreevError::Validation(
                    "'run_id' must be a string".into(),
                ))
            }
        }

        // Reject null bytes in string fields — they can cause issues with
        // C-based indexing (Tantivy, USearch) and downstream consumers.
        for (key, value) in fields.iter() {
            if let Some(s) = value.as_str() {
                if s.contains('\0') {
                    return Err(AreevError::Validation(format!(
                        "field '{}' contains null bytes, which are not permitted in text fields",
                        key
                    )));
                }
            }
        }

        match grain_type {
            "fact" => {
                let subject = require_str("subject", "fact")?;
                let relation = require_str("relation", "fact")?;
                let object = require_str("object", "fact")?;
                let mut grain = apply_common!(Fact::new(&subject, &relation, &object));
                grain.common_mut().extra_fields = collect_extra_fields(fields, "fact");
                sink.consume(&grain)
            }
            "event" => {
                let content = require_str("content", "event")?;
                let mut ev = Event::new(&content);
                if let Some(s) = get_str("subject") { ev = ev.subject(&s); }
                if let Some(o) = get_str("object") { ev = ev.object(&o); }
                if let Some(role) = get_str("role") {
                    let role = Role::from_str(&role).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "event 'role' must be user, assistant, system, or tool, got {role:?}"
                        ))
                    })?;
                    ev = ev.role(role);
                }
                if let Some(s) = get_str("session_id") { ev = ev.session(s); }
                if let Some(p) = get_str("parent_message_id") { ev = ev.parent_message(p); }
                if let Some(cb) = fields.get("content_blocks") {
                    let blocks = serde_json::from_value(cb.clone()).map_err(|e| {
                        AreevError::Validation(format!("event 'content_blocks' invalid: {e}"))
                    })?;
                    ev = ev.content_blocks(blocks);
                }
                if let Some(m) = get_str("model_id") { ev = ev.model(m); }
                if let Some(s) = get_str("stop_reason") { ev = ev.stop_reason(s); }
                if let Some(tu) = fields.get("token_usage") {
                    let usage = serde_json::from_value(tu.clone()).map_err(|e| {
                        AreevError::Validation(format!("event 'token_usage' invalid: {e}"))
                    })?;
                    ev = ev.token_usage(usage);
                }
                // OMS §8.2 `run_id` (wire key `rid`). Without this it fell through
                // to `extra_fields` *and* `common.context`, so it was written twice
                // and never under its spec key.
                if let Some(r) = get_str("run_id") { ev = ev.run_id(r); }
                let mut grain = apply_common!(ev);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "event");
                sink.consume(&grain)
            }
            "state" => {
                // `context` is the OMS §8.3 field name; `data`/`context_data` are
                // accepted aliases. `context_data` was previously listed as a known
                // field but never read, so `add("state", {"context_data": …})`
                // silently stored an empty snapshot.
                let data = fields
                    .get("context")
                    .or_else(|| fields.get("data"))
                    .or_else(|| fields.get("context_data"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let mut st = State::new(data);
                if let Some(plan) = fields.get("plan").and_then(|v| v.as_array()) {
                    st = st.plan(
                        plan.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect(),
                    );
                }
                if let Some(history) = fields.get("history").and_then(|v| v.as_array()) {
                    st = st.history(history.clone());
                }
                let mut grain = apply_common!(st);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "state");
                sink.consume(&grain)
            }
            "workflow" => {
                let wf = build_workflow_from_json(fields, &get_str)?;
                let mut grain = apply_common!(wf);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "workflow");
                sink.consume(&grain)
            }
            "trigger" => {
                let trig = build_trigger_from_json(fields, &get_str)?;
                let mut grain = apply_common!(trig);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "trigger");
                sink.consume(&grain)
            }
            "tool" => {
                let tool_name = require_str("tool_name", "tool")?;
                let mut tool = Tool::new(&tool_name);
                if let Some(inp_val) = fields.get("input") {
                    if let Some(s) = inp_val.as_str() {
                        tool = tool.input_str(s);
                    } else {
                        tool = tool.input(inp_val.clone());
                    }
                }
                if let Some(cnt) = get_str("content") {
                    tool = tool.content(&cnt);
                }
                if let Some(ie) = fields.get("is_error").and_then(|v| v.as_bool()) {
                    tool = tool.is_error(ie);
                }
                if let Some(d) = get_i64("duration_ms") {
                    if d >= 0 {
                        tool = tool.duration_ms(d as u64);
                    }
                }
                // The invocation this record is the result of. Serialized since
                // 1.0 (`tcid`) and listed as queryable in the registry, but no
                // builder arm ever set it, so it was unreachable from `add()`,
                // `record_tool_call`, MCP and the CLI alike. It is also the
                // field that makes two executions of the same tool distinct
                // records rather than one — see `occurrence_id`.
                if let Some(id) = get_str("tool_call_id") {
                    tool = tool.tool_call_id(&id);
                }
                // A string that is present but not a string is refused, never
                // ignored — `get_str` returning None must mean "absent", or a
                // caller sending `{"executor_uri": 3}` gets a Definition with
                // no executor and no error.
                let strict_str = |key: &str| -> Result<Option<String>> {
                    match fields.get(key) {
                        None | Some(serde_json::Value::Null) => Ok(None),
                        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
                        Some(_) => Err(AreevError::Validation(format!(
                            "tool '{key}' must be a string"
                        ))),
                    }
                };
                let strict_bool = |key: &str| -> Result<Option<bool>> {
                    match fields.get(key) {
                        None | Some(serde_json::Value::Null) => Ok(None),
                        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
                        Some(_) => Err(AreevError::Validation(format!(
                            "tool '{key}' must be a boolean"
                        ))),
                    }
                };
                // These three were in `type_known_fields("tool")` with no arm:
                // excluded from `extra_fields` because "known", set nowhere —
                // dropped entirely. The same shape as `related_to`.
                if let Some(e) = strict_str("error")? {
                    tool = tool.error(&e);
                }
                if let Some(p) = strict_str("parent_task_id")? {
                    tool = tool.parent_task_id(&p);
                }
                if let Some(schema) = fields.get("output_schema").filter(|v| !v.is_null()) {
                    SchemaValidator::tool_schema().validate(schema).map_err(|e| {
                        AreevError::Validation(format!("tool 'output_schema' rejected: {e}"))
                    })?;
                    tool = tool.output_schema(schema.clone());
                }
                // ── Definition vocabulary + async lifecycle (see the
                // `type_known_fields` comment). Enum strings parse strictly:
                // an unknown value is an error naming the accepted set, never
                // a silent default — `kind: "definiton"` must not quietly
                // build an Execution grain.
                if let Some(k) = strict_str("kind")? {
                    let kind = ToolKind::parse(&k).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "tool 'kind' '{k}' is not recognized; \
                             accepted values: definition, execution"
                        ))
                    })?;
                    tool = tool.kind(kind);
                }
                if let Some(d) = strict_str("tool_description")? {
                    tool = tool.tool_description(&d);
                }
                if let Some(schema) = fields.get("input_schema").filter(|v| !v.is_null()) {
                    SchemaValidator::tool_schema().validate(schema).map_err(|e| {
                        AreevError::Validation(format!("tool 'input_schema' rejected: {e}"))
                    })?;
                    tool = tool.input_schema(schema.clone());
                }
                if let Some(s) = strict_bool("strict")? {
                    tool = tool.strict(s);
                }
                if let Some(uri) = strict_str("executor_uri")? {
                    tool = tool.executor_uri(&uri);
                }
                if let Some(lp) = fields.get("locked_params").filter(|v| !v.is_null()) {
                    if !lp.is_object() {
                        return Err(AreevError::Validation(
                            "tool 'locked_params' must be a JSON object".into(),
                        ));
                    }
                    tool = tool.locked_params(lp.clone());
                }
                if let Some(ex) = fields.get("examples").filter(|v| !v.is_null()) {
                    let arr = ex.as_array().ok_or_else(|| {
                        AreevError::Validation("tool 'examples' must be an array".into())
                    })?;
                    tool = tool.examples(arr.clone());
                }
                if let Some(ann) = fields.get("annotations").filter(|v| !v.is_null()) {
                    let obj = ann.as_object().ok_or_else(|| {
                        AreevError::Validation(
                            "tool 'annotations' must be an object with boolean \
                             read_only / destructive / idempotent"
                                .into(),
                        )
                    })?;
                    let mut a = ToolAnnotations::default();
                    for (key, value) in obj {
                        let flag = value.as_bool().ok_or_else(|| {
                            AreevError::Validation(format!(
                                "tool 'annotations.{key}' must be a boolean"
                            ))
                        })?;
                        match key.as_str() {
                            "read_only" => a.read_only = flag,
                            "destructive" => a.destructive = flag,
                            "idempotent" => a.idempotent = flag,
                            other => {
                                return Err(AreevError::Validation(format!(
                                    "tool 'annotations' has unknown key '{other}'; \
                                     accepted keys: read_only, destructive, idempotent"
                                )))
                            }
                        }
                    }
                    // Mirrors the `bind_tool` write-time rule (MEM-E103): a
                    // tool cannot be both read-only and destructive.
                    if a.read_only && a.destructive {
                        return Err(AreevError::Validation(
                            "tool 'annotations' cannot set both read_only and \
                             destructive"
                                .into(),
                        ));
                    }
                    tool = tool.annotations(a);
                }
                if let Some(h) = strict_str("spec_hash")? {
                    tool = tool.spec_hash(&h);
                }
                if let Some(ek) = strict_str("executor_kind")? {
                    let kind = ExecutorKind::parse(&ek).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "tool 'executor_kind' '{ek}' is not recognized; \
                             accepted values: host, client"
                        ))
                    })?;
                    tool = tool.executor_kind(kind);
                }
                if let Some(am) = strict_bool("async_mode")? {
                    tool.async_mode = Some(am);
                }
                if let Some(st) = strict_str("status")? {
                    let status = ExecutionStatus::parse(&st).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "tool 'status' '{st}' is not recognized; \
                             accepted values: pending, completed, failed"
                        ))
                    })?;
                    tool.status = Some(status);
                }
                if let Some(cid) = strict_str("correlation_id")? {
                    tool.correlation_id = Some(cid);
                }
                if let Some(bid) = strict_str("call_batch_id")? {
                    tool = tool.call_batch_id(&bid);
                }
                if fields.get("expires_at_sec").is_some_and(|v| !v.is_null()) {
                    let ts = get_i64("expires_at_sec").ok_or_else(|| {
                        AreevError::Validation(
                            "tool 'expires_at_sec' must be an integer (epoch seconds)".into(),
                        )
                    })?;
                    tool.expires_at_sec = Some(ts);
                }
                if let Some(fc) = strict_str("failure_cause")? {
                    let cause = FailureCause::parse(&fc).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "tool 'failure_cause' '{fc}' is not recognized; \
                             accepted values: timeout, executor_error, \
                             schema_validation_failed, user_aborted, unknown"
                        ))
                    })?;
                    tool.failure_cause = Some(cause);
                }
                if let Some(fd) = strict_str("failure_detail")? {
                    tool.failure_detail = Some(fd);
                }
                if let Some(env) = strict_str("actor_execution_environment")? {
                    let e = ActorExecutionEnvironment::parse(&env).ok_or_else(|| {
                        AreevError::Validation(format!(
                            "tool 'actor_execution_environment' '{env}' is not \
                             recognized; accepted values: server_side_host, \
                             client_side_attested, client_side_unattested"
                        ))
                    })?;
                    tool.actor_execution_environment = Some(e);
                }
                let mut grain = apply_common!(tool);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "tool");
                sink.consume(&grain)
            }
            "observation" => {
                // Relaxed validation: observation grains now require `content` as the
                // primary field. `observer_id` and `observer_type` default to "unknown"
                // and "agent" respectively when omitted, since callers like Atmatic
                // typically just need to store an observation with content.
                let content = require_str("content", "observation")?;
                let observer_id = get_str("observer_id").unwrap_or_else(|| "unknown".to_string());
                let observer_type = get_str("observer_type").unwrap_or_else(|| "agent".to_string());
                let mut obs = Observation::new(&observer_id, &observer_type);
                if let Some(s) = get_str("subject") { obs = obs.subject(&s); }
                // Store content in `object` so it persists and is searchable.
                let obj = get_str("object").unwrap_or(content);
                obs = obs.object(&obj);
                let mut grain = apply_common!(obs);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "observation");
                sink.consume(&grain)
            }
            "goal" => {
                let description = get_str("description")
                    .or_else(|| get_str("object"))
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| AreevError::Validation("goal requires non-empty 'description' (or 'object' as fallback)".into()))?;
                let mut g = Goal::new(&description);
                if let Some(s) = get_str("subject") { g = g.subject(&s); }
                if let Some(o) = get_str("object") { g = g.object(&o); }
                let mut grain = apply_common!(g);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "goal");
                sink.consume(&grain)
            }
            "reasoning" => {
                let mut r = Reasoning::new();
                if let Some(c) = get_str("conclusion") { r.conclusion = Some(c); }
                if let Some(m) = get_str("inference_method") { r.inference_method = Some(m); }
                let mut grain = apply_common!(r);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "reasoning");
                sink.consume(&grain)
            }
            "consensus" => {
                let mut c = Consensus::new();
                if let Some(po) = fields.get("participating_observers").and_then(|v| v.as_array()) {
                    c.participating_observers = po.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
                if let Some(t) = get_f64("threshold") { c.threshold = Some(t); }
                if let Some(ac) = get_i64("agreement_count") { c.agreement_count = Some(ac); }
                if let Some(dc) = get_i64("dissent_count") { c.dissent_count = Some(dc); }
                if let Some(dg) = fields.get("dissent_grains").and_then(|v| v.as_array()) {
                    c.dissent_grains = dg.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
                if let Some(ac) = get_str("agreed_content") { c.agreed_content = Some(ac); }
                let mut grain = apply_common!(c);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "consensus");
                sink.consume(&grain)
            }
            "consent" => {
                let subject_did = require_str("subject_did", "consent")?;
                // Consent grains must identify the data subject via user_id.
                // This prevents unauthenticated callers from forging consent records without
                // a verifiable subject identity.
                let user_id_val = get_str("user_id");
                match &user_id_val {
                    None => {
                        return Err(AreevError::Validation(
                            "consent grains require a non-empty 'user_id' field".into(),
                        ));
                    }
                    Some(uid) if uid.trim().is_empty() => {
                        return Err(AreevError::Validation(
                            "consent grains require a non-empty 'user_id' field".into(),
                        ));
                    }
                    _ => {}
                }
                let mut c = Consent::new(&subject_did);
                if let Some(g) = get_str("grantee_did") { c.grantee_did = Some(g); }
                if let Some(s) = get_str("scope") { c.scope = Some(s); }
                if let Some(iw) = fields.get("is_withdrawal").and_then(|v| v.as_bool()) { c.is_withdrawal = Some(iw); }
                if let Some(b) = get_str("basis") { c.basis = Some(b); }
                if let Some(j) = get_str("jurisdiction") { c.jurisdiction = Some(j); }
                let mut grain = apply_common!(c);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "consent");
                sink.consume(&grain)
            }
            "skill" => {
                let s = build_skill_from_json(fields, &get_str, &get_f64, &get_i64)?;
                let mut grain = apply_common!(s);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "skill");
                sink.consume(&grain)
            }
            _ => Err(AreevError::Validation(unbuildable_type_message(grain_type))),
        }
    }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A sink that throws the grain away — these tests only care whether the
    /// builder accepted or rejected the input.
    struct NullSink;
    impl GrainSink for NullSink {
        type Out = ();
        fn consume<G: Grain + Clone + 'static>(self, _grain: &G) -> Result<Self::Out> {
            Ok(())
        }
    }

    /// A payload that satisfies every builder arm, so removing exactly one key
    /// isolates that key's requirement.
    fn full_payload() -> serde_json::Map<String, serde_json::Value> {
        json!({
            "subject": "john", "relation": "prefers", "object": "tea",
            "content": "some content",
            "tool_name": "crm_lookup",
            "description": "what it does",
            "name": "refund",
            "subject_did": "did:x:1", "user_id": "u1",
        })
        .as_object()
        .unwrap()
        .clone()
    }

    /// The shared payload plus whatever only this type can accept.
    ///
    /// `kind` is the reason this exists: Tool reads it as
    /// `definition|execution` and Trigger as `interval|schedule|…`, so one
    /// shared value cannot satisfy both. Grains are single-typed, so the two
    /// never meet in a real payload — only in a fixture that tries to be every
    /// type at once.
    fn full_payload_for(gt: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = full_payload();
        if gt == "trigger" {
            fields.insert("kind".into(), json!("interval"));
            fields.insert(
                "workflow".into(),
                json!("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"),
            );
            // Satisfies the kind-specific requirement `incoherence()` enforces.
            fields.insert("interval_secs".into(), json!(120));
        }
        fields
    }

    const ALL_TYPES: &[&str] = &[
        "fact", "event", "state", "workflow", "tool", "observation", "goal",
        "reasoning", "consensus", "consent", "skill", "trigger",
    ];

    /// `required_fields` is what `DESCRIBE` publishes and what ARCHITECTURE
    /// §2.3 bolds. It is a hand-maintained mirror of the `require_str` calls in
    /// the builder arms, so pin it: every listed field must actually be
    /// enforced, and no unlisted field may be.
    #[test]
    fn required_fields_match_the_validator() {
        for gt in ALL_TYPES {
            // Every declared requirement really is enforced.
            for missing in required_fields(gt) {
                let mut fields = full_payload_for(gt);
                fields.remove(*missing);
                // `goal` accepts `object` in place of `description`, so drop the
                // fallback too or the requirement can't be observed.
                if *gt == "goal" {
                    fields.remove("object");
                }
                let got = build_grain_from_json(gt, &fields, NullSink);
                assert!(
                    got.is_err(),
                    "{gt}: required_fields lists '{missing}', but the builder \
                     accepted a payload without it"
                );
            }
            // And the full payload is accepted, so nothing beyond the declared
            // set is silently required.
            assert!(
                build_grain_from_json(gt, &full_payload_for(gt), NullSink).is_ok(),
                "{gt}: rejected a payload carrying every declared required field"
            );
        }
    }

    /// The four container types accept an empty payload. Documented in
    /// ARCHITECTURE §2.3 as deliberate, so a change here is a decision, not a
    /// drift.
    #[test]
    fn container_types_accept_an_empty_payload() {
        let empty = serde_json::Map::new();
        for gt in ["state", "workflow", "reasoning", "consensus"] {
            assert!(
                build_grain_from_json(gt, &empty, NullSink).is_ok(),
                "{gt} should accept {{}}"
            );
            assert!(required_fields(gt).is_empty());
        }
    }

    /// The builder's accepted set is exactly the registry's `host_addable`
    /// column. Three surfaces used to disagree about how many grain types there
    /// are — `DESCRIBE` said twelve, CAL `ADD` named the four it can shape from
    /// flat `SET` pairs, and `add()` claimed eleven and called the twelfth
    /// unknown. Pinning the builder to the registry means a new type is
    /// reachable, or explicitly not, by one edit rather than by remembering
    /// this list.
    #[test]
    fn builder_accepts_exactly_the_host_addable_types() {
        for name in areev_core::types::registry::host_addable_names() {
            assert!(
                ALL_TYPES.contains(&name),
                "{name} is host_addable in the registry but the builder has no arm"
            );
        }
        for gt in ALL_TYPES {
            assert!(
                areev_core::types::registry::host_addable_names().any(|n| n == *gt),
                "{gt} has a builder arm but is not host_addable in the registry"
            );
        }
    }

    /// A type that exists but may not be host-authored must say so. Answering
    /// "unknown grain type" sends the caller looking for a typo in a name that
    /// `DESCRIBE` had just listed back to them.
    #[test]
    fn engine_authored_types_are_refused_by_reason_not_by_denial() {
        let mut fields = serde_json::Map::new();
        fields.insert("target_ref".into(), serde_json::json!("entity:x"));
        fields.insert("analyzer".into(), serde_json::json!("loop.x/1"));
        fields.insert("summary".into(), serde_json::json!("s"));
        fields.insert("dedup_key".into(), serde_json::json!("k"));

        let e = build_grain_from_json("recommendation", &fields, NullSink)
            .expect_err("recommendation is engine-authored");
        let msg = e.to_string();
        assert!(
            msg.contains("engine-authored"),
            "must explain why, got: {msg}"
        );
        assert!(
            !msg.contains("unknown grain type"),
            "recommendation is a real OMS type — saying otherwise is the bug (#67): {msg}"
        );
        assert!(
            msg.contains("RECALL recommendations"),
            "must point at the surface that does work, got: {msg}"
        );

        // A name that really is not a type still reads as one.
        let e = build_grain_from_json("sandwich", &fields, NullSink).expect_err("not a type");
        assert!(
            e.to_string().contains("unknown grain type"),
            "got: {e}"
        );
    }

    /// `tool_call_id` is serialized (`tcid`) and listed as queryable in the
    /// registry, but no builder arm set it, so it was write-only in the worst
    /// direction: unreachable from every host surface. It is also the field
    /// that keeps two executions of one tool from collapsing into one grain.
    #[test]
    fn tool_call_id_reaches_the_grain() {
        let mut fields = serde_json::Map::new();
        fields.insert("tool_name".into(), serde_json::json!("stripe_refund"));
        fields.insert("tool_call_id".into(), serde_json::json!("call_abc"));

        struct Capture;
        impl GrainSink for Capture {
            type Out = Option<String>;
            fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out> {
                let any = grain as &dyn std::any::Any;
                Ok(any
                    .downcast_ref::<areev_core::types::Tool>()
                    .and_then(|t| t.tool_call_id.clone()))
            }
        }

        assert_eq!(
            build_grain_from_json("tool", &fields, Capture).unwrap(),
            Some("call_abc".to_string()),
        );
    }

    #[test]
    fn event_json_reaches_every_typed_slot() {
        let fields = json!({
            "content": "calling search",
            "role": "assistant",
            "session_id": "sess-1",
            "parent_message_id": "msg-0",
            "content_blocks": [{
                "type": "tool_use",
                "id": "call-1",
                "name": "search",
                "input": {"q": "Areev"}
            }],
            "model_id": "model/build",
            "stop_reason": "tool_use",
            "token_usage": {
                "input_tokens": 12,
                "output_tokens": 4,
                "cache_read_tokens": 3
            },
            "run_id": "run-1"
        })
        .as_object()
        .unwrap()
        .clone();

        struct Capture;
        impl GrainSink for Capture {
            type Out = Event;
            fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out> {
                let any = grain as &dyn std::any::Any;
                any.downcast_ref::<Event>()
                    .cloned()
                    .ok_or_else(|| AreevError::Internal("expected event".into()))
            }
        }

        let event = build_grain_from_json("event", &fields, Capture).unwrap();
        assert_eq!(event.role, Some(Role::Assistant));
        assert_eq!(event.session_id.as_deref(), Some("sess-1"));
        assert_eq!(event.parent_message_id.as_deref(), Some("msg-0"));
        assert_eq!(event.model_id.as_deref(), Some("model/build"));
        assert_eq!(event.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(event.run_id.as_deref(), Some("run-1"));
        assert_eq!(event.content_blocks.as_ref().unwrap().len(), 1);
        assert_eq!(event.token_usage.unwrap().input_tokens, 12);
        assert!(event.common.extra_fields.is_empty());
    }

    #[test]
    fn malformed_typed_event_payloads_are_rejected() {
        for (field, value) in [
            ("content_blocks", json!({"type": "text", "text": "not an array"})),
            ("token_usage", json!({"input_tokens": "many", "output_tokens": 1})),
        ] {
            let mut fields = json!({"content": "x"}).as_object().unwrap().clone();
            fields.insert(field.into(), value);
            let err = build_grain_from_json("event", &fields, NullSink).unwrap_err();
            assert!(err.to_string().contains(field), "{err}");
        }
    }

    #[test]
    fn common_and_type_specific_field_sets_are_disjoint() {
        for gt in ALL_TYPES {
            for field in type_known_fields(gt) {
                assert!(
                    !COMMON_KNOWN_FIELDS.contains(field),
                    "{gt}.{field} is both common and type-specific; routing would be ambiguous"
                );
            }
        }
        let unique: std::collections::HashSet<_> = ADD_JSON_KNOWN_FIELDS.iter().collect();
        assert_eq!(unique.len(), ADD_JSON_KNOWN_FIELDS.len(), "duplicate ADD JSON key");
        for field in [
            "role",
            "session_id",
            "parent_message_id",
            "content_blocks",
            "model_id",
            "stop_reason",
            "token_usage",
        ] {
            assert!(type_known_fields("event").contains(&field));
            assert!(ADD_JSON_KNOWN_FIELDS.contains(&field));
            assert!(!COMMON_KNOWN_FIELDS.contains(&field));
        }
    }

    // ── related_to: the run↔plan join write path (Wave 0 item 1) ────────────

    /// A valid 64-hex content address for link targets.
    fn link_hash() -> String {
        "a".repeat(64)
    }

    struct CaptureCommon;
    impl GrainSink for CaptureCommon {
        type Out = GrainCommon;
        fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out> {
            Ok(grain.common().clone())
        }
    }

    /// `related_to` sat in COMMON_KNOWN_FIELDS with no builder arm, so the
    /// links a caller passed were swept into `common.context` under a
    /// compacted key nothing reads — `mg:step_action` was writable only from
    /// Rust while the read APIs existed on every surface. This pins the fix:
    /// links land in the typed field, and nowhere else.
    #[test]
    fn related_to_reaches_the_typed_field_and_only_there() {
        let fields = json!({
            "subject": "run:r1", "relation": "notes", "object": "ok",
            "related_to": [
                {"hash": link_hash(), "relation_type": "mg:step_action:fetch"},
                {"hash": link_hash(), "relation_type": "similar_to", "weight": 0.5},
            ],
        })
        .as_object()
        .unwrap()
        .clone();

        let common = build_grain_from_json("fact", &fields, CaptureCommon).unwrap();
        assert_eq!(common.related_to.len(), 2);
        assert_eq!(common.related_to[0].hash, link_hash());
        assert_eq!(common.related_to[0].relation_type, "mg:step_action:fetch");
        assert_eq!(common.related_to[0].weight, None);
        assert_eq!(common.related_to[1].relation_type, "similar_to");
        assert_eq!(common.related_to[1].weight, Some(0.5));
        // Not double-written: neither swept into ctx nor copied to extras.
        assert!(common.context.is_none(), "related_to leaked into common.context");
        assert!(common.extra_fields.is_empty());
    }

    #[test]
    fn related_to_null_reads_as_absent() {
        let fields = json!({
            "subject": "s", "relation": "r", "object": "o",
            "related_to": null,
        })
        .as_object()
        .unwrap()
        .clone();
        let common = build_grain_from_json("fact", &fields, CaptureCommon).unwrap();
        assert!(common.related_to.is_empty());
    }

    /// Malformed links are refused with an error naming the entry — never
    /// silently dropped, never partially accepted (the fuzz rule: "never
    /// panics, never silently accepts").
    #[test]
    fn malformed_related_to_is_refused_not_dropped() {
        let too_many: Vec<serde_json::Value> = (0..65)
            .map(|_| json!({"hash": link_hash(), "relation_type": "x"}))
            .collect();
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("must be an array", json!("nope")),
            ("must be an object", json!(["just a string"])),
            (
                "unknown key",
                json!([{"hash": link_hash(), "relation_type": "x", "note": "hi"}]),
            ),
            (
                "not a valid content address",
                json!([{"hash": "xyz", "relation_type": "x"}]),
            ),
            ("requires a string 'hash'", json!([{"relation_type": "x"}])),
            (
                "requires a string 'relation_type'",
                json!([{"hash": link_hash()}]),
            ),
            (
                "must be non-empty",
                json!([{"hash": link_hash(), "relation_type": "   "}]),
            ),
            (
                "exceeds 256 bytes",
                json!([{"hash": link_hash(), "relation_type": "r".repeat(257)}]),
            ),
            (
                "control characters",
                json!([{"hash": link_hash(), "relation_type": "a\u{0007}b"}]),
            ),
            (
                "no node id",
                json!([{"hash": link_hash(), "relation_type": "mg:step_action:"}]),
            ),
            (
                "no node id",
                json!([{"hash": link_hash(), "relation_type": "mg:step_action:  "}]),
            ),
            (
                "finite number",
                json!([{"hash": link_hash(), "relation_type": "x", "weight": "heavy"}]),
            ),
            ("maximum is", serde_json::Value::Array(too_many)),
        ];
        for (needle, related) in cases {
            let mut fields = json!({"subject": "s", "relation": "r", "object": "o"})
                .as_object()
                .unwrap()
                .clone();
            fields.insert("related_to".into(), related);
            let err = build_grain_from_json("fact", &fields, NullSink).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("related_to") && msg.contains(needle),
                "expected error mentioning 'related_to' and '{needle}', got: {msg}"
            );
        }
    }

    // ── Tool Definition + async lifecycle vocabulary (Wave 0 item 4) ────────

    struct CaptureTool;
    impl GrainSink for CaptureTool {
        type Out = Tool;
        fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out> {
            let any = grain as &dyn std::any::Any;
            any.downcast_ref::<Tool>()
                .cloned()
                .ok_or_else(|| AreevError::Internal("expected tool".into()))
        }
    }

    /// Every Definition field is a typed `Tool` slot with a compact key since
    /// Phase 1, but none was reachable from `add()` — they fell to verbatim
    /// extras the typed readers never saw. This pins the write path end to
    /// end for the fields §7 of the governed-agents proposal depends on
    /// (CAL supersession of a Definition must be able to set `executor_uri`).
    #[test]
    fn tool_definition_json_reaches_every_typed_slot() {
        let fields = json!({
            "tool_name": "crm.lookup",
            "kind": "definition",
            "tool_description": "Look up a customer record",
            "input_schema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            },
            "output_schema": {"type": "object"},
            "strict": true,
            "executor_uri": "executor://crm.lookup@v3",
            "locked_params": {"region": "eu"},
            "examples": [{"id": "cust_1"}],
            "annotations": {"read_only": true, "idempotent": true},
            "spec_hash": "abc123",
            "executor_kind": "client",
            "async_mode": true,
        })
        .as_object()
        .unwrap()
        .clone();

        let tool = build_grain_from_json("tool", &fields, CaptureTool).unwrap();
        assert_eq!(tool.kind, ToolKind::Definition);
        assert_eq!(tool.tool_description.as_deref(), Some("Look up a customer record"));
        assert!(tool.input_schema.is_some());
        assert!(tool.output_schema.is_some());
        assert_eq!(tool.strict, Some(true));
        assert_eq!(tool.executor_uri.as_deref(), Some("executor://crm.lookup@v3"));
        assert_eq!(tool.locked_params, Some(json!({"region": "eu"})));
        assert_eq!(tool.examples.as_ref().map(|e| e.len()), Some(1));
        let ann = tool.annotations.expect("annotations set");
        assert!(ann.read_only && ann.idempotent && !ann.destructive);
        assert_eq!(tool.spec_hash.as_deref(), Some("abc123"));
        assert_eq!(tool.executor_kind, Some(ExecutorKind::Client));
        assert_eq!(tool.async_mode, Some(true));
        assert!(tool.common.extra_fields.is_empty(), "definition fields leaked to extras");
    }

    /// The async execution lifecycle — and the three fields that were listed
    /// as known but set by no arm (`error`, `parent_task_id`,
    /// `output_schema`): excluded from extras because "known", stored
    /// nowhere. The same silent-drop shape as `related_to`.
    #[test]
    fn tool_execution_lifecycle_reaches_typed_slots() {
        let fields = json!({
            "tool_name": "crm.lookup",
            "status": "pending",
            "correlation_id": "corr-1",
            "call_batch_id": "batch-1",
            "expires_at_sec": 1755200000,
            "failure_cause": "timeout",
            "failure_detail": "upstream 504",
            "actor_execution_environment": "server_side_host",
            "error": "gateway timeout",
            "parent_task_id": "task-9",
        })
        .as_object()
        .unwrap()
        .clone();

        let tool = build_grain_from_json("tool", &fields, CaptureTool).unwrap();
        assert_eq!(tool.status, Some(ExecutionStatus::Pending));
        assert_eq!(tool.correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(tool.call_batch_id.as_deref(), Some("batch-1"));
        assert_eq!(tool.expires_at_sec, Some(1755200000));
        assert_eq!(tool.failure_cause, Some(FailureCause::Timeout));
        assert_eq!(tool.failure_detail.as_deref(), Some("upstream 504"));
        assert_eq!(
            tool.actor_execution_environment,
            Some(ActorExecutionEnvironment::ServerSideHost)
        );
        assert_eq!(tool.error.as_deref(), Some("gateway timeout"));
        assert_eq!(tool.parent_task_id.as_deref(), Some("task-9"));
        assert!(tool.common.extra_fields.is_empty());
    }

    /// Unknown enum strings are errors naming the accepted set — never a
    /// silent default. `kind: "definiton"` must not quietly build an
    /// Execution grain.
    #[test]
    fn unknown_tool_enum_values_error_naming_the_accepted_set() {
        for (field, bad, expect) in [
            ("kind", "definiton", "definition, execution"),
            ("status", "done", "pending, completed, failed"),
            ("executor_kind", "server", "host, client"),
            ("failure_cause", "oops", "timeout"),
            ("actor_execution_environment", "local", "server_side_host"),
        ] {
            let mut fields = json!({"tool_name": "t"}).as_object().unwrap().clone();
            fields.insert(field.into(), json!(bad));
            let err = build_grain_from_json("tool", &fields, NullSink).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(field) && msg.contains(expect),
                "'{field}': expected error naming '{expect}', got: {msg}"
            );
        }
    }

    /// Wrongly-typed values on the new strict fields are refused, never
    /// ignored — `{"executor_uri": 3}` must not build a Definition with no
    /// executor and no error.
    #[test]
    fn tool_wrong_typed_values_are_refused() {
        for (field, value, expect) in [
            ("executor_uri", json!(3), "must be a string"),
            ("strict", json!("yes"), "must be a boolean"),
            ("async_mode", json!(1), "must be a boolean"),
            ("locked_params", json!([1, 2]), "must be a JSON object"),
            ("examples", json!({"a": 1}), "must be an array"),
            ("expires_at_sec", json!("soon"), "must be an integer"),
            ("annotations", json!("ro"), "must be an object"),
            ("annotations", json!({"read_only": "yes"}), "must be a boolean"),
            ("annotations", json!({"reversible": true}), "unknown key"),
            ("input_schema", json!(42), "input_schema"),
        ] {
            let mut fields = json!({"tool_name": "t"}).as_object().unwrap().clone();
            fields.insert(field.into(), value);
            let err = build_grain_from_json("tool", &fields, NullSink).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(expect), "'{field}': expected '{expect}', got: {msg}");
        }
    }

    /// Mirrors bind_tool's MEM-E103 rule at this surface: read_only and
    /// destructive are mutually exclusive.
    #[test]
    fn tool_annotations_read_only_destructive_conflict_is_refused() {
        let fields = json!({
            "tool_name": "t",
            "annotations": {"read_only": true, "destructive": true},
        })
        .as_object()
        .unwrap()
        .clone();
        let err = build_grain_from_json("tool", &fields, NullSink).unwrap_err();
        assert!(err.to_string().contains("read_only"), "{err}");
    }
}
