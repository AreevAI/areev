//! Property tests for the frozen canonical serializer (CLAUDE.md invariant 2:
//! "Canonical serialization is frozen ... changing it silently changes every
//! content address").
//!
//! For arbitrary grains the following must hold, whatever the field contents:
//!
//! 1. `serialize_grain` → `deserialize_blob` → rebuild → `serialize_grain`
//!    yields a **byte-identical** blob, and
//! 2. an **identical content-address hash**, and
//! 3. every field value that was set survives to the deserialized view.
//!
//! Each case uses a FIXED `created_at` so nothing depends on the wall clock.
//! Strings are drawn from an NFC-stable code-point pool (printable ASCII plus
//! curated precomposed Latin / CJK / Cyrillic-Greek / emoji characters, none
//! carrying combining marks) so the serializer's `nfc()` pass is a no-op and
//! field values compare equal exactly after the round-trip.

use areev_core::{
    deserialize_blob, serialize_grain, DeserializedGrain, Event, Fact, Grain, Observation,
    Reasoning, Role,
};
use proptest::prelude::*;

/// Fixed instant (epoch ms) pinning both the header's `created_at_sec` and the
/// payload `created_at`, so every blob is clock-independent.
const FIXED_CREATED_AT: i64 = 1_768_471_200_000;

// ─────────────────────────────── strategies ───────────────────────────────

/// A single NFC-stable code point. All variants are non-combining base
/// characters, so any concatenation is already in NFC form.
fn nfc_char() -> impl Strategy<Value = char> {
    prop_oneof![
        (0x20u8..=0x7e).prop_map(char::from), // printable ASCII
        prop::sample::select(vec!['é', 'ñ', 'ü', 'ç', 'ö', 'å', 'ø', 'Æ']),
        prop::sample::select(vec!['日', '本', '語', '中', '文', '한', '국']),
        prop::sample::select(vec!['П', 'р', 'и', 'в', 'е', 'т', 'α', 'β', 'γ']),
        prop::sample::select(vec!['😀', '🎉', '🚀', '❤', '✨']),
    ]
}

/// Possibly-empty NFC-stable string of up to `max` code points.
fn nfc_string(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(nfc_char(), 0..max).prop_map(|cs| cs.into_iter().collect())
}

/// Non-empty NFC-stable string of up to `max` code points.
fn nfc_string_ne(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(nfc_char(), 1..max).prop_map(|cs| cs.into_iter().collect())
}

/// Arbitrary but always-finite confidence (NaN/±Inf are excluded because the
/// deserializer rejects non-finite floats — see `msgpack_to_json_expanded`).
fn any_confidence() -> impl Strategy<Value = f64> {
    prop_oneof![
        0.0f64..=1.0,
        -1.0e9f64..1.0e9,
        prop::num::f64::NORMAL,
        prop::num::f64::SUBNORMAL,
        prop::num::f64::ZERO,
    ]
}

/// 0..=3 non-empty structural tags.
fn tag_list() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(nfc_string_ne(10), 0..4)
}

fn opt_ns() -> impl Strategy<Value = Option<String>> {
    prop::option::of(nfc_string_ne(12))
}

// ─────────────────────────────── helpers ──────────────────────────────────

/// Read an array-of-strings field back from the deserialized view (missing →
/// empty vec, matching the serializer which omits empty collections).
fn read_str_array(dg: &DeserializedGrain, key: &str) -> Vec<String> {
    dg.fields
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn role_from_idx(i: usize) -> Role {
    match i {
        0 => Role::User,
        1 => Role::Assistant,
        2 => Role::System,
        _ => Role::Tool,
    }
}

// Each rebuild reads the fields that were set back out of the deserialized
// view and reconstructs the typed grain, so a second serialization must
// reproduce the original bytes exactly.

fn rebuild_fact(dg: &DeserializedGrain) -> Fact {
    let mut f = Fact::new(
        dg.get_str("subject").unwrap_or_default(),
        dg.get_str("relation").unwrap_or_default(),
        dg.get_str("object").unwrap_or_default(),
    )
    .confidence(dg.get_f64("confidence").unwrap_or(1.0))
    .created_at(dg.get_i64("created_at").unwrap_or(FIXED_CREATED_AT))
    .tags(read_str_array(dg, "structural_tags"));
    if let Some(ns) = dg.get_str("namespace") {
        f = f.namespace(ns);
    }
    f
}

fn rebuild_event(dg: &DeserializedGrain) -> Event {
    let mut e = Event::new(dg.get_str("content").unwrap_or_default())
        .confidence(dg.get_f64("confidence").unwrap_or(1.0))
        .created_at(dg.get_i64("created_at").unwrap_or(FIXED_CREATED_AT))
        .tags(read_str_array(dg, "structural_tags"));
    if let Some(s) = dg.get_str("subject") {
        e = e.subject(s);
    }
    if let Some(r) = dg.get_str("role").and_then(Role::from_str) {
        e = e.role(r);
    }
    if let Some(ns) = dg.get_str("namespace") {
        e = e.namespace(ns);
    }
    e
}

fn rebuild_observation(dg: &DeserializedGrain) -> Observation {
    let mut o = Observation::new(
        dg.get_str("observer_id").unwrap_or_default(),
        dg.get_str("observer_type").unwrap_or_default(),
    )
    .confidence(dg.get_f64("confidence").unwrap_or(1.0))
    .created_at(dg.get_i64("created_at").unwrap_or(FIXED_CREATED_AT))
    .tags(read_str_array(dg, "structural_tags"));
    if let Some(s) = dg.get_str("subject") {
        o = o.subject(s);
    }
    if let Some(obj) = dg.get_str("object") {
        o = o.object(obj);
    }
    if let Some(ns) = dg.get_str("namespace") {
        o = o.namespace(ns);
    }
    o
}

fn rebuild_reasoning(dg: &DeserializedGrain) -> Reasoning {
    let mut r = Reasoning::new()
        .confidence(dg.get_f64("confidence").unwrap_or(1.0))
        .created_at(dg.get_i64("created_at").unwrap_or(FIXED_CREATED_AT))
        .tags(read_str_array(dg, "structural_tags"));
    r.premises = read_str_array(dg, "premises");
    if let Some(c) = dg.get_str("conclusion") {
        r.conclusion = Some(c.to_string());
    }
    if let Some(m) = dg.get_str("inference_method") {
        r.inference_method = Some(m.to_string());
    }
    if let Some(ns) = dg.get_str("namespace") {
        r = r.namespace(ns);
    }
    r
}

// ─────────────────────────────── properties ───────────────────────────────

proptest! {
    /// Fact: subject/relation/object + confidence + namespace + tags.
    #[test]
    fn fact_roundtrip_is_stable(
        subject in nfc_string(14),
        relation in nfc_string(14),
        object in nfc_string(14),
        confidence in any_confidence(),
        namespace in opt_ns(),
        tags in tag_list(),
    ) {
        let mut fact = Fact::new(&subject, &relation, &object)
            .confidence(confidence)
            .created_at(FIXED_CREATED_AT)
            .tags(tags.clone());
        if let Some(ns) = namespace.as_deref() {
            fact = fact.namespace(ns);
        }

        let (blob1, hash1) = serialize_grain(&fact).unwrap();
        let dg = deserialize_blob(&blob1).unwrap();

        // (3) every field survives to the deserialized view.
        prop_assert_eq!(dg.get_str("subject"), Some(subject.as_str()));
        prop_assert_eq!(dg.get_str("relation"), Some(relation.as_str()));
        prop_assert_eq!(dg.get_str("object"), Some(object.as_str()));
        prop_assert_eq!(dg.get_f64("confidence").map(f64::to_bits), Some(confidence.to_bits()));
        prop_assert_eq!(dg.get_i64("created_at"), Some(FIXED_CREATED_AT));
        prop_assert_eq!(dg.get_str("namespace"), namespace.as_deref());
        prop_assert_eq!(read_str_array(&dg, "structural_tags"), tags);
        // deserialize recomputes the same content address serialize returned.
        prop_assert_eq!(dg.hash, hash1);

        // (1) & (2) rebuild + re-serialize: byte-identical blob, same hash.
        let (blob2, hash2) = serialize_grain(&rebuild_fact(&dg)).unwrap();
        prop_assert_eq!(blob1, blob2);
        prop_assert_eq!(hash1, hash2);
    }

    /// Event: content + optional subject/role + confidence + namespace + tags.
    #[test]
    fn event_roundtrip_is_stable(
        content in nfc_string(20),
        subject in prop::option::of(nfc_string_ne(12)),
        role_idx in prop::option::of(0usize..4),
        confidence in any_confidence(),
        namespace in opt_ns(),
        tags in tag_list(),
    ) {
        let role = role_idx.map(role_from_idx);
        let mut ev = Event::new(&content)
            .confidence(confidence)
            .created_at(FIXED_CREATED_AT)
            .tags(tags.clone());
        if let Some(s) = subject.as_deref() {
            ev = ev.subject(s);
        }
        if let Some(r) = role {
            ev = ev.role(r);
        }
        if let Some(ns) = namespace.as_deref() {
            ev = ev.namespace(ns);
        }

        let (blob1, hash1) = serialize_grain(&ev).unwrap();
        let dg = deserialize_blob(&blob1).unwrap();

        prop_assert_eq!(dg.get_str("content"), Some(content.as_str()));
        prop_assert_eq!(dg.get_str("subject"), subject.as_deref());
        prop_assert_eq!(dg.get_str("role"), role.map(|r| r.as_str()));
        prop_assert_eq!(dg.get_f64("confidence").map(f64::to_bits), Some(confidence.to_bits()));
        prop_assert_eq!(read_str_array(&dg, "structural_tags"), tags);
        prop_assert_eq!(dg.hash, hash1);

        let (blob2, hash2) = serialize_grain(&rebuild_event(&dg)).unwrap();
        prop_assert_eq!(blob1, blob2);
        prop_assert_eq!(hash1, hash2);
    }

    /// Observation: observer_id/type + optional subject/object + common.
    #[test]
    fn observation_roundtrip_is_stable(
        observer_id in nfc_string(14),
        observer_type in nfc_string(14),
        subject in prop::option::of(nfc_string_ne(12)),
        object in prop::option::of(nfc_string_ne(12)),
        confidence in any_confidence(),
        namespace in opt_ns(),
        tags in tag_list(),
    ) {
        let mut obs = Observation::new(&observer_id, &observer_type)
            .confidence(confidence)
            .created_at(FIXED_CREATED_AT)
            .tags(tags.clone());
        if let Some(s) = subject.as_deref() {
            obs = obs.subject(s);
        }
        if let Some(o) = object.as_deref() {
            obs = obs.object(o);
        }
        if let Some(ns) = namespace.as_deref() {
            obs = obs.namespace(ns);
        }

        let (blob1, hash1) = serialize_grain(&obs).unwrap();
        let dg = deserialize_blob(&blob1).unwrap();

        prop_assert_eq!(dg.get_str("observer_id"), Some(observer_id.as_str()));
        prop_assert_eq!(dg.get_str("observer_type"), Some(observer_type.as_str()));
        prop_assert_eq!(dg.get_str("subject"), subject.as_deref());
        prop_assert_eq!(dg.get_str("object"), object.as_deref());
        prop_assert_eq!(dg.get_f64("confidence").map(f64::to_bits), Some(confidence.to_bits()));
        prop_assert_eq!(read_str_array(&dg, "structural_tags"), tags);
        prop_assert_eq!(dg.hash, hash1);

        let (blob2, hash2) = serialize_grain(&rebuild_observation(&dg)).unwrap();
        prop_assert_eq!(blob1, blob2);
        prop_assert_eq!(hash1, hash2);
    }

    /// Reasoning: premises + optional conclusion/inference_method + common.
    #[test]
    fn reasoning_roundtrip_is_stable(
        premises in prop::collection::vec(nfc_string_ne(14), 0..4),
        conclusion in prop::option::of(nfc_string_ne(16)),
        inference_method in prop::option::of(nfc_string_ne(12)),
        confidence in any_confidence(),
        namespace in opt_ns(),
        tags in tag_list(),
    ) {
        let mut r = Reasoning::new()
            .confidence(confidence)
            .created_at(FIXED_CREATED_AT)
            .tags(tags.clone());
        r.premises = premises.clone();
        if let Some(c) = conclusion.as_deref() {
            r = r.conclusion(c);
        }
        if let Some(m) = inference_method.as_deref() {
            r = r.inference_method(m);
        }
        if let Some(ns) = namespace.as_deref() {
            r = r.namespace(ns);
        }

        let (blob1, hash1) = serialize_grain(&r).unwrap();
        let dg = deserialize_blob(&blob1).unwrap();

        prop_assert_eq!(read_str_array(&dg, "premises"), premises);
        prop_assert_eq!(dg.get_str("conclusion"), conclusion.as_deref());
        prop_assert_eq!(dg.get_str("inference_method"), inference_method.as_deref());
        prop_assert_eq!(dg.get_f64("confidence").map(f64::to_bits), Some(confidence.to_bits()));
        prop_assert_eq!(read_str_array(&dg, "structural_tags"), tags);
        prop_assert_eq!(dg.hash, hash1);

        let (blob2, hash2) = serialize_grain(&rebuild_reasoning(&dg)).unwrap();
        prop_assert_eq!(blob1, blob2);
        prop_assert_eq!(hash1, hash2);
    }
}
