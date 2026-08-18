//! Golden detector corpus + round-trip property test for `areev_core::anon`
//! (docs/anonymization-proposal.md P0 gates).
//!
//! Corpus style: every category gets positives AND near-miss negatives — the
//! validators (Luhn, mod-97, address parse, cue words) are the point, so the
//! negatives are as load-bearing as the positives.

use std::collections::BTreeMap;

use areev_core::anon::{anonymize, mapping_from_json, rehydrate, scan, AnonPolicy, KnownIdentity};
use proptest::prelude::*;

fn spans(text: &str) -> Vec<(String, String)> {
    let out = scan(text, &AnonPolicy::default(), &[]).unwrap();
    out.detections
        .iter()
        .map(|d| (d.category.clone(), out.text[d.start..d.end].to_string()))
        .collect()
}

fn assert_detects(text: &str, category: &str, value: &str) {
    let found = spans(text);
    assert!(
        found.iter().any(|(c, v)| c == category && v == value),
        "expected {category}={value:?} in {text:?}, got {found:?}"
    );
}

fn assert_clean(text: &str) {
    let found = spans(text);
    assert!(found.is_empty(), "expected no detections in {text:?}, got {found:?}");
}

#[test]
fn golden_email() {
    assert_detects("reach me at sathish.k+test@sub.example.co.uk ok", "email", "sathish.k+test@sub.example.co.uk");
    assert_clean("not-an@email without a tld");
}

#[test]
fn golden_phone() {
    assert_detects("call +1 415 555 0142 today", "phone", "+1 415 555 0142");
    assert_detects("call (415) 555-0142 today", "phone", "(415) 555-0142");
    assert_detects("call 415-555-0142 today", "phone", "415-555-0142");
    assert_clean("in 2026 there were 1462 cases");
}

#[test]
fn golden_ip_and_mac() {
    assert_detects("host 10.0.0.1 down", "ipv4", "10.0.0.1");
    assert_clean("version 999.1.1.1 shipped");
    assert_detects("node at fe80::1 responded", "ipv6", "fe80::1");
    assert_detects("via 2001:db8:0:0:0:0:2:1 route", "ipv6", "2001:db8:0:0:0:0:2:1");
    assert_clean("use areev_core::anon::scan in code");
    assert_detects("nic 00:1A:2B:3C:4D:5E up", "mac", "00:1A:2B:3C:4D:5E");
}

#[test]
fn golden_url_userinfo_and_date() {
    assert_detects(
        "pull from https://svc:t0ken@git.example/repo.git now",
        "url_userinfo",
        "https://svc:t0ken@git.example/repo.git",
    );
    assert_detects("met on 2026-08-16 at noon", "date", "2026-08-16");
    assert_detects("met on 16/08/2026 at noon", "date", "16/08/2026");
    assert_clean("code 2026-13-45 is not a date");
}

#[test]
fn golden_checksummed_ids() {
    // Valid Luhn; flipping the last digit must kill the detection.
    assert_detects("card 4539 1488 0343 6467 on file", "credit_card", "4539 1488 0343 6467");
    assert_clean("card 4539 1488 0343 6468 on file");
    assert_detects("iban GB82WEST12345698765432 given", "iban", "GB82WEST12345698765432");
    assert_clean("iban GB82WEST12345698765433 given");
}

#[test]
fn golden_secrets() {
    assert_detects("key sk-abc123DEF456ghi789JKL0 leaked", "secret", "sk-abc123DEF456ghi789JKL0");
    assert_detects("aws AKIAIOSFODNN7EXAMPLE used", "secret", "AKIAIOSFODNN7EXAMPLE");
    // A 64-lowercase-hex run is a grain hash silhouette, not a credential.
    assert_clean(
        "grain 3288d0d49f2e1a6b7c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b stored",
    );
}

#[test]
fn golden_keyword_proximity() {
    assert_detects("my user name is john, and pin number is 1462", "pin", "1462");
    assert_detects("the OTP is 774213 for login", "otp", "774213");
    assert_detects("password: hunter2! rotate it", "password", "hunter2");
    assert_detects("account number is 9944-221100 at the bank", "account_number", "9944-221100");
    assert_clean("spinning 12345 times is not a pin");
    assert_clean("the account is open");
}

#[test]
fn golden_dictionary_and_known_identities() {
    let policy =
        AnonPolicy { custom_terms: vec!["Project Nightingale".into()], ..Default::default() };
    let out = scan("ship project nightingale next week", &policy, &[]).unwrap();
    assert_eq!(out.detections.len(), 1);
    assert_eq!(out.detections[0].category, "custom");

    let out = scan(
        "my user name is john, and johnson is someone else; caller:john agrees",
        &AnonPolicy::default(),
        &["caller:john".into()],
    )
    .unwrap();
    let found: Vec<&str> =
        out.detections.iter().map(|d| &out.text[d.start..d.end]).collect();
    assert!(found.contains(&"john"), "tail propagation missed: {found:?}");
    assert!(found.contains(&"caller:john"), "full identity missed: {found:?}");
    assert!(!found.contains(&"johnson"), "boundary violated: {found:?}");
}

#[test]
fn policy_known_identities_match_each_with_its_own_category() {
    // GitHub issue #32's escape hatch: a caller-supplied `known` entry
    // detects with the category IT names, not tier0's fixed "person" — a
    // project codename is `custom`, not a person.
    let policy = AnonPolicy {
        known: vec![
            KnownIdentity { value: "Kenneth Shea".into(), category: "person".into() },
            KnownIdentity { value: "Project Falcon".into(), category: "custom".into() },
            KnownIdentity { value: "ab".into(), category: "custom".into() }, // too short, dropped
        ],
        ..Default::default()
    };
    let out = scan("Kenneth Shea sent the Project Falcon NDA.", &policy, &[]).unwrap();
    let found: BTreeMap<String, String> = out
        .detections
        .iter()
        .map(|d| (out.text[d.start..d.end].to_string(), d.category.clone()))
        .collect();
    assert_eq!(found.get("Kenneth Shea"), Some(&"person".to_string()));
    assert_eq!(found.get("Project Falcon"), Some(&"custom".to_string()));
    assert_eq!(found.len(), 2, "the sub-3-char entry must not match: {found:?}");
}

#[test]
fn overlap_severity_redact_beats_pseudonym() {
    // A long pseudonym-actioned custom span must not swallow the card into
    // the reversible mapping (proposal §5): redact wins on severity.
    let mut policy = AnonPolicy {
        custom_terms: vec!["card 4539148803436467 here".into()],
        ..Default::default()
    };
    policy.categories.insert("credit_card".into(), "redact".into());
    let out = anonymize("card 4539148803436467 here we go", &policy, &[], None).unwrap();
    assert!(out.text.contains("[REDACTED:CREDIT_CARD]"), "got: {}", out.text);
    assert!(
        !out.mapping.values().any(|v| v.contains("4539148803436467")),
        "card leaked into the reversible mapping: {:?}",
        out.mapping
    );
}

#[test]
fn default_action_fails_closed_and_allow_passes() {
    let policy = AnonPolicy { default_action: "redact".into(), ..Default::default() };
    let out = anonymize("mail a@b.co now", &policy, &[], None).unwrap();
    assert!(out.text.contains("[REDACTED:EMAIL]"));
    assert!(out.mapping.is_empty());

    let mut policy = AnonPolicy::default();
    policy.categories.insert("email".into(), "allow".into());
    let out = anonymize("mail a@b.co now", &policy, &[], None).unwrap();
    assert_eq!(out.text, "mail a@b.co now");
    assert_eq!(out.replaced, 0);
}

#[test]
fn worked_example_round_trip() {
    // The proposal §6 worked example, end to end.
    let text = "my user name is john, and pin number is 1462";
    let out = anonymize(text, &AnonPolicy::default(), &["caller:john".into()], None).unwrap();
    assert_eq!(out.text, "my user name is [PERSON_1], and pin number is [PIN_1]");
    assert_eq!(out.mapping["[PERSON_1]"], "john");
    assert_eq!(out.mapping["[PIN_1]"], "1462");

    let response = "Hi [PERSON_1], I've verified pin [PIN_1].";
    let back = rehydrate(response, &out.mapping).unwrap();
    assert_eq!(back.text, "Hi john, I've verified pin 1462.");
    assert_eq!(back.replaced, 2);
    assert!(back.unmatched.is_empty());
}

#[test]
fn rehydrate_reports_unmatched_and_never_guesses() {
    let mapping: BTreeMap<String, String> =
        mapping_from_json(r#"{"[PERSON_1]": "john"}"#).unwrap();
    let back = rehydrate("Hi [PERSON_1], code [PIN_9] pending", &mapping).unwrap();
    assert_eq!(back.text, "Hi john, code [PIN_9] pending");
    assert_eq!(back.unmatched, vec!["[PIN_9]".to_string()]);
}

// ---- property: the round trip is lossless for pseudonym-only policies -----

fn pii_segment() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{1,8}",                                  // plain word
        Just("a@b.co".to_string()),                    // email
        Just("+1 415 555 0142".to_string()),           // phone
        Just("pin is 1462".to_string()),               // keyword-proximity
        Just("4539 1488 0343 6467".to_string()),       // credit card (Luhn-valid)
        Just("10.0.0.1".to_string()),                  // ipv4
        Just("2026-08-16".to_string()),                // date
        Just("[EMAIL_1]".to_string()),                 // literal token collision
        "\\PC{0,12}",                                  // arbitrary printable unicode
    ]
}

proptest! {
    #[test]
    fn prop_rehydrate_inverts_anonymize(segments in prop::collection::vec(pii_segment(), 0..12)) {
        let text = segments.join(" ");
        let policy = AnonPolicy::default(); // every action defaults to pseudonym
        let normalized = areev_core::anon::nfc(&text).into_owned();
        let out = anonymize(&text, &policy, &["caller:john".into()], Some(b"prop-key")).unwrap();
        let back = rehydrate(&out.text, &out.mapping).unwrap();
        prop_assert_eq!(&back.text, &normalized);

        // Determinism: same inputs + key → same tokens and same id (D11).
        let again = anonymize(&text, &policy, &["caller:john".into()], Some(b"prop-key")).unwrap();
        prop_assert_eq!(out.text, again.text);
        prop_assert_eq!(out.mapping_id, again.mapping_id);
    }
}
