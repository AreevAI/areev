//! CAL 1.3 §8.14 Tier-2 destruction, end to end: `FORGET SUBJECT` and
//! `PURGE OLDER THAN` through text → executor → facade → store, with the
//! mandatory-BECAUSE rule, the erase-verb gate, and the audit Observation.
//!
//! Per the testing rules: every filter asserts what it must NOT touch — an
//! erasure that over-reaches is worse than one that fails.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Event, Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn payload(ex: &CalExecutor, f: &AreevFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

fn grain_count(ex: &CalExecutor, f: &AreevFacade, q: &str) -> usize {
    payload(ex, f, q)["grains"].as_array().map(|a| a.len()).unwrap_or(0)
}

#[test]
fn forget_subject_erases_the_identity_and_only_the_identity() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
        m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(2_000)).unwrap();
        m.add(&Fact::new("alice", "prefers", "coffee").namespace("caller").created_at(3_000)).unwrap();
    });

    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "gdpr erasure request""#);
    assert_eq!(v["type"], "forgotten", "{v}");
    assert_eq!(v["target"], "subject:pat");
    assert!(v["count"].as_u64().unwrap() >= 2, "{v}");

    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 0);
    // The neighbor must survive — erasure takes an identity, not a namespace.
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "alice""#), 1);

    // The audit Observation is a grain: principal, verb, target, reason.
    let audit = payload(
        &ex,
        &f,
        r#"RECALL observations WHERE namespace = "agent:authz""#,
    );
    let grains = audit["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 1, "{audit}");
    let fields = &grains[0]["fields"];
    // The audit target carries a FINGERPRINT, not the identity: this grain
    // is immutable, replicates, and lands in archives, so naming the erased
    // subject here would undo the erasure it records.
    let fp = areev_core::authz::subject_fingerprint("pat");
    assert_eq!(fields["subject"], format!("subject:{fp} ns:caller"));
    assert!(
        !fields["subject"].as_str().unwrap().contains("pat"),
        "the erased identity must not survive in its own audit record: {fields}"
    );
    assert_eq!(fields["object"], "erase");
    let ctx = &fields["context"];
    assert_eq!(ctx["because"], "gdpr erasure request");
    assert_eq!(ctx["audit"], "tier2");
    assert_eq!(ctx["subject_ref"], "sha256-64/hex", "the scheme is self-describing");
}

#[test]
fn because_is_mandatory_on_subject_and_purge_but_not_on_hash() {
    let (ex, f, _dir) = setup();
    let err = ex
        .execute(r#"FORGET SUBJECT "pat""#, &f)
        .expect_err("subject erasure without BECAUSE must not parse");
    assert!(err.to_string().contains("CAL-E018"), "{err}");

    let err = ex
        .execute(r#"PURGE OLDER THAN 30d"#, &f)
        .expect_err("purge without BECAUSE must not parse");
    assert!(err.to_string().contains("CAL-E018"), "{err}");

    // The hash form predates the rule: BECAUSE stays optional there.
    let h = f.with_store(|m| {
        m.add(&Fact::new("x", "r", "o").namespace("caller").created_at(1_000)).unwrap()
    });
    let v = payload(&ex, &f, &format!("FORGET sha256:{}", h.to_hex()));
    assert_eq!(v["type"], "forgotten", "{v}");
}

#[test]
fn forget_hash_records_its_reason_in_the_audit() {
    let (ex, f, _dir) = setup();
    let h = f.with_store(|m| {
        m.add(&Fact::new("x", "r", "o").namespace("caller").created_at(1_000)).unwrap()
    });
    let v = payload(
        &ex,
        &f,
        &format!(r#"FORGET sha256:{} BECAUSE "test cleanup""#, h.to_hex()),
    );
    assert_eq!(v["type"], "forgotten", "{v}");
    let audit = payload(
        &ex,
        &f,
        r#"RECALL observations WHERE namespace = "agent:authz""#,
    );
    let fields = &audit["grains"][0]["fields"];
    assert_eq!(fields["object"], "delete");
    assert_eq!(fields["context"]["because"], "test cleanup");
}

#[test]
fn user_and_scope_spellings_are_refused_with_a_pointer_to_subject() {
    let (ex, f, _dir) = setup();
    let err = ex
        .execute(r#"FORGET USER "pat" BECAUSE "x""#, &f)
        .expect_err("FORGET USER must not parse");
    assert!(err.to_string().contains("SUBJECT"), "{err}");
    assert!(ex.execute(r#"FORGET SCOPE "s" BECAUSE "x""#, &f).is_err());
}

#[test]
fn purge_sweeps_by_age_and_type_and_spares_everything_else() {
    let (ex, f, _dir) = setup();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let old = now - 40 * 86_400_000;
    f.with_store(|m| {
        let mut e_old = Event::new("session ended abruptly");
        e_old.common.namespace = Some("caller".into());
        e_old.common.created_at = Some(old);
        m.add(&e_old).unwrap();
        let mut e_new = Event::new("session completed");
        e_new.common.namespace = Some("caller".into());
        e_new.common.created_at = Some(now - 1_000);
        m.add(&e_new).unwrap();
        // Same age as the old event, wrong type — the TYPE filter must
        // spare it.
        m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(old)).unwrap();
        // Same age and type, different namespace — the IN scope must spare it.
        let mut e_other = Event::new("other-ns event");
        e_other.common.namespace = Some("shared".into());
        e_other.common.created_at = Some(old);
        m.add(&e_other).unwrap();
    });

    let v = payload(
        &ex,
        &f,
        r#"PURGE OLDER THAN 30d TYPE event IN "caller" BECAUSE "retention policy""#,
    );
    assert_eq!(v["type"], "purged", "{v}");
    assert_eq!(v["count"], 1, "{v}");

    assert_eq!(grain_count(&ex, &f, r#"RECALL events WHERE namespace = "caller" RECENT 10"#), 1);
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 1);
    assert_eq!(grain_count(&ex, &f, r#"RECALL events WHERE namespace = "shared" RECENT 10"#), 1);
}

#[test]
fn erasure_requires_the_erase_verb() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    // A principal with everything except erase, and one with erase.
    m.add(
        &Fact::new("agent:worker", REL_PERMITS, "read,write,delete ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    m.add(
        &Fact::new("job:retention", REL_PERMITS, "read,erase ON caller")
            .namespace(AUTHZ_NS)
            .created_at(2_000),
    )
    .unwrap();
    m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(3_000)).unwrap();

    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("agent:worker")
        .unwrap();
    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "not allowed""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
    // Nothing was erased.
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 1);

    let f = AreevFacade::with_session(f.into_inner(), Some("caller".to_string()), None)
        .with_principal("job:retention")
        .unwrap();
    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "authorized erasure""#);
    assert_eq!(v["type"], "forgotten", "{v}");
}

#[test]
fn text_mentions_extends_the_erasure() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
        // A grain about someone else whose text mentions the identity.
        let mut e = Event::new("call with pat about the refund");
        e.common.namespace = Some("caller".into());
        e.common.created_at = Some(2_000);
        m.add(&e).unwrap();
    });

    let v = payload(
        &ex,
        &f,
        r#"FORGET SUBJECT "pat" WITH text_mentions BECAUSE "gdpr, thorough""#,
    );
    assert_eq!(v["type"], "forgotten", "{v}");
    assert!(v["count"].as_u64().unwrap() >= 2, "text mention must be swept: {v}");
    assert_eq!(grain_count(&ex, &f, r#"RECALL events RECENT 10"#), 0);
}

// ── REPORT SUBJECT — the DSAR read (OMS 1.6 draft) ─────────────────────────

#[test]
fn report_subject_is_the_erasure_selector_in_show_me_mode() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
        m.add(&Fact::new("pat#visit1", "note", "arrived late").namespace("caller").created_at(2_000))
            .unwrap();
        m.add(&Fact::new("dr_lee", "treats", "pat").namespace("caller").created_at(3_000)).unwrap();
        m.add(&Fact::new("patricia", "prefers", "coffee").namespace("caller").created_at(4_000))
            .unwrap();
    });

    let v = payload(&ex, &f, r#"REPORT SUBJECT "pat""#);
    assert_eq!(v["type"], "subject_report", "{v}");
    let shown = v["grains"].as_array().unwrap().len();
    assert_eq!(shown, 3, "exact + partition key + object reference: {v}");
    let names: Vec<&str> =
        v["identity_names"].as_array().unwrap().iter().filter_map(|n| n.as_str()).collect();
    assert!(names.contains(&"pat#visit1"), "{names:?}");
    assert!(!names.contains(&"patricia"), "a longer word is never the identity");
    // A read writes no audit grain — the obligation is on destruction.
    assert_eq!(
        grain_count(&ex, &f, r#"RECALL observations WHERE namespace = "agent:authz""#),
        0
    );

    // THE DSAR contract: erasure removes exactly what the report showed.
    let erased = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "gdpr""#);
    assert_eq!(erased["count"].as_u64().unwrap() as usize, shown, "{erased}");
    let after = payload(&ex, &f, r#"REPORT SUBJECT "pat""#);
    assert_eq!(after["grains"].as_array().unwrap().len(), 0, "{after}");
    // And the neighbor is still reportable.
    let survivor = payload(&ex, &f, r#"REPORT SUBJECT "patricia""#);
    assert_eq!(survivor["grains"].as_array().unwrap().len(), 1, "{survivor}");
}

#[test]
fn report_subject_is_a_read_not_a_destructive_op() {
    // Classifies Read → available on the token-less read-only console.
    assert!(areev_cal::classify::query_is_read_only(r#"REPORT SUBJECT "pat""#));
    assert!(areev_cal::classify::query_is_read_only(
        r#"REPORT SUBJECT "pat" WITH text_mentions"#
    ));

    // The destructive cap must NOT gate it.
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
    });
    let ex = CalExecutor::new(CalExecutorConfig {
        allow_destructive_ops: false,
        ..CalExecutorConfig::default()
    });
    let v = serde_json::to_value(
        ex.execute(r#"REPORT SUBJECT "pat""#, &f).unwrap().payload_json().unwrap(),
    )
    .unwrap();
    assert_eq!(v["type"], "subject_report", "a read is not behind the destructive cap: {v}");
    assert_eq!(v["grains"].as_array().unwrap().len(), 1, "{v}");
}

#[test]
fn report_subject_requires_the_read_verb() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    // A principal that may erase but not read: the report must refuse —
    // DSAR access follows the read grant, not the erase grant.
    m.add(
        &Fact::new("job:eraser", REL_PERMITS, "erase ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(2_000)).unwrap();

    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("job:eraser")
        .unwrap();
    let v = payload(&ex, &f, r#"REPORT SUBJECT "pat""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
}

/// Regression: the audit record must not resurrect the identity it records
/// the erasure of. An audit grain is immutable, replicates, and lands in
/// archives — so a raw identifier there is an Art. 17 failure hiding inside
/// the Art. 30 record. The fingerprint stays *verifiable*: recomputing it
/// from a candidate identity proves a given record is about that person.
#[test]
fn erasure_audit_does_not_re_introduce_the_erased_identity() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(
            &Fact::new("wellumbrix", "prefers", "tea")
                .namespace("caller")
                .created_at(1_000),
        )
        .unwrap();
    });
    payload(&ex, &f, r#"FORGET SUBJECT "wellumbrix" BECAUSE "ticket #42""#);

    // Nothing anywhere in the file may still say the identity.
    let audit = payload(&ex, &f, r#"RECALL observations WHERE namespace = "agent:authz""#);
    assert!(
        !audit.to_string().contains("wellumbrix"),
        "erased identity survived in the audit trail: {audit}"
    );

    // But the record is verifiable: recompute the fingerprint and match.
    let fp = areev_core::authz::subject_fingerprint("wellumbrix");
    let target = audit["grains"][0]["fields"]["subject"].as_str().unwrap();
    assert_eq!(target, format!("subject:{fp} ns:caller"));
    assert_ne!(
        fp,
        areev_core::authz::subject_fingerprint("someone_else"),
        "distinct identities must not collide in the audit trail"
    );
    // And the operator's own reference survives, in the field they control.
    assert_eq!(audit["grains"][0]["fields"]["context"]["because"], "ticket #42");
}
