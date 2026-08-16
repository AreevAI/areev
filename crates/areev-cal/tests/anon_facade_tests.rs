//! Facade front door for text anonymization (proposal P0): the three
//! explicit APIs, their JSON envelopes, and the fail-closed policy errors.

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
        r#"{"scope": "memory"}"#,
        r#"{"categories": {"date": "generalize:month"}}"#,
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
