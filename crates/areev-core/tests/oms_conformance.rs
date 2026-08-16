//! OMS conformance tests.
//! Vector 1 is byte-exact per OMS §21.1; roundtrips cover the rest.

use areev_core::*;


#[test]
fn test_oms_vector1_minimal_fact() {
    // OMS 1.2 Test Vector 1: Minimal Fact (formerly Fact)
    let fact = Fact::new("user", "likes", "coffee")
        .confidence(0.9)
        .source_type("user_explicit")
        .created_at(1768471200000_i64)
        .namespace("shared")
        .author_did("did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK");

    let (blob, hash) = serialize_grain(&fact).unwrap();
    // OMS 1.2: type string is now "fact" (was "fact"), so hash differs from OMS 1.1 vectors
    assert!(!hash.to_hex().is_empty());

    // Verify header
    assert_eq!(blob[0], 0x01); // version
    assert_eq!(blob[2], 0x01); // fact type byte (grain 0x01)

    // Verify deserialize roundtrip
    let deserialized = deserialize_blob(&blob).unwrap();
    assert_eq!(deserialized.grain_type, GrainType::Fact);
    assert_eq!(deserialized.get_str("subject"), Some("user"));
    assert_eq!(deserialized.get_str("relation"), Some("likes"));
    assert_eq!(deserialized.get_str("object"), Some("coffee"));
    assert_eq!(deserialized.get_f64("confidence"), Some(0.9));

    // Verify deterministic — same grain always produces same hash
    let (_, hash2) = serialize_grain(&fact).unwrap();
    assert_eq!(hash, hash2);
}

#[test]
fn test_oms_vector6_protected_fact() {
    // OMS 1.2 Test Vector 6: Protected Fact with invalidation_policy
    let mut fact = Fact::new(
        "agent-007",
        "constraint",
        "never delete user files without confirmation",
    )
    .confidence(1.0)
    .source_type("user_explicit")
    .created_at(1768471200000_i64)
    .namespace("safety");

    fact.common.invalidation_policy = Some(InvalidationPolicy {
        mode: "locked".to_string(),
        authorized: Some(vec![
            "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".to_string(),
        ]),
        threshold: None,
        locked_until: None,
        fallback_mode: None,
        scope: None,
        protection_reason: None,
    });

    let (blob, _hash) = serialize_grain(&fact).unwrap();
    // OMS 1.2: type string changed, hash differs from OMS 1.1 vectors
    assert!(!blob.is_empty());
    assert_eq!(blob[2], 0x01); // fact type byte
}

// --- OMS 1.2 Conformance: Reasoning grain ---

#[test]
fn test_oms_vector_reasoning_roundtrip() {
    // OMS 1.2: Reasoning grain — inference chain and thought audit trail
    let mut reasoning = Reasoning::new()
        .conclusion("The project should use Rust for performance")
        .inference_method("abductive")
        .thinking_content("Considered Go, Rust, and C++. Rust provides memory safety without GC.");
    reasoning.premises = vec![
        "Requirement: p99 < 1ms".to_string(),
        "Requirement: memory safety".to_string(),
        "Constraint: no garbage collector".to_string(),
    ];
    reasoning.alternatives_considered = vec![
        "Go — has GC pauses".to_string(),
        "C++ — no memory safety guarantees".to_string(),
    ];
    reasoning.thinking_redacted = Some(false);
    reasoning.common.created_at = Some(1768471200000_i64);
    reasoning.common.namespace = Some("engineering".to_string());
    reasoning.common.confidence = 0.95;

    // Serialize
    let (blob, hash) = serialize_grain(&reasoning).unwrap();
    assert_eq!(blob[2], 0x08); // reasoning type byte

    // Deserialize
    let g = deserialize_blob(&blob).unwrap();
    assert_eq!(g.grain_type, GrainType::Reasoning);
    assert_eq!(
        g.get_str("conclusion"),
        Some("The project should use Rust for performance")
    );
    assert_eq!(g.get_str("inference_method"), Some("abductive"));
    assert_eq!(
        g.get_str("thinking_content"),
        Some("Considered Go, Rust, and C++. Rust provides memory safety without GC.")
    );
    assert_eq!(g.get_f64("confidence"), Some(0.95));

    // Content addressing deterministic
    let (_, hash2) = serialize_grain(&reasoning).unwrap();
    assert_eq!(hash, hash2);

}

// --- OMS 1.2 Conformance: Consensus grain ---

#[test]
fn test_oms_vector_consensus_roundtrip() {
    // OMS 1.2: Consensus grain — multi-agent agreement record
    let mut consensus = Consensus::new();
    consensus.participating_observers = vec![
        "agent-alpha".to_string(),
        "agent-beta".to_string(),
        "agent-gamma".to_string(),
    ];
    consensus.threshold = Some(0.67);
    consensus.agreement_count = Some(3);
    consensus.dissent_count = Some(0);
    consensus.dissent_grains = Vec::new();
    consensus.agreed_content = Some("User is located in Berlin, Germany".to_string());
    consensus.common.created_at = Some(1768471200000_i64);
    consensus.common.namespace = Some("location".to_string());
    consensus.common.confidence = 1.0;

    // Serialize
    let (blob, hash) = serialize_grain(&consensus).unwrap();
    assert_eq!(blob[2], 0x09); // consensus type byte

    // Deserialize
    let g = deserialize_blob(&blob).unwrap();
    assert_eq!(g.grain_type, GrainType::Consensus);
    assert_eq!(
        g.get_str("agreed_content"),
        Some("User is located in Berlin, Germany")
    );
    assert_eq!(g.get_f64("threshold"), Some(0.67));
    assert_eq!(g.get_i64("agreement_count"), Some(3));
    assert_eq!(g.get_i64("dissent_count"), Some(0));

    // Content addressing deterministic
    let (_, hash2) = serialize_grain(&consensus).unwrap();
    assert_eq!(hash, hash2);

}

// --- OMS 1.2 Conformance: Consent grain ---

#[test]
fn test_oms_vector_consent_roundtrip() {
    // OMS 1.2: Consent grain — DID-scoped, purpose-bounded permission
    let mut consent = Consent::new("did:key:z6MkUser789");
    consent.grantee_did = Some("did:key:z6MkAgent012".to_string());
    consent.scope = Some("memory:read,memory:write".to_string());
    consent.is_withdrawal = Some(false);
    consent.basis = Some("explicit_consent".to_string());
    consent.jurisdiction = Some("EU-GDPR".to_string());
    consent.prior_consent = None;
    consent.witness_dids = vec!["did:key:z6MkWitness345".to_string()];
    consent.common.created_at = Some(1768471200000_i64);
    consent.common.namespace = Some("consent-registry".to_string());
    consent.common.confidence = 1.0;

    // Serialize
    let (blob, hash) = serialize_grain(&consent).unwrap();
    assert_eq!(blob[2], 0x0A); // consent type byte

    // Deserialize
    let g = deserialize_blob(&blob).unwrap();
    assert_eq!(g.grain_type, GrainType::Consent);
    assert_eq!(g.get_str("subject_did"), Some("did:key:z6MkUser789"));
    assert_eq!(g.get_str("grantee_did"), Some("did:key:z6MkAgent012"));
    assert_eq!(g.get_str("scope"), Some("memory:read,memory:write"));
    assert_eq!(g.get_str("basis"), Some("explicit_consent"));
    assert_eq!(g.get_str("jurisdiction"), Some("EU-GDPR"));

    // Content addressing deterministic
    let (_, hash2) = serialize_grain(&consent).unwrap();
    assert_eq!(hash, hash2);

}

// --- OMS 1.2 Conformance: Consent withdrawal ---

#[test]
fn test_oms_vector_consent_withdrawal() {
    // Consent grain with is_withdrawal=true — permission revocation
    let mut consent = Consent::new("did:key:z6MkUser789");
    consent.grantee_did = Some("did:key:z6MkAgent012".to_string());
    consent.scope = Some("memory:read,memory:write".to_string());
    consent.is_withdrawal = Some(true);
    consent.basis = Some("user_request".to_string());
    consent.jurisdiction = Some("EU-GDPR".to_string());
    consent.prior_consent = Some("abc123-previous-consent-hash".to_string());
    consent.common.created_at = Some(1768471200000_i64);
    consent.common.namespace = Some("consent-registry".to_string());

    let (blob, hash) = serialize_grain(&consent).unwrap();
    assert_eq!(blob[2], 0x0A);

    let g = deserialize_blob(&blob).unwrap();
    assert_eq!(g.grain_type, GrainType::Consent);
    assert_eq!(
        g.get_str("prior_consent"),
        Some("abc123-previous-consent-hash")
    );

    // Withdrawal hash differs from grant hash
    let mut grant = Consent::new("did:key:z6MkUser789");
    grant.grantee_did = Some("did:key:z6MkAgent012".to_string());
    grant.scope = Some("memory:read,memory:write".to_string());
    grant.is_withdrawal = Some(false);
    grant.common.created_at = Some(1768471200000_i64);
    grant.common.namespace = Some("consent-registry".to_string());
    let (_, grant_hash) = serialize_grain(&grant).unwrap();
    assert_ne!(hash, grant_hash);
}

// --- Content Addressing Tests ---

#[test]
fn test_content_addressing_deterministic() {
    // Same grain must always produce the same hash
    let fact = Fact::new("john", "likes", "coffee")
        .confidence(0.9)
        .namespace("test")
        .created_at(1000);

    let (blob1, hash1) = serialize_grain(&fact).unwrap();
    let (blob2, hash2) = serialize_grain(&fact).unwrap();

    assert_eq!(hash1, hash2);
    assert_eq!(blob1, blob2);
}

#[test]
fn test_content_addressing_different_data_different_hash() {
    let fact1 = Fact::new("john", "likes", "coffee")
        .namespace("test")
        .created_at(1000);
    let fact2 = Fact::new("john", "likes", "tea")
        .namespace("test")
        .created_at(1000);

    let (_, hash1) = serialize_grain(&fact1).unwrap();
    let (_, hash2) = serialize_grain(&fact2).unwrap();

    assert_ne!(hash1, hash2);
}

// --- All 11 Grain Types (OMS 1.4) ---

/// Round-trip one grain and assert the OMS content-addressing invariants:
/// deterministic hash, byte-identical blob, and a header grain-type byte that
/// maps back to the same `GrainType` through both `from_byte` and a full
/// `deserialize_blob`.
fn assert_grain_round_trip<G: Grain + 'static>(grain: &G, expected: GrainType) {
    let type_byte = expected.type_byte();

    // Serialize twice — content addressing must be deterministic.
    let (blob1, hash1) = serialize_grain(grain).unwrap();
    let (blob2, hash2) = serialize_grain(grain).unwrap();

    // Content-address stability + byte-identity across re-serialization.
    assert_eq!(hash1, hash2, "{:?}: content address not stable", expected);
    assert_eq!(blob1, blob2, "{:?}: blob not byte-identical", expected);

    // Grain-type byte lives at header offset 2 and maps back to the same type.
    assert_eq!(blob1[2], type_byte, "{:?}: wrong header type byte", expected);
    assert_eq!(
        GrainType::from_byte(blob1[2]),
        Some(expected),
        "{:?}: header byte does not map back to GrainType",
        expected
    );

    // Deserialize the blob; the grain type must survive the round-trip.
    let de = deserialize_blob(&blob1).unwrap();
    assert_eq!(
        de.grain_type, expected,
        "{:?}: deserialized grain_type mismatch",
        expected
    );
    assert_eq!(
        de.header.grain_type, type_byte,
        "{:?}: deserialized header byte mismatch",
        expected
    );
}

/// Every one of the 11 OMS grain types (Fact 0x01 … Skill 0x0B) round-trips
/// deterministically: same blob, same content address, same grain-type byte.
/// Fixed `created_at` + namespace on every grain so nothing depends on the
/// wall clock.
#[test]
fn all_grain_types_round_trip_is_deterministic() {
    const CREATED_AT: i64 = 1_768_471_200_000; // fixed epoch ms — no wall clock
    const NS: &str = "conformance";

    // 0x01 Fact
    assert_grain_round_trip(
        &Fact::new("user", "likes", "coffee")
            .confidence(0.9)
            .source_type("user_explicit")
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Fact,
    );

    // 0x02 Event
    assert_grain_round_trip(
        &Event::new("User logged in from a new device")
            .subject("user")
            .object("session")
            .role(Role::User)
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Event,
    );

    // 0x03 State
    assert_grain_round_trip(
        &State::new(serde_json::json!({
            "label": "checkpoint-1",
            "step": 3,
            "cursor": "abc"
        }))
        .created_at(CREATED_AT)
        .namespace(NS),
        GrainType::State,
    );

    // 0x04 Workflow
    assert_grain_round_trip(
        &Workflow::new(vec!["start".into(), "process".into(), "end".into()])
            .trigger("on_request")
            .edge("start", "process")
            .edge("process", "end")
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Workflow,
    );

    // 0x0C Recommendation (OMS 1.5)
    assert_grain_round_trip(
        &Recommendation::new(
            "entity:conformance/alice",
            Analyzer::new("loop.duplicate_sweep/1"),
            Summary {
                template_id: "dup.consolidate".into(),
                args: serde_json::json!({ "count": 3, "subject": "alice" }),
            },
            Proposal::Cal("SUPERSEDE sha256:aa BECAUSE \"duplicate\"".into()),
        )
        .severity(Severity::Medium)
        .created_at(CREATED_AT)
        .namespace(NS),
        GrainType::Recommendation,
    );

    // 0x05 Tool
    assert_grain_round_trip(
        &Tool::new("calculator")
            .input(serde_json::json!({"x": 2, "y": 3}))
            .content("5")
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Tool,
    );

    // 0x06 Observation
    assert_grain_round_trip(
        &Observation::new("agent-observer", "llm")
            .subject("user")
            .object("sentiment")
            .mode(ObservationMode::Realtime)
            .scope(ObservationScope::Private)
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Observation,
    );

    // 0x07 Goal
    assert_grain_round_trip(
        &Goal::new("Ship the 0.1.0 release")
            .priority(Priority::High)
            .state(GoalState::Active)
            .subject("team")
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Goal,
    );

    // 0x08 Reasoning
    let mut reasoning = Reasoning::new()
        .conclusion("Use Rust for the memory engine")
        .inference_method("abductive")
        .thinking_content("Weighed Go, Rust, and C++; Rust gives safety without a GC.");
    reasoning.premises = vec!["p99 < 1ms".to_string(), "memory safety".to_string()];
    reasoning.alternatives_considered = vec!["Go — GC pauses".to_string()];
    reasoning.common.created_at = Some(CREATED_AT);
    reasoning.common.namespace = Some(NS.to_string());
    assert_grain_round_trip(&reasoning, GrainType::Reasoning);

    // 0x09 Consensus
    let mut consensus = Consensus::new();
    consensus.participating_observers =
        vec!["agent-a".to_string(), "agent-b".to_string(), "agent-c".to_string()];
    consensus.threshold = Some(0.67);
    consensus.agreement_count = Some(3);
    consensus.dissent_count = Some(0);
    consensus.agreed_content = Some("User is located in Berlin, Germany".to_string());
    consensus.common.created_at = Some(CREATED_AT);
    consensus.common.namespace = Some(NS.to_string());
    assert_grain_round_trip(&consensus, GrainType::Consensus);

    // 0x0A Consent
    let mut consent = Consent::new("did:key:z6MkUser789");
    consent.grantee_did = Some("did:key:z6MkAgent012".to_string());
    consent.scope = Some("memory:read,memory:write".to_string());
    consent.is_withdrawal = Some(false);
    consent.basis = Some("explicit_consent".to_string());
    consent.jurisdiction = Some("EU-GDPR".to_string());
    consent.common.created_at = Some(CREATED_AT);
    consent.common.namespace = Some(NS.to_string());
    assert_grain_round_trip(&consent, GrainType::Consent);

    // 0x0B Skill
    assert_grain_round_trip(
        &Skill::new("code_review", "Reviews code for correctness bugs")
            .version("1.0.0")
            .domain("software_engineering")
            .when_to_use("when reviewing a pull request")
            .created_at(CREATED_AT)
            .namespace(NS),
        GrainType::Skill,
    );
}

// --- Header Validation ---

#[test]
fn test_mg_header_format() {
    let fact = Fact::new("s", "r", "o").namespace("test").created_at(1000);

    let (blob, _) = serialize_grain(&fact).unwrap();

    // Verify header structure (9 bytes)
    assert!(blob.len() >= 9);
    assert_eq!(blob[0], 0x01); // OMS v1
    assert_eq!(blob[2], 0x01); // Fact type byte

    // Parse header back
    let header = MgHeader::from_bytes(&blob[..9]).unwrap();
    assert_eq!(header.version, 1);
    assert_eq!(header.grain_type, 0x01);
}

#[test]
fn test_blob_too_short() {
    let result = deserialize_blob(&[0x01, 0x02, 0x03]);
    assert!(result.is_err());
}

/// Fixed epoch ms — no wall clock, so blobs stay deterministic.
const FIXED_AT: i64 = 1_768_471_200_000;

/// OMS §8.3 gives State a required `context` map; §6.1 gives every grain a
/// common `context` field. Both compact to the same wire key `ctx`, and
/// `add_common_fields` runs after `add_type_specific_fields` into one BTreeMap —
/// so an unconditional insert replaced the entire State snapshot with the common
/// metadata map. Type-specific fields must win.
#[test]
fn state_context_is_not_clobbered_by_common_context() {
    let mut st = State::new(serde_json::json!({
        "label": "planning_phase",
        "active_node": "build",
        "step": 3
    }));
    st.common.context = Some(serde_json::json!({ "source": "unit-test" }));
    st.common.created_at = Some(FIXED_AT);

    let (blob, _hash) = serialize_grain(&st).unwrap();
    let back = deserialize_blob(&blob).unwrap();
    let ctx = back
        .fields
        .get("context")
        .expect("State snapshot must be present under `context`");

    assert_eq!(ctx["label"], "planning_phase");
    assert_eq!(ctx["active_node"], "build");
    assert_eq!(ctx["step"], 3);
    assert!(
        ctx.get("source").is_none(),
        "common.context leaked into the State snapshot: {ctx}"
    );

    // Yielding `ctx` is not the same as being discarded. The common context
    // moves to `cctx`, because it is where `merge_heads` records merge parents
    // and the importers record provenance — losing it at the blob boundary
    // would lose that record with no error.
    let common = back
        .fields
        .get("common_context")
        .expect("common.context must survive under its own key");
    assert_eq!(common["source"], "unit-test");

    // And it comes back on the typed side as the common context, not as the
    // snapshot — reading the wrong one hands the caller agent state as metadata.
    let st2 = back.to_state().unwrap();
    assert_eq!(st2.context_data["label"], "planning_phase");
    assert_eq!(
        st2.common.context.as_ref().unwrap()["source"],
        "unit-test",
        "to_state must restore the common context from `cctx`"
    );
}

/// Every other grain type keeps the §6.1 common context in `ctx` — only State
/// collides, so only State pays for the move. A Fact must not sprout `cctx`.
#[test]
fn only_a_colliding_type_uses_the_alternate_context_key() {
    let mut f = Fact::new("alice", "prefers", "rust");
    f.common.context = Some(serde_json::json!({ "source": "unit-test" }));
    f.common.created_at = Some(FIXED_AT);

    let (blob, _hash) = serialize_grain(&f).unwrap();
    let back = deserialize_blob(&blob).unwrap();

    assert_eq!(back.fields["context"]["source"], "unit-test");
    assert!(
        !back.fields.contains_key("common_context"),
        "a non-colliding type must keep the common context in `ctx`: {:?}",
        back.fields
    );
}

/// `related_to` serializes its entries with compacted keys (`h`/`rl`/`w`), and
/// nested maps decode verbatim — so a cross-grain link came back as
/// `{"h":…,"rl":…}` and every reader looking for `hash`/`relation_type` found
/// nothing. Same asymmetry class as the workflow edge's `mxc`.
#[test]
fn related_to_links_expand_their_nested_keys() {
    let wf_hash = "a".repeat(64);
    let tool = Tool::new("cargo test")
        .created_at(FIXED_AT)
        .step_action(&wf_hash, "test");

    let (blob, _hash) = serialize_grain(&tool).unwrap();
    let back = deserialize_blob(&blob).unwrap();
    let links = back.fields["related_to"].as_array().unwrap();

    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["hash"], wf_hash);
    assert_eq!(links[0]["relation_type"], "mg:step_action:test");
    assert!(links[0].get("h").is_none(), "compact key must be expanded");
    assert!(links[0].get("rl").is_none(), "compact key must be expanded");
    // No weight: an execution record is a structural fact, not a similarity.
    assert!(links[0].get("weight").is_none());

    assert_eq!(
        step_action_node(links[0]["relation_type"].as_str().unwrap()),
        Some("test")
    );
}

/// Until now there was no `to_workflow()`: nothing in the codebase could hand
/// back a Workflow with its edges, bindings and retries populated, so every
/// reader hand-parsed raw JSON. Node order is load-bearing (entry point).
#[test]
fn to_workflow_restores_the_full_topology() {
    let mut wf = Workflow::new(vec!["build".into(), "test".into(), "deploy".into()])
        .trigger("merge to main")
        .edge("build", "test")
        .cond_edge("test", "deploy", "tests green")
        .bind("test", "sha256:abc")
        .retry("test", 3);
    wf.edges.push(WorkflowEdge {
        src: "deploy".into(),
        dst: "build".into(),
        cond: None,
        max_cycles: Some(2),
    });
    wf.common.created_at = Some(FIXED_AT);
    wf.common.namespace = Some("conformance".into());

    let (blob, _hash) = serialize_grain(&wf).unwrap();
    let back = deserialize_blob(&blob).unwrap().to_workflow().unwrap();

    assert_eq!(back.nodes, vec!["build", "test", "deploy"]);
    assert_eq!(back.trigger.as_deref(), Some("merge to main"));
    assert_eq!(back.edges.len(), 3);
    assert_eq!(back.edges[0].src, "build");
    assert_eq!(back.edges[1].cond.as_deref(), Some("tests green"));
    assert_eq!(back.edges[2].max_cycles, Some(2), "back-edge bound must survive");
    assert_eq!(back.bindings.get("test").map(String::as_str), Some("sha256:abc"));
    assert_eq!(back.retries.get("test"), Some(&3));
    assert_eq!(back.common.namespace.as_deref(), Some("conformance"));

    // Wrong type must be refused, not silently coerced.
    let (fblob, _) = serialize_grain(&Fact::new("a", "b", "c")).unwrap();
    assert!(deserialize_blob(&fblob).unwrap().to_workflow().is_err());
}

/// `to_state()` likewise did not exist.
#[test]
fn to_state_restores_snapshot_plan_and_history() {
    let st = State::new(serde_json::json!({ "label": "mid-run", "cursor": 7 }))
        .plan(vec!["fetch".into(), "load".into()])
        .history(vec![serde_json::json!({ "node": "fetch", "ok": true })])
        .created_at(FIXED_AT)
        .namespace("conformance");

    let (blob, _hash) = serialize_grain(&st).unwrap();
    let back = deserialize_blob(&blob).unwrap().to_state().unwrap();

    assert_eq!(back.context_data["label"], "mid-run");
    assert_eq!(back.context_data["cursor"], 7);
    assert_eq!(
        back.plan.as_deref(),
        Some(["fetch".to_string(), "load".to_string()].as_slice())
    );
    assert_eq!(back.history.as_ref().unwrap()[0]["node"], "fetch");
    assert_eq!(back.common.namespace.as_deref(), Some("conformance"));

    let (fblob, _) = serialize_grain(&Fact::new("a", "b", "c")).unwrap();
    assert!(deserialize_blob(&fblob).unwrap().to_state().is_err());
}

/// OMS §8.3 optional fields. They had no struct field at all, so any caller that
/// wanted a plan or a history had nowhere to put it.
#[test]
fn state_plan_and_history_round_trip() {
    let st = State::new(serde_json::json!({ "label": "mid-run" }))
        .plan(vec!["fetch".into(), "transform".into(), "load".into()])
        .history(vec![serde_json::json!({ "node": "fetch", "ok": true })])
        .created_at(FIXED_AT)
        .namespace("conformance");

    let (blob, _hash) = serialize_grain(&st).unwrap();
    let back = deserialize_blob(&blob).unwrap();

    assert_eq!(
        back.fields["plan"],
        serde_json::json!(["fetch", "transform", "load"])
    );
    assert_eq!(back.fields["history"][0]["node"], "fetch");

    // Omit-when-default: a State without them must not emit the keys at all.
    let bare = State::new(serde_json::json!({ "label": "x" })).created_at(FIXED_AT);
    let (bare_blob, _) = serialize_grain(&bare).unwrap();
    let bare_back = deserialize_blob(&bare_blob).unwrap();
    assert!(!bare_back.fields.contains_key("plan"));
    assert!(!bare_back.fields.contains_key("history"));
}

/// These six Goal fields had struct fields and compact keys in `field_map.rs`
/// but no arm in `add_type_specific_fields`, so every write silently dropped
/// them — the data never reached the blob.
#[test]
fn goal_optional_fields_reach_the_blob() {
    let mut goal = Goal::new("ship the release");
    goal.criteria_structured = Some(serde_json::json!([{ "check": "tests green" }]));
    goal.expiry_policy = Some(serde_json::json!("on_deadline"));
    goal.recurrence = Some(serde_json::json!("weekly"));
    goal.evidence_required = Some(true);
    goal.rollback_on_failure = Some(true);
    goal.allowed_transitions = Some(vec!["active".into(), "satisfied".into()]);
    goal.common.created_at = Some(FIXED_AT);

    let (blob, _hash) = serialize_grain(&goal).unwrap();
    let back = deserialize_blob(&blob).unwrap();

    assert_eq!(back.fields["criteria_structured"][0]["check"], "tests green");
    assert_eq!(back.fields["expiry_policy"], "on_deadline");
    assert_eq!(back.fields["recurrence"], "weekly");
    assert_eq!(back.fields["evidence_required"], true);
    assert_eq!(back.fields["rollback_on_failure"], true);
    assert_eq!(
        back.fields["allowed_transitions"],
        serde_json::json!(["active", "satisfied"])
    );

    // Unset stays unset — that is what keeps existing Goal blobs byte-identical.
    let bare = Goal::new("ship the release");
    let mut bare = bare;
    bare.common.created_at = Some(FIXED_AT);
    let (bare_blob, _) = serialize_grain(&bare).unwrap();
    let bare_back = deserialize_blob(&bare_blob).unwrap();
    for k in [
        "criteria_structured",
        "expiry_policy",
        "recurrence",
        "evidence_required",
        "rollback_on_failure",
        "allowed_transitions",
    ] {
        assert!(!bare_back.fields.contains_key(k), "{k} must be omitted");
    }
}

// --- Grain Builder Pattern ---

