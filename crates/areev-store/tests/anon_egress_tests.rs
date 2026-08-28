//! The egress boundary (docs/anonymization-proposal.md P1): `anon:<ns>`
//! policy CRUD, coverage of every grain-returning read, session-scope token
//! stability + in-process mapping custody, fail-closed poisoned rows, the
//! floor, audit mode, replication, and the deliberate exemptions.

use areev_core::types::{Event, Fact, Grain};
use areev_store::{Axis, Direction, Areev};
use std::collections::HashMap;
use tempfile::TempDir;

/// A grain's fields rendered for leak-scanning, with every **number** dropped.
///
/// The canaries these tests scan for include a bare four-digit PIN, and a
/// grain carries machine-generated integers — `created_at` above all — that
/// can contain any short digit run by coincidence. That is not hypothetical:
/// CI failed on `created_at` 1787890614620, which contains "1462", reporting
/// a leak in a grain whose text was correctly redacted to
/// `[PERSON_1] said his email is [EMAIL_1]`. The bug is time-dependent, fires
/// on any platform, and says nothing about pseudonymization.
///
/// Pseudonymization redacts *text*, so scan the text. Nesting is preserved
/// (a leak inside `content_blocks` still counts) — only numeric leaves go.
fn scannable(fields: &HashMap<String, serde_json::Value>) -> String {
    fn strip(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Number(_) => serde_json::Value::Null,
            serde_json::Value::Array(a) => a.iter().map(strip).collect(),
            serde_json::Value::Object(o) => {
                o.iter().map(|(k, v)| (k.clone(), strip(v))).collect()
            }
            other => other.clone(),
        }
    }
    let scrubbed: serde_json::Map<String, serde_json::Value> =
        fields.iter().map(|(k, v)| (k.clone(), strip(v))).collect();
    serde_json::to_string(&scrubbed).unwrap()
}

fn open_mem(dir: &TempDir, name: &str) -> Areev {
    Areev::open(dir.path().join(name).to_str().unwrap()).unwrap()
}

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9).source_type("user_explicit");
    f.common.namespace = Some(ns.to_string());
    f
}

const EGRESS: &str = r#"{"mode": "egress"}"#;
const SESSION: &str = r#"{"mode": "egress", "scope": "session"}"#;

#[test]
fn policy_crud_stamps_and_refuses_unbuilt_modes() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");

    assert!(m.anon_policies().unwrap().is_empty());
    m.set_anon_policy("caller", EGRESS).unwrap();
    let policies = m.anon_policies().unwrap();
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].0, "caller");
    assert_eq!(policies[0].1.mode, "egress");
    assert_eq!(m.anon_declared(), vec![("caller".to_string(), "egress".to_string())]);
    assert_eq!(m.anon_active_mode("caller").unwrap().as_deref(), Some("egress"));
    assert_eq!(m.anon_active_mode("other").unwrap(), None);

    // Setting a policy stamps min_reader_version so an older build warns.
    assert_eq!(
        m.meta_get("min_reader_version").unwrap().as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    // Unbuilt modes refuse at set time — declared-but-unenforced protection
    // is the failure this feature must never have.
    let err = m.set_anon_policy("caller", r#"{"mode": "ingress"}"#).unwrap_err().to_string();
    assert!(err.starts_with("VAL-E001"), "{err}");
    let err = m.set_anon_policy("caller", r#"{"mode": "both"}"#).unwrap_err().to_string();
    assert!(err.starts_with("VAL-E001"), "{err}");

    m.clear_anon_policy("caller").unwrap();
    assert!(m.anon_policies().unwrap().is_empty());
    assert_eq!(m.anon_active_mode("caller").unwrap(), None);
}

#[test]
fn egress_covers_the_grain_returning_reads() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");
    m.set_anon_policy("caller", EGRESS).unwrap();

    let h1 = m.add(&fact("caller", "caller:john", "prefers", "window seat")).unwrap();
    let _h2 = m
        .add(&fact("caller", "caller:john", "pin_note", "my user name is john, and pin number is 1462"))
        .unwrap();
    let mut ev = Event::new("john said his email is j@x.io");
    ev.common.namespace = Some("caller".to_string());
    ev.session_id = Some("sess-1".to_string());
    ev.run_id = Some("run-1".to_string());
    let hev = m.add(&ev).unwrap();

    let assert_clean = |label: &str, text: &str| {
        assert!(
            !text.contains("john") && !text.contains("1462") && !text.contains("j@x.io"),
            "{label} leaked raw values: {text}"
        );
    };

    // recall / get / latest / recent / thread_tail / run_trace / entity_at
    for g in m.recall("caller", "caller:john", None, 16).unwrap() {
        assert_clean("recall", &scannable(&g.fields));
    }
    let g = m.get(&h1).unwrap();
    assert_clean("get", &scannable(&g.fields));
    assert_eq!(g.fields["subject"].as_str().unwrap(), "[PERSON_1]");
    let g = m.latest("caller", "caller:john", "prefers").unwrap().unwrap();
    assert_clean("latest", &scannable(&g.fields));
    for g in m.recent("caller", None, 16).unwrap() {
        assert_clean("recent", &scannable(&g.fields));
    }
    for g in m.thread_tail("caller", "sess-1", 8).unwrap() {
        assert_clean("thread_tail", &scannable(&g.fields));
    }
    for g in m.run_trace("caller", "run-1", 8).unwrap() {
        assert_clean("run_trace", &scannable(&g.fields));
    }
    let g = m
        .entity_at("caller", "caller:john", "prefers", i64::MAX, Axis::Knowledge)
        .unwrap()
        .unwrap();
    assert_clean("entity_at", &scannable(&g.fields));

    // grains_by_object / grains_derived_from
    for g in m.grains_by_object("caller", "window seat", 8).unwrap() {
        assert_clean("grains_by_object", &scannable(&g.fields));
    }
    let _ = m.grains_derived_from(&hev).unwrap(); // covered path, may be empty

    // history: the object projection
    let hist = m.history("caller", "caller:john", "prefers").unwrap();
    assert!(!hist.is_empty());

    // graph reads: bare identity strings
    let subs = m.subjects_with_relation("caller", "prefers").unwrap();
    assert_eq!(subs, vec!["[PERSON_1]".to_string()], "subjects leaked: {subs:?}");
    let rel = m
        .related("caller", "caller:john", &["prefers"], Direction::Out, 2, 16)
        .unwrap();
    for s in &rel {
        assert!(!s.contains("john"), "related leaked: {rel:?}");
    }

    // The session mapping is available in process (D5 custody) and round-trips.
    let mappings = m.anon_mappings().unwrap();
    assert!(!mappings.is_empty());
    let (_, id, map) = &mappings[0];
    assert_eq!(id.len(), 16);
    assert!(map.values().any(|v| v == "caller:john"), "mapping: {map:?}");
}

#[test]
fn session_scope_is_stable_across_calls_and_rehydrates() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");
    m.set_anon_policy("caller", SESSION).unwrap();
    m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
    m.add(&fact("caller", "caller:mary", "prefers", "coffee")).unwrap();

    let a = m.recall("caller", "caller:john", None, 4).unwrap();
    let b = m.recall("caller", "caller:john", None, 4).unwrap();
    let tok_a = a[0].fields["subject"].as_str().unwrap().to_string();
    let tok_b = b[0].fields["subject"].as_str().unwrap().to_string();
    assert_eq!(tok_a, tok_b, "session scope must keep tokens stable");
    let c = m.recall("caller", "caller:mary", None, 4).unwrap();
    assert_ne!(c[0].fields["subject"].as_str().unwrap(), tok_a);

    // Rehydrate a model reply through the in-process mapping.
    let (_, _, map) = &m.anon_mappings().unwrap()[0];
    let reply = format!("Understood, {tok_a} prefers tea.");
    let back = areev_core::anon::rehydrate(&reply, map).unwrap();
    assert_eq!(back.text, "Understood, caller:john prefers tea.");
}

#[test]
fn unreadable_policy_row_fails_reads_closed_but_is_repairable() {
    let dir = TempDir::new().unwrap();
    {
        let mut m = open_mem(&dir, "a.db");
        m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
        m.meta_put("anon:caller", "{not json").unwrap(); // simulate a row from a future build
    }
    let mut m = open_mem(&dir, "a.db");
    assert!(
        m.open_warnings().iter().any(|w| w.contains("anon:caller")),
        "poisoned row must surface at open: {:?}",
        m.open_warnings()
    );
    let err = m.recall("caller", "caller:john", None, 4).unwrap_err().to_string();
    assert!(err.starts_with("VAL-E001"), "reads must fail closed, got: {err}");
    // ...but repair still works, and reads come back.
    m.clear_anon_policy("caller").unwrap();
    assert_eq!(m.recall("caller", "caller:john", None, 4).unwrap().len(), 1);
}

#[test]
fn floor_forces_egress_and_never_weakens() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");
    m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();

    // No declared policy: raw by default, transformed under the floor.
    assert_eq!(
        m.recall("caller", "caller:john", None, 4).unwrap()[0].fields["subject"],
        "caller:john"
    );
    m.set_anonymize_egress_floor(true);
    assert!(m.anonymize_egress_floor());
    assert_eq!(m.anon_active_mode("caller").unwrap().as_deref(), Some("egress"));
    let got = m.recall("caller", "caller:john", None, 4).unwrap();
    assert_eq!(got[0].fields["subject"], "[PERSON_1]");

    // A declared "off" is overridden upward by the floor.
    m.set_anon_policy("caller", r#"{"mode": "off"}"#).unwrap();
    assert_eq!(m.anon_active_mode("caller").unwrap().as_deref(), Some("egress"));
    m.set_anonymize_egress_floor(false);
    assert_eq!(m.anon_active_mode("caller").unwrap(), None);
}

#[test]
fn audit_mode_counts_without_transforming() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");
    m.set_anon_policy("caller", r#"{"mode": "audit"}"#).unwrap();
    m.add(&fact("caller", "caller:john", "note", "pin number is 1462, mail j@x.io")).unwrap();

    let got = m.recall("caller", "caller:john", None, 4).unwrap();
    assert_eq!(got[0].fields["subject"], "caller:john", "audit must not transform");
    let counts = m.anon_audit_counts();
    assert!(
        counts.iter().any(|(ns, cat, n)| ns == "caller" && cat == "pin" && *n > 0),
        "expected pin count, got {counts:?}"
    );
    assert!(counts.iter().any(|(_, cat, _)| cat == "email"), "{counts:?}");
}

#[test]
fn anon_policy_replicates_write_if_absent() {
    let dir = TempDir::new().unwrap();
    let bundle = dir.path().join("delta.mgb");
    let bundle = bundle.to_str().unwrap();

    let mut a = open_mem(&dir, "a.db");
    a.set_anon_policy("caller", EGRESS).unwrap();
    a.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
    a.bundle_since(0, bundle).unwrap();

    // Absent on B → the policy lands and takes effect immediately.
    let mut b = open_mem(&dir, "b.db");
    let stats = b.import_bundle(bundle).unwrap();
    assert!(stats.meta_applied >= 1, "{stats:?}");
    assert_eq!(b.anon_active_mode("caller").unwrap().as_deref(), Some("egress"));
    let got = b.recall("caller", "caller:john", None, 4).unwrap();
    assert_eq!(got[0].fields["subject"], "[PERSON_1]");

    // Present on C → a sync never silently swaps a live policy.
    let mut c = open_mem(&dir, "c.db");
    c.set_anon_policy("caller", r#"{"mode": "audit"}"#).unwrap();
    c.import_bundle(bundle).unwrap();
    assert_eq!(c.anon_active_mode("caller").unwrap().as_deref(), Some("audit"));
}

#[test]
fn authz_and_dsar_and_replay_exemptions_hold() {
    let dir = TempDir::new().unwrap();
    let mut m = open_mem(&dir, "a.db");

    // A grant in agent:authz keeps working under a policy on that ns.
    let grant = fact("agent:authz", "operator", "mg:permits", "read,write ON caller");
    m.add(&grant).unwrap();
    m.set_anon_policy("agent:authz", EGRESS).unwrap();
    let grants = m.authz_grants("operator").unwrap();
    assert!(!grants.is_empty(), "authz must read raw grants under any policy");

    // The DSAR read stays a faithful disclosure (REQ-ERASE-9 symmetry).
    m.set_anon_policy("caller", EGRESS).unwrap();
    m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
    let report = m.subject_report("caller", "caller:john").unwrap();
    let text = serde_json::to_string(&report.grains.iter().map(|g| &g.fields).collect::<Vec<_>>())
        .unwrap();
    assert!(text.contains("caller:john"), "subject_report must disclose raw data");

    // The machine-replay read stays raw too.
    let mut ev = Event::new("step output for caller:john");
    ev.common.namespace = Some("caller".to_string());
    ev.run_id = Some("run-9".to_string());
    m.add(&ev).unwrap();
    let page = m.run_grains("caller", "run-9", 0, 16).unwrap();
    let raw = serde_json::to_string(&page[0].1.fields).unwrap();
    assert!(raw.contains("caller:john"), "run_grains is the exempt replay read");
    // ...while the model-facing view of the same run is covered.
    let trace = m.run_trace("caller", "run-9", 16).unwrap();
    let covered = serde_json::to_string(&trace[0].fields).unwrap();
    assert!(!covered.contains("caller:john"), "run_trace must be covered: {covered}");
}

/// The regression `scannable` exists for, pinned in both directions: a number
/// whose digits coincidentally spell a canary must not read as a leak, and a
/// real leak in a nested text field must still be caught.
#[test]
fn a_timestamp_cannot_read_as_a_leak_but_nested_text_still_can() {
    let mut fields = HashMap::new();
    // The exact `created_at` that failed CI on 2026-08-28: 1787890614620
    // contains "1462", the PIN canary, at offset 8.
    fields.insert("created_at".to_string(), serde_json::json!(1_787_890_614_620i64));
    fields.insert(
        "content".to_string(),
        serde_json::json!("[PERSON_1] said his email is [EMAIL_1]"),
    );
    let text = scannable(&fields);
    assert!(!text.contains("1462"), "a wall-clock number must not read as the PIN: {text}");
    assert!(text.contains("[PERSON_1]"), "redacted text must survive scanning: {text}");

    // The scrub must not buy its determinism by blinding the scan: a genuine
    // leak nested inside a structure is still a leak.
    let mut leaky = HashMap::new();
    leaky.insert("blocks".to_string(), serde_json::json!([{"text": "pin number is 1462"}]));
    assert!(
        scannable(&leaky).contains("1462"),
        "a real leak in nested text must still be caught"
    );
}
