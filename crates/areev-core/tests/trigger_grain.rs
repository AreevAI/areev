//! The Trigger grain (0x0D): full-field round trip, the omit-default rules, and
//! the coherence check that refuses a declaration which could never fire.

use areev_core::format::{deserialize_blob, serialize_grain};
use areev_core::types::{Catchup, Concurrency, Grain, GrainType, Trigger, TriggerKind};

const WF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

fn roundtrip(t: &Trigger) -> Trigger {
    let (blob, _) = serialize_grain(t).expect("serialize");
    deserialize_blob(&blob).expect("deserialize").to_trigger().expect("to_trigger")
}

#[test]
fn every_field_survives_a_round_trip() {
    let original = Trigger::new(TriggerKind::Composite, WF)
        .connector("gmail")
        .scope("mailbox:accounts@example.com")
        .enabled(false)
        .dedup_key("/id")
        .dedup_key("/updated_at")
        .interval_secs(120)
        .cron("0 9 * * 1-5")
        .at_ms(1_755_600_000_000)
        .predicate(serde_json::json!({
            "kind": "or",
            "left": { "kind": "comparison", "field": "a", "comparator": "eq" },
            "right": { "kind": "comparison", "field": "b", "comparator": "eq" }
        }))
        .member("aa")
        .member("bb")
        .correlate("/thread_id")
        .window_ms(600_000)
        .concurrency(Concurrency::Replace)
        .catchup(Catchup::All)
        .config(serde_json::json!({
            "int:cursor_field": "since",
            "int:cursor_type": "timestamp",
            "connector_specific": { "label": "INBOX" }
        }))
        .created_at(1_700_000_000_000)
        .namespace("ops");

    let back = roundtrip(&original);

    assert_eq!(back.kind, TriggerKind::Composite);
    assert_eq!(back.workflow, WF);
    assert_eq!(back.connector.as_deref(), Some("gmail"));
    assert_eq!(back.scope.as_deref(), Some("mailbox:accounts@example.com"));
    assert!(!back.enabled);
    assert_eq!(back.dedup_key, vec!["/id".to_string(), "/updated_at".to_string()]);
    assert_eq!(back.interval_secs, Some(120));
    assert_eq!(back.cron.as_deref(), Some("0 9 * * 1-5"));
    assert_eq!(back.at_ms, Some(1_755_600_000_000));
    assert_eq!(back.predicate, original.predicate);
    assert_eq!(back.members, vec!["aa".to_string(), "bb".to_string()]);
    assert_eq!(back.correlate.as_deref(), Some("/thread_id"));
    assert_eq!(back.window_ms, Some(600_000));
    assert_eq!(back.concurrency, Concurrency::Replace);
    assert_eq!(back.catchup, Catchup::All);
    assert_eq!(back.config, original.config);
}

#[test]
fn connector_config_keys_round_trip_verbatim() {
    // `int:` keys are compacted on the wire and expanded on read; a
    // connector's own nested keys must survive untouched, including one that
    // collides with an OMS short code. `"o"` expanding to `"object"` is the
    // documented failure this guards.
    let t = Trigger::new(TriggerKind::Polling, WF)
        .connector("gmail")
        .interval_secs(60)
        .config(serde_json::json!({
            "int:cursor_field": "since",
            "vendor": { "o": "not-object", "s": "not-subject" }
        }));
    let back = roundtrip(&t);
    let cfg = back.config.expect("config");
    assert_eq!(cfg["int:cursor_field"], "since");
    assert_eq!(cfg["vendor"]["o"], "not-object");
    assert_eq!(cfg["vendor"]["s"], "not-subject");
}

#[test]
fn defaults_are_omitted_and_read_back_as_defaults() {
    let minimal = Trigger::new(TriggerKind::Interval, WF).interval_secs(300);
    let (blob, _) = serialize_grain(&minimal).unwrap();
    let raw = deserialize_blob(&blob).unwrap();

    // Omit-when-default keeps the wire small and legacy blobs stable.
    assert!(!raw.fields.contains_key("enabled"), "a live trigger should not carry the flag");
    assert!(!raw.fields.contains_key("concurrency"));
    assert!(!raw.fields.contains_key("catchup"));

    let back = roundtrip(&minimal);
    assert!(back.enabled, "absent enabled must read back as true, not false");
    assert_eq!(back.concurrency, Concurrency::Forbid);
    assert_eq!(back.catchup, Catchup::Last);
}

#[test]
fn identical_declarations_share_a_content_address() {
    let a = Trigger::new(TriggerKind::Schedule, WF)
        .cron("0 9 * * 1-5")
        .created_at(1_700_000_000_000)
        .namespace("ops");
    let b = Trigger::new(TriggerKind::Schedule, WF)
        .cron("0 9 * * 1-5")
        .created_at(1_700_000_000_000)
        .namespace("ops");
    assert_eq!(serialize_grain(&a).unwrap().1, serialize_grain(&b).unwrap().1);
}

#[test]
fn unknown_kind_on_the_wire_does_not_fail_the_read() {
    // Forward compatibility: a trigger kind this build does not know must not
    // make the grain unreadable, the way every other enum here behaves.
    let t = Trigger::new(TriggerKind::Interval, WF).interval_secs(60);
    let (mut blob, _) = serialize_grain(&t).unwrap();
    // Rewrite the kind value in place: "interval" and "sideways" are both 8
    // bytes, so the msgpack framing is unchanged.
    let needle = b"interval";
    let pos = blob.windows(needle.len()).position(|w| w == needle).expect("kind on the wire");
    blob[pos..pos + needle.len()].copy_from_slice(b"sideways");

    let back = deserialize_blob(&blob).unwrap().to_trigger().expect("still readable");
    assert_eq!(back.kind, TriggerKind::Interval, "unknown kind falls back to the default");
}

#[test]
fn text_is_what_a_person_would_search_for() {
    let t = Trigger::new(TriggerKind::Polling, WF)
        .connector("gmail")
        .scope("mailbox:accounts@example.com")
        .interval_secs(120);
    let text = t.text();
    assert!(text.contains("polling"));
    assert!(text.contains("gmail"));
    assert!(text.contains("accounts@example.com"));
}

// ---- coherence: refuse a declaration that could never fire ----------------

#[test]
fn a_coherent_declaration_passes() {
    assert!(Trigger::new(TriggerKind::Interval, WF).interval_secs(60).incoherence().is_none());
    assert!(Trigger::new(TriggerKind::Schedule, WF).cron("0 * * * *").incoherence().is_none());
    assert!(Trigger::new(TriggerKind::Once, WF).at_ms(1).incoherence().is_none());
    assert!(Trigger::new(TriggerKind::Manual, WF).incoherence().is_none());
    assert!(Trigger::new(TriggerKind::Webhook, WF).incoherence().is_none());
}

#[test]
fn a_trigger_that_could_never_fire_is_refused() {
    // Each of these is the failure nobody notices, because its symptom is
    // nothing happening.
    let cases: Vec<(Trigger, &str)> = vec![
        (Trigger::new(TriggerKind::Interval, WF), "interval"),
        (Trigger::new(TriggerKind::Interval, WF).interval_secs(0), "interval"),
        (Trigger::new(TriggerKind::Schedule, WF), "cron"),
        (Trigger::new(TriggerKind::Once, WF), "at_ms"),
        (Trigger::new(TriggerKind::Memory, WF), "predicate"),
        (Trigger::new(TriggerKind::Polling, WF).interval_secs(60), "connector"),
        (Trigger::new(TriggerKind::Composite, WF).predicate(serde_json::json!({})), "members"),
        (Trigger::new(TriggerKind::Composite, WF).member("a").member("b"), "predicate"),
        (Trigger::new(TriggerKind::Interval, "").interval_secs(60), "workflow"),
    ];
    for (t, expected) in cases {
        let why = t.incoherence().unwrap_or_else(|| {
            panic!("expected {:?} to be refused for missing {expected}", t.kind)
        });
        assert!(why.contains(expected), "message should name the missing piece: {why}");
    }
}

#[test]
fn the_type_byte_is_0x0d_and_appended_last() {
    assert_eq!(GrainType::Trigger.type_byte(), 0x0D);
    assert_eq!(GrainType::from_byte(0x0D), Some(GrainType::Trigger));
    // The store indexes `gtype` as the enum ordinal, so Trigger must sort after
    // every pre-existing variant or stored rows silently renumber.
    assert!(GrainType::Trigger as usize > GrainType::Recommendation as usize);
}
