//! Facade front door for text anonymization (proposal P0): the three
//! explicit APIs, their JSON envelopes, and the fail-closed policy errors.

use areev_cal::facade::CalStoreFacade;
use areev_cal::AreevFacade;
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    (AreevFacade::with_session(m, Some("caller".to_string()), None), dir)
}

#[test]
fn scan_anonymize_rehydrate_round_trip() {
    let (facade, _d) = setup();

    let scanned = facade
        .scan_text("my user name is john, and pin number is 1462", None)
        .unwrap();
    let scanned: serde_json::Value = serde_json::from_str(&scanned).unwrap();
    let cats: Vec<&str> = scanned["detections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["category"].as_str().unwrap())
        .collect();
    assert!(cats.contains(&"pin"), "scan missed the pin: {cats:?}");

    let out = facade
        .anonymize_text("mail a@b.co, pin number is 1462", None, Some("00ff"))
        .unwrap();
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    let anon_text = out["text"].as_str().unwrap();
    assert!(!anon_text.contains("a@b.co") && !anon_text.contains("1462"), "{anon_text}");
    assert!(out["mapping_id"].as_str().unwrap().len() == 16);

    // Tokens are deterministic per assembly, so the model reply is literal.
    assert_eq!(anon_text, "mail [EMAIL_1], pin number is [PIN_1]");
    let mapping_json = serde_json::to_string(&out["mapping"]).unwrap();
    let back = facade
        .rehydrate_text("sent to [EMAIL_1] after checking pin [PIN_1]", &mapping_json)
        .unwrap();
    let back: serde_json::Value = serde_json::from_str(&back).unwrap();
    assert_eq!(
        back["text"].as_str().unwrap(),
        "sent to a@b.co after checking pin 1462"
    );
    assert_eq!(back["replaced"], 2);
}

#[test]
fn policy_errors_are_hard_val_errors() {
    let (facade, _d) = setup();
    // Unknown fields, unbuilt scopes, and unbuilt actions all refuse loudly
    // (D3: an unreadable policy must not silently mean "no policy").
    for bad in [
        r#"{"surprise": true}"#,
        r#"{"default_action": "shred"}"#,
        r#"{"categories": {"date": "generalize:eon"}}"#,
    ] {
        let err = facade.scan_text("hello", Some(bad)).unwrap_err().to_string();
        assert!(err.starts_with("VAL-E001"), "want VAL-E001, got: {err}");
    }
    let err = facade
        .rehydrate_text("x", "{\"bad\": 1}")
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("VAL-E001"), "want VAL-E001, got: {err}");
}

#[test]
fn known_identities_propagate_from_the_store_into_free_text() {
    // GitHub issue #32: a subject interned as a grain (the same way
    // egress-grain reads build their propagation table) must now also be
    // detectable/pseudonymizable through the free-text APIs, not only
    // through recall/CAL — same matcher, same category assignment.
    use areev_cal::{CalExecutor, CalExecutorConfig};

    let (facade, _d) = setup();
    facade
        .with_store(|m| m.set_anon_policy("caller", r#"{"mode": "egress"}"#))
        .unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    ex.execute(
        r#"ADD fact SET subject = "Kenneth Shea" SET relation = "role" SET object = "sell-side banker" SET namespace = "caller" REASON "test""#,
        &facade,
    )
    .unwrap();

    let scanned = facade
        .scan_text("Kenneth Shea sent the Project Falcon NDA.", None)
        .unwrap();
    let scanned: serde_json::Value = serde_json::from_str(&scanned).unwrap();
    let cats: Vec<&str> = scanned["detections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["category"].as_str().unwrap())
        .collect();
    assert!(cats.contains(&"person"), "scan missed the propagated subject: {cats:?}");

    let out = facade
        .anonymize_text("Kenneth Shea sent the Project Falcon NDA.", None, None)
        .unwrap();
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    let anon_text = out["text"].as_str().unwrap();
    assert!(!anon_text.contains("Kenneth Shea"), "raw identity leaked: {anon_text}");
    assert!(anon_text.contains("[PERSON_1]"), "expected pseudonym: {anon_text}");
    // Project Falcon was never interned as a subject and the policy carried
    // no `known` entry for it — it must be left alone.
    assert!(anon_text.contains("Project Falcon"));
}

#[test]
fn policy_known_lets_callers_inject_identities_the_store_never_interned() {
    // Issue #32's suggested option 2: identities from outside the store
    // (an email header, a CRM row) without writing them as grains first.
    let (facade, _d) = setup();
    let policy = r#"{"known": [{"value": "Project Falcon", "category": "custom"}]}"#;
    let out = facade
        .anonymize_text("Kenneth Shea sent the Project Falcon NDA.", Some(policy), None)
        .unwrap();
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    let anon_text = out["text"].as_str().unwrap();
    assert!(!anon_text.contains("Project Falcon"), "{anon_text}");
    assert!(anon_text.contains("[CUSTOM_1]"), "{anon_text}");
    // Kenneth Shea is neither interned nor listed in `known` — untouched.
    assert!(anon_text.contains("Kenneth Shea"));
}

#[test]
fn cal_payload_carries_the_egress_flag_and_transformed_fields() {
    use areev_cal::{CalExecutor, CalExecutorConfig};

    let (facade, _d) = setup();
    facade
        .with_store(|m| m.set_anon_policy("caller", r#"{"mode": "egress"}"#))
        .unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    ex.execute(
        r#"ADD fact SET subject = "caller:john" SET relation = "prefers" SET object = "call me at +1 415 555 0142" SET namespace = "caller" REASON "test""#,
        &facade,
    )
    .unwrap();

    let res = ex
        .execute(r#"RECALL facts WHERE subject = "caller:john""#, &facade)
        .unwrap();
    let payload = res.payload_json().unwrap();

    // The flag rides the payload with mapping ids only — never the mapping.
    assert_eq!(payload["anonymized"]["namespaces"][0], "caller");
    let flat = payload.to_string();
    assert!(!flat.contains("caller:john"), "raw identity leaked: {flat}");
    assert!(!flat.contains("415 555"), "raw phone leaked: {flat}");
    assert!(flat.contains("[PERSON_1]"), "expected pseudonym: {flat}");
    assert!(!flat.contains("\"mapping\":"), "the mapping must never ride a payload");

    // The in-process caller still holds the mapping for rehydration (D5).
    let mappings = facade.with_store(|m| m.anon_mappings()).unwrap();
    assert!(mappings.iter().any(|(_, _, map)| map.values().any(|v| v == "caller:john")));
}

#[test]
fn structured_writes_pass_the_ingress_boundary() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open_encrypted(dir.path().join("e.db").to_str().unwrap(), [9u8; 32]).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    facade
        .with_store(|m| m.set_anon_policy("caller", r#"{"mode": "ingress"}"#))
        .unwrap();

    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), serde_json::json!("caller:john"));
    fields.insert("relation".into(), serde_json::json!("prefers"));
    fields.insert("object".into(), serde_json::json!("call me at +1 415 555 0142"));
    fields.insert("namespace".into(), serde_json::json!("caller"));
    let (hash, _) = facade.cal_add_if_novel("fact", &fields).unwrap();

    // mode=ingress → reads are raw: what we read back IS what is stored.
    let g = facade.with_store(|m| m.get(&hash)).unwrap();
    let subject = g.fields["subject"].as_str().unwrap();
    let object = g.fields["object"].as_str().unwrap();
    assert!(subject.starts_with("[PERSON_"), "stored subject leaked: {subject}");
    assert!(!object.contains("415 555"), "stored object leaked: {object}");
    assert!(object.contains("[PHONE_"), "expected value-derived token: {object}");
}

#[test]
fn reveal_is_audited_by_fingerprint_never_identity() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open_encrypted(dir.path().join("v.db").to_str().unwrap(), [3u8; 32]).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    facade
        .with_store(|m| {
            m.set_anon_policy(
                "caller",
                r#"{"mode": "egress", "scope": "session", "vault": true}"#,
            )
        })
        .unwrap();

    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), serde_json::json!("caller:john"));
    fields.insert("relation".into(), serde_json::json!("prefers"));
    fields.insert("object".into(), serde_json::json!("tea"));
    fields.insert("namespace".into(), serde_json::json!("caller"));
    facade.cal_add_if_novel("fact", &fields).unwrap();
    let mut params = areev_cal::store_types::RecallParams::default();
    params.subject = Some("caller:john".to_string());
    params.namespace = Some("caller".to_string());
    params.limit = Some(4);
    let hits = facade.recall(&params).unwrap();
    let token = hits[0].grain.fields["subject"].as_str().unwrap().to_string();

    let out = facade.reveal_tokens("caller", std::slice::from_ref(&token)).unwrap();
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["revealed"][&token], "caller:john");

    // The audit Observation exists in agent:authz, carries the verb and a
    // fingerprint — and never the identity itself.
    let audits = facade
        .with_store(|m| m.recent("agent:authz", None, 8))
        .unwrap();
    let reveal_audit = audits
        .iter()
        .map(|g| serde_json::to_string(&g.fields).unwrap())
        .find(|s| s.contains("reveal"))
        .expect("a reveal audit Observation must be written");
    assert!(
        !reveal_audit.contains("caller:john"),
        "the audit grain must not re-identify: {reveal_audit}"
    );
    assert!(
        reveal_audit.contains(&areev_core::authz::subject_fingerprint("caller:john")),
        "the audit names the fingerprint: {reveal_audit}"
    );
}
