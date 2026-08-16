use std::collections::HashMap;
use std::sync::LazyLock;

/// Core FIELD_MAP: maps long field names to short (compact) names.
/// OMS 1.2 canonical names — writers MUST emit these.
static FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    // Core fields
    m.insert("type", "t");
    m.insert("subject", "s");
    m.insert("relation", "r");
    m.insert("object", "o");
    m.insert("confidence", "c");
    m.insert("source_type", "st");
    m.insert("created_at", "ca");
    m.insert("temporal_type", "tt");
    m.insert("valid_from", "vf");
    m.insert("valid_to", "vt");
    m.insert("system_valid_from", "svf");
    m.insert("system_valid_to", "svt");
    m.insert("context", "ctx");
    // Where the §6.1 common context goes when the type-specific `context` field
    // already owns `ctx` — State (§8.3) is the only such type. Written only in
    // that collision, so no other grain type's blob gains a key.
    m.insert("common_context", "cctx");
    m.insert("superseded_by", "sb");
    m.insert("importance", "im");
    m.insert("author_did", "adid");
    m.insert("namespace", "ns");
    m.insert("user_id", "user");
    m.insert("structural_tags", "tags");
    m.insert("derived_from", "df");
    m.insert("consolidation_level", "cl");
    m.insert("success_count", "sc");
    m.insert("failure_count", "fc");
    m.insert("provenance_chain", "pc");
    m.insert("origin_did", "odid");
    m.insert("origin_namespace", "ons");
    m.insert("content_refs", "cr");
    m.insert("embedding_refs", "er");
    m.insert("related_to", "rt");
    m.insert("_elided", "_e");
    m.insert("_disclosure_of", "_do");
    m.insert("invalidation_policy", "ip");
    m.insert("supersession_justification", "sj");
    m.insert("supersession_auth", "sa");

    // verification_status
    m.insert("verification_status", "vstatus");

    // OMS 1.2 new common fields (§6.1)
    m.insert("owner", "own");
    m.insert("category", "cat");
    m.insert("run_id", "rid");
    m.insert("role", "role");
    m.insert("access_count", "ac");
    m.insert("last_accessed_at", "laa");
    m.insert("timestamp_ms", "tms");
    m.insert("observer_did", "obsdid");
    m.insert("subject_did", "sdid");
    m.insert("session_id", "sid2");
    m.insert("entity_id", "eid");
    m.insert("epistemic_status", "epstat");
    m.insert("requires_human_review", "rhr");
    m.insert("processing_basis", "pbasis");
    m.insert("identity_state", "idst");
    m.insert("license", "lic");
    m.insert("trusted_timestamp", "tts");
    m.insert("invalidation_type", "itype");
    m.insert("invalidation_reason", "ireason");
    m.insert("invalidation_initiator", "iinit");
    m.insert("retention_policy", "rpol");
    m.insert("recall_priority", "rpri");

    // Tool fields
    m.insert("tool_name", "tn");
    m.insert("input", "inp"); // was: arguments → args
    m.insert("tool_content", "cnt"); // Tool's content field (distinct from Event's uncompacted "content")
    m.insert("is_error", "iserr"); // was: success → ok (inverted polarity)
    m.insert("error", "err");
    m.insert("duration_ms", "dur");
    m.insert("parent_task_id", "ptid");
    // OMS 1.2 new Tool fields
    m.insert("tool_phase", "aphase");
    m.insert("tool_call_id", "tcid");
    m.insert("call_batch_id", "cbid");
    m.insert("tool_type", "ttype");
    m.insert("tool_version", "tver");
    m.insert("execution_mode", "emode");
    m.insert("code", "code");
    m.insert("stdout", "stdout");
    m.insert("stderr", "stderr");
    m.insert("exit_code", "xcode");
    m.insert("interpreter_id", "iid");
    m.insert("error_type", "etype");
    m.insert("tool_description", "tdesc");
    m.insert("input_schema", "ischema");
    m.insert("strict", "strict");
    m.insert("async_mode", "amode");
    // OMS 1.3 new Tool field
    m.insert("output_schema", "osch");

    // Phase 1 (2026-04-19) — Tool definition fields promoted from extra_fields.
    m.insert("kind", "aknd");
    m.insert("executor_uri", "axu");
    m.insert("locked_params", "lprm");
    m.insert("examples", "exmp");
    m.insert("annotations", "anno");
    m.insert("spec_hash", "shsh");
    // HPL Phase 4.1 (2026-04-22): per-binding executor classifier.
    // Default Host is omitted from the serialized blob (see
    // `serialize.rs`) so pre-HPL grains stay byte-identical.
    m.insert("executor_kind", "exk");

    // Async execution + EU AI Act / failure typing.
    // All Option-typed on Tool; absent on legacy grains. Defaults at
    // deserialize: status=None reads as Completed; the rest stay None.
    m.insert("status", "ast");
    m.insert("correlation_id", "acid");
    m.insert("expires_at_sec", "axp");
    m.insert("transient_definition_hash", "tdh");
    m.insert("failure_cause", "afc");
    m.insert("failure_detail", "afd");
    m.insert("actor_execution_environment", "aex");

    // OMS 1.3 Integration Domain Profile (int:* context map keys)
    // Tool fields (REST API connectors)
    m.insert("int:base_url", "ib");
    m.insert("int:http_method", "ihm"); // "im" collides with "importance"
    m.insert("int:http_path", "ihp"); // "ip" collides with "invalidation_policy"
    m.insert("int:path_params", "ipp");
    m.insert("int:query_params", "iqp");
    m.insert("int:body_params", "ibp");
    m.insert("int:response_mapping", "irm");
    m.insert("int:auth_type", "iat");
    m.insert("int:auth_scopes", "ias");
    m.insert("int:read_only", "iro");
    m.insert("int:connector", "ic");
    m.insert("int:docs_url", "idu");
    m.insert("int:rate_limit", "irl");
    m.insert("int:category", "icat");
    m.insert("int:sunset_date", "isd");
    m.insert("int:content_type", "ict");
    // Trigger-specific fields
    m.insert("int:poll_interval_secs", "ipis");
    m.insert("int:cursor_field", "icf");
    m.insert("int:cursor_type", "icft");
    m.insert("int:webhook_path", "iwp");
    m.insert("int:webhook_secret_header", "iwsh");
    m.insert("int:cron_expression", "icron");
    m.insert("int:timezone", "itz");
    m.insert("int:config_schema", "icfg");
    m.insert("int:event_schema", "ievt");

    // Observation fields
    m.insert("observer_id", "oid");
    m.insert("observer_type", "otype");
    m.insert("frame_id", "fid");
    m.insert("sync_group", "sg");
    m.insert("observation_mode", "omode");
    m.insert("observation_scope", "oscope");
    m.insert("observer_model", "omdl");
    m.insert("compression_ratio", "ocmp");

    // Goal fields
    m.insert("description", "desc");
    m.insert("goal_state", "gs");
    m.insert("criteria", "crit");
    m.insert("criteria_structured", "crs");
    m.insert("priority", "pri");
    m.insert("parent_goals", "pgs");
    m.insert("state_reason", "sr");
    m.insert("satisfaction_evidence", "se");
    m.insert("progress", "prog");
    m.insert("delegate_to", "dto");
    m.insert("delegate_from", "dfo");
    m.insert("expiry_policy", "ep");
    m.insert("recurrence", "rec");
    m.insert("evidence_required", "evreq");
    m.insert("rollback_on_failure", "rof");
    m.insert("allowed_transitions", "atr");
    // OMS 1.5 §6.13 — Recommendation-specific fields.
    m.insert("target_ref", "tref");
    m.insert("analyzer", "anlz");
    m.insert("summary", "summ");
    m.insert("dedup_key", "ddk");
    m.insert("proposal_cal", "pcal");
    m.insert("proposal_edit", "pedit");
    m.insert("proposal_data", "pdata");
    m.insert("severity", "sev");
    m.insert("metric_snapshot", "msnap");
    m.insert("evidence_query", "evq");
    // §6.1 — index-layer, never written into a blob (§8.12 rule 3). The key
    // exists so the name round-trips wherever the index layer surfaces it.
    m.insert("rec_status", "rstat");

    // OMS 1.2 new Goal fields
    m.insert("depends_on", "depg");
    m.insert("assigned_agent", "asgn");
    m.insert("expected_output", "expout");
    m.insert("output_grain", "outg");
    m.insert("deadline", "dline");

    // Event fields — "content" stays uncompacted
    m.insert("content_blocks", "cblocks");
    m.insert("model_id", "mdl");
    m.insert("stop_reason", "stopr");
    m.insert("token_usage", "toku");
    m.insert("parent_message_id", "pmid");

    // Reasoning fields (OMS 1.2 new grain type)
    m.insert("premises", "prem");
    m.insert("conclusion", "conc");
    m.insert("inference_method", "imethod");
    m.insert("alternatives_considered", "altc");
    m.insert("thinking_content", "think");
    m.insert("thinking_redacted", "tredact");
    m.insert("statistical_context", "statctx");
    m.insert("software_environment", "swenv");
    m.insert("parameter_set", "params");
    m.insert("random_seed", "rseed");

    // Consensus fields (OMS 1.2 new grain type)
    m.insert("participating_observers", "pobs");
    m.insert("threshold", "thr");
    m.insert("agreement_count", "agrc");
    m.insert("dissent_count", "disc");
    m.insert("dissent_grains", "dgrains");
    m.insert("agreed_content", "acnt");

    // Consent fields (OMS 1.2 new grain type)
    // subject_did already mapped above as "sdid"
    m.insert("grantee_did", "gdid");
    m.insert("scope", "scope");
    m.insert("is_withdrawal", "isw");
    m.insert("basis", "basis");
    m.insert("jurisdiction", "jur");
    m.insert("prior_consent", "pcon");
    m.insert("witness_dids", "wdids");

    // Skill fields (OMS 1.4 new grain type, 0x0B). `description` reuses the
    // shared `desc` key (mapped above with Goal); `proficiency` aliases
    // `confidence` (D3) but still carries a dedicated `prof` key on the wire
    // for held instances. 16 keys, collision-audited clean (design §11).
    m.insert("name", "skname");
    m.insert("instructions", "instr");
    m.insert("when_to_use", "wtu");
    m.insert("version", "sver");
    m.insert("allowed_tools", "atls");
    m.insert("resources", "res");
    m.insert("dependencies", "deps");
    m.insert("input_modalities", "imod");
    m.insert("output_modalities", "omod");
    m.insert("domain", "dom");
    m.insert("holder_did", "hdid");
    m.insert("proficiency", "prof");
    m.insert("practice_count", "prcnt");
    m.insert("last_practiced_at", "lpa");
    m.insert("strategies", "strat");
    m.insert("transferable", "xfer");

    // Embedding text override
    m.insert("embedding_text", "et");

    // Scoped memory fields
    m.insert("scope_path", "scp");
    m.insert("scope_depth", "scd");

    // Delegation fields (§6.10)
    m.insert("authorized_namespaces", "ans");
    m.insert("authorized_types", "atypes");
    m.insert("authorized_tools", "atools");
    m.insert("delegation_depth", "ddepth");
    m.insert("delegation_expiry", "dexp");
    m.insert("context_grains", "cgrains");
    m.insert("return_to", "retdid");

    // Event/State/Workflow fields — not compacted per spec
    // "content" stays as "content" (Event, uncompacted)
    // "consolidated" stays as "consolidated"
    // "plan" stays as "plan"
    // "history" stays as "history"
    // "trigger" stays as "trigger"
    // "nodes" stays as "nodes"
    // "edges" stays as "edges"
    // "retries" stays as "retries"
    m.insert("bindings", "bind");

    m
});

/// Reverse map: short name → long name.
static REVERSE_FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| FIELD_MAP.iter().map(|(&k, &v)| (v, k)).collect());

/// Content reference nested field map.
static CONTENT_REF_FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("uri", "u");
    m.insert("modality", "m");
    m.insert("mime_type", "mt");
    m.insert("size_bytes", "sz");
    m.insert("checksum", "ck");
    m.insert("metadata", "md");
    m
});

/// Embedding reference nested field map.
static EMBEDDING_REF_FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("vector_id", "vi");
        m.insert("model", "mo");
        m.insert("dimensions", "dm");
        m.insert("modality_source", "ms");
        m.insert("distance_metric", "di");
        // OMS 1.2 new embedding ref fields
        m.insert("chunk_index", "ci");
        m.insert("chunk_text", "ct");
        m.insert("chunk_strategy", "cs");
        m.insert("chunk_overlap", "co");
        m
    });

/// Related-to nested field map.
static RELATED_TO_FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("hash", "h");
    m.insert("relation_type", "rl");
    m.insert("weight", "w");
    m
});

/// Workflow edge nested field map.
static WORKFLOW_EDGE_FIELD_MAP: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        // src, dst, cond stay as-is (already short)
        m.insert("max_cycles", "mxc");
        m
    });

/// Compact a field name using the FIELD_MAP.
pub fn compact_field(name: &str) -> &str {
    FIELD_MAP.get(name).copied().unwrap_or(name)
}

/// Expand a short field name back to its full name.
pub fn expand_field(short: &str) -> &str {
    REVERSE_FIELD_MAP.get(short).copied().unwrap_or(short)
}

/// Expand a compacted `context`-map key. Only the OMS `int:*` Integration
/// Domain Profile keys are deliberately compacted inside a context map; every
/// other context key is user-controlled and MUST survive verbatim. Reversing
/// such a key through the general [`expand_field`] corrupts any user key that
/// happens to equal an OMS short code (e.g. `"o"` → `"object"`), so the general
/// expansion must never be applied to nested user JSON — only this restricted
/// `int:*` reversal is safe there.
pub fn expand_context_field(short: &str) -> &str {
    match REVERSE_FIELD_MAP.get(short) {
        Some(&long) if long.starts_with("int:") => long,
        _ => short,
    }
}

/// Compact content_ref nested fields.
pub fn compact_content_ref_field(name: &str) -> &str {
    CONTENT_REF_FIELD_MAP.get(name).copied().unwrap_or(name)
}

/// Compact embedding_ref nested fields.
pub fn compact_embedding_ref_field(name: &str) -> &str {
    EMBEDDING_REF_FIELD_MAP.get(name).copied().unwrap_or(name)
}

/// Compact related_to nested fields.
pub fn compact_related_to_field(name: &str) -> &str {
    RELATED_TO_FIELD_MAP.get(name).copied().unwrap_or(name)
}

/// Compact workflow edge nested fields.
pub fn compact_workflow_edge_field(name: &str) -> &str {
    WORKFLOW_EDGE_FIELD_MAP.get(name).copied().unwrap_or(name)
}

/// Expand a compacted `related_to` entry key (`h`/`rl`/`w`).
///
/// Like the workflow-edge table this is a closed, OMS-defined map, so the
/// reversal is safe — but it must stay scoped: `h`, `rl` and `w` are common
/// enough short codes that running the general [`expand_field`] over arbitrary
/// nested data would corrupt it. Without this, a cross-grain link came back as
/// `{"h":…, "rl":…}` and every reader looking for `relation_type` found nothing.
pub fn expand_related_to_field(short: &str) -> &str {
    match short {
        "h" => "hash",
        "rl" => "relation_type",
        "w" => "weight",
        other => other,
    }
}

/// Expand a compacted workflow-edge key (`mxc` → `max_cycles`).
///
/// Restricted to the edge table on purpose: an edge map is OMS-defined, not
/// user-controlled, but running the general [`expand_field`] over it would still
/// rewrite `src`/`dst`/`cond` if they ever collided with a short code. Without
/// this, `max_cycles` came back off the wire as the raw `mxc`, so every reader
/// (renderers, `to_workflow`, the JSON builder) looked for a key that was never
/// there and cycle bounds silently vanished on read.
pub fn expand_workflow_edge_field(short: &str) -> &str {
    match short {
        "mxc" => "max_cycles",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_field() {
        assert_eq!(compact_field("subject"), "s");
        assert_eq!(compact_field("relation"), "r");
        assert_eq!(compact_field("object"), "o");
        assert_eq!(compact_field("confidence"), "c");
        assert_eq!(compact_field("type"), "t");
        assert_eq!(compact_field("created_at"), "ca");
        assert_eq!(compact_field("namespace"), "ns");
        assert_eq!(compact_field("author_did"), "adid");
        assert_eq!(compact_field("source_type"), "st");
        assert_eq!(compact_field("unknown_field"), "unknown_field");
        // OMS 1.2 fields
        assert_eq!(compact_field("input"), "inp");
        assert_eq!(compact_field("is_error"), "iserr");
        assert_eq!(compact_field("verification_status"), "vstatus");
        assert_eq!(compact_field("premises"), "prem");
        assert_eq!(compact_field("conclusion"), "conc");
        // OMS 1.3 fields
        assert_eq!(compact_field("output_schema"), "osch");
        assert_eq!(compact_field("int:base_url"), "ib");
        assert_eq!(compact_field("int:http_method"), "ihm");
        assert_eq!(compact_field("int:connector"), "ic");
        assert_eq!(compact_field("int:poll_interval_secs"), "ipis");
        assert_eq!(compact_field("int:webhook_path"), "iwp");
        // Async-execution fields
        assert_eq!(compact_field("status"), "ast");
        assert_eq!(compact_field("correlation_id"), "acid");
        assert_eq!(compact_field("expires_at_sec"), "axp");
        assert_eq!(compact_field("transient_definition_hash"), "tdh");
        assert_eq!(compact_field("failure_cause"), "afc");
        assert_eq!(compact_field("failure_detail"), "afd");
        assert_eq!(compact_field("actor_execution_environment"), "aex");
    }

    #[test]
    fn test_expand_field() {
        assert_eq!(expand_field("s"), "subject");
        assert_eq!(expand_field("r"), "relation");
        assert_eq!(expand_field("o"), "object");
        assert_eq!(expand_field("t"), "type");
        assert_eq!(expand_field("unknown"), "unknown");
        // OMS 1.2 fields
        assert_eq!(expand_field("inp"), "input");
        assert_eq!(expand_field("iserr"), "is_error");
        assert_eq!(expand_field("vstatus"), "verification_status");
        // OMS 1.1 aliases removed — these should pass through unchanged
        assert_eq!(expand_field("args"), "args");
        assert_eq!(expand_field("ok"), "ok");
        assert_eq!(expand_field("ct"), "ct");
        // OMS 1.3 fields
        assert_eq!(expand_field("osch"), "output_schema");
        assert_eq!(expand_field("ib"), "int:base_url");
        assert_eq!(expand_field("ihm"), "int:http_method");
        assert_eq!(expand_field("ic"), "int:connector");
        assert_eq!(expand_field("ipis"), "int:poll_interval_secs");
        // Async-execution fields
        assert_eq!(expand_field("ast"), "status");
        assert_eq!(expand_field("acid"), "correlation_id");
        assert_eq!(expand_field("axp"), "expires_at_sec");
        assert_eq!(expand_field("tdh"), "transient_definition_hash");
        assert_eq!(expand_field("afc"), "failure_cause");
        assert_eq!(expand_field("afd"), "failure_detail");
        assert_eq!(expand_field("aex"), "actor_execution_environment");
    }

    #[test]
    fn test_nested_compaction() {
        assert_eq!(compact_content_ref_field("uri"), "u");
        assert_eq!(compact_content_ref_field("mime_type"), "mt");
        assert_eq!(compact_embedding_ref_field("vector_id"), "vi");
        assert_eq!(compact_related_to_field("hash"), "h");
        assert_eq!(compact_related_to_field("relation_type"), "rl");
        // OMS 1.2 new embedding ref fields
        assert_eq!(compact_embedding_ref_field("chunk_index"), "ci");
        assert_eq!(compact_embedding_ref_field("chunk_text"), "ct");
    }

    #[test]
    fn test_no_compact_key_collisions() {
        // Verify every compact key maps to exactly one long name.
        // A collision means REVERSE_FIELD_MAP lost an entry.
        let map = &*FIELD_MAP;
        let reverse = &*REVERSE_FIELD_MAP;
        assert_eq!(map.len(), reverse.len(),
            "FIELD_MAP has {} entries but REVERSE_FIELD_MAP has {} — compact key collision detected",
            map.len(), reverse.len());
    }

    #[test]
    fn test_integration_profile_roundtrip() {
        // All 25 int:* keys must roundtrip through compact → expand
        let int_keys = [
            "int:base_url",
            "int:http_method",
            "int:http_path",
            "int:path_params",
            "int:query_params",
            "int:body_params",
            "int:response_mapping",
            "int:auth_type",
            "int:auth_scopes",
            "int:read_only",
            "int:connector",
            "int:docs_url",
            "int:rate_limit",
            "int:category",
            "int:sunset_date",
            "int:content_type",
            "int:poll_interval_secs",
            "int:cursor_field",
            "int:cursor_type",
            "int:webhook_path",
            "int:webhook_secret_header",
            "int:cron_expression",
            "int:timezone",
            "int:config_schema",
            "int:event_schema",
        ];
        for key in &int_keys {
            let short = compact_field(key);
            assert_ne!(short, *key, "int:* key '{}' has no compact mapping", key);
            let expanded = expand_field(short);
            assert_eq!(
                expanded, *key,
                "roundtrip failed for '{}' → '{}' → '{}'",
                key, short, expanded
            );
        }
    }
}
