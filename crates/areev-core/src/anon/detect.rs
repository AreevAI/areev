//! Tier-0 detectors (proposal §5.1): structural known-identity propagation,
//! regex + checksum validators, secrets, keyword-proximity cues, and the user
//! dictionary. Deterministic, dependency-free beyond what the crate already
//! ships, and precision-first: a candidate that fails its validator (Luhn,
//! mod-97, address parse) is not a detection. Tiers 1–2 buy recall.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::OnceLock;

use regex::Regex;

use super::Detection;
use crate::error::Result;

/// Categories the built-in tier can emit (the policy vocabulary is open —
/// Tier-1/2 detectors may emit anything; unmapped categories take the
/// policy's `default_action`).
pub const KNOWN_CATEGORIES: &[&str] = &[
    "person",
    "email",
    "phone",
    "ipv4",
    "ipv6",
    "mac",
    "url_userinfo",
    "date",
    "credit_card",
    "iban",
    "secret",
    "pin",
    "password",
    "otp",
    "account_number",
    "custom",
    // National / health identifiers (issue #47). Every one of these is
    // checksum-validated or cue-gated — see the individual detectors.
    "sg_nric",
    "ae_eid",
    "mrn",
    // US identifiers (issue #281). Structure-validated (SSN, ITIN) or
    // checksum-plus-cue (ABA routing) — see the individual detectors.
    "us_ssn",
    "us_itin",
    "aba_routing",
    // Indian tax identifiers (issue #347). Checksum-validated (GSTIN,
    // mod-36) or structure-plus-cue (PAN) — see the individual detectors.
    "in_gstin",
    "in_pan",
    // Indian personal identifiers (issue #418). Checksum-validated (Aadhaar,
    // Verhoeff), first-digit-shaped (mobile), or cue/state-code-gated (EPIC,
    // vehicle registration) — see the individual detectors.
    "in_mobile",
    "in_aadhaar",
    "in_epic",
    "in_vehicle",
];

/// Categories whose detections are backed by a checksum or a structural
/// validator, as opposed to shape alone. Used as an overlap tiebreak (#281):
/// a dashed SSN also matches the phone pattern, and the specific,
/// validator-backed reading must win over the generic one.
pub fn category_is_validated(category: &str) -> bool {
    matches!(
        category,
        "email"
            | "ipv4"
            | "ipv6"
            | "mac"
            | "url_userinfo"
            | "credit_card"
            | "iban"
            | "sg_nric"
            | "ae_eid"
            | "us_ssn"
            | "us_itin"
            | "aba_routing"
            | "in_gstin"
            // Structure (holder-type letter) plus a cue (#347). Without it the
            // `Permanent Account Number` cue also fires the generic `account`
            // keyword rule on the same span, and `account_number` would win
            // the alphabetical fallback — the PAN cue would never yield a PAN.
            | "in_pan"
            // Verhoeff over all twelve (or sixteen) digits (#418). Marked so a
            // dash-grouped Aadhaar, which the generic dashed-phone shape also
            // matches, is reported as what it is.
            | "in_aadhaar"
            // Structure (three letters, seven digits) plus a cue (#418).
            | "in_epic"
    )
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static detector regex"))
}

fn det(start: usize, end: usize, category: &str, detector: &str) -> Detection {
    Detection {
        start,
        end,
        category: category.to_string(),
        confidence: 1.0,
        detector: detector.to_string(),
    }
}

/// Character-class word boundary for hand-checked matches: the position is a
/// boundary when the adjacent character is absent or not alphanumeric.
fn boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
}

pub(super) fn run_tier0(
    text: &str,
    custom_terms: &[String],
    known_identities: &[String],
    term_sets: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<Vec<Detection>> {
    let mut out = Vec::new();
    detect_email(text, &mut out);
    detect_phone(text, &mut out);
    detect_ipv4(text, &mut out);
    detect_ipv6(text, &mut out);
    detect_mac(text, &mut out);
    detect_url_userinfo(text, &mut out);
    detect_date(text, &mut out);
    detect_credit_card(text, &mut out);
    detect_iban(text, &mut out);
    detect_sg_nric(text, &mut out);
    detect_ae_eid(text, &mut out);
    detect_us_ssn(text, &mut out);
    detect_us_itin(text, &mut out);
    detect_aba_routing(text, &mut out);
    detect_in_gstin(text, &mut out);
    detect_in_pan(text, &mut out);
    detect_in_mobile(text, &mut out);
    detect_in_aadhaar(text, &mut out);
    detect_in_epic(text, &mut out);
    detect_in_vehicle(text, &mut out);
    detect_mrn(text, &mut out);
    detect_secret(text, &mut out);
    detect_keyword_proximity(text, &mut out);
    detect_terms(text, custom_terms, "custom", "tier0.dictionary", &mut out)?;
    // Named dictionaries: each set emits its OWN category, which is what makes
    // a co-occurrence rule expressible ("person near condition"). One shared
    // `custom` bucket could not distinguish the two halves of the rule.
    for (category, terms) in term_sets {
        detect_terms(text, terms, category, "tier0.term_set", &mut out)?;
    }
    let identity_terms = identity_match_terms(known_identities);
    detect_terms(text, &identity_terms, "person", "tier0.known_identity", &mut out)?;
    Ok(out)
}

fn detect_email(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}");
    for m in re.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) {
            out.push(det(m.start(), m.end(), "email", "tier0.email"));
        }
    }
}

/// Phones: international `+` numbers, and separator-formatted national
/// numbers. Bare digit runs and space-separated digit groups deliberately do
/// NOT match (precision over recall — "in 2026 there were 1462 cases" is not
/// a phone number).
fn detect_phone(text: &str, out: &mut Vec<Detection>) {
    static INTL: OnceLock<Regex> = OnceLock::new();
    static NANP: OnceLock<Regex> = OnceLock::new();
    static DASHED: OnceLock<Regex> = OnceLock::new();
    static ISO_SHAPE: OnceLock<Regex> = OnceLock::new();
    static DMY_SHAPE: OnceLock<Regex> = OnceLock::new();
    // No backreferences in the regex crate, so the dash-grouped form is its
    // own pattern; ISO-date-shaped and day-first dashed-date candidates
    // (`12-05-2024`, the common Indian form, #418) are excluded — the date
    // detector owns those.
    let iso_shape = re(&ISO_SHAPE, r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$");
    let dmy_shape = re(&DMY_SHAPE, r"^[0-9]{1,2}-[0-9]{1,2}-[0-9]{4}$");
    let candidates = [
        re(&INTL, r"\+[1-9][0-9 ().\-]{5,18}[0-9]"),
        re(&NANP, r"\(?[0-9]{3}\)?[ .\-][0-9]{3}[ .\-][0-9]{4}"),
        re(&DASHED, r"[0-9]{2,4}-[0-9]{2,4}-[0-9]{2,8}"),
    ];
    for r in candidates {
        for m in r.find_iter(text) {
            if iso_shape.is_match(m.as_str())
                || (dmy_shape.is_match(m.as_str()) && plausible_dmy(m.as_str(), '-'))
            {
                continue;
            }
            let digits = m.as_str().chars().filter(char::is_ascii_digit).count();
            if (7..=15).contains(&digits) && boundary_ok(text, m.start(), m.end()) {
                out.push(det(m.start(), m.end(), "phone", "tier0.phone"));
            }
        }
    }
}

fn detect_ipv4(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}");
    for m in re.find_iter(text) {
        if boundary_ok(text, m.start(), m.end())
            && !text[m.end()..].starts_with('.')
            && Ipv4Addr::from_str(m.as_str()).is_ok()
        {
            out.push(det(m.start(), m.end(), "ipv4", "tier0.ipv4"));
        }
    }
}

/// IPv6 candidates are hex-and-colon runs validated by the std parser. The
/// candidate must contain a digit and either "::" or ≥4 colons — all-letter
/// addresses like `face::cafe` are a deliberate false negative, because
/// otherwise every Rust path segment pair (`core::anon`) becomes a candidate.
fn detect_ipv6(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[0-9A-Fa-f:]{4,45}");
    for m in re.find_iter(text) {
        let s = m.as_str();
        let colons = s.matches(':').count();
        if !s.chars().any(|c| c.is_ascii_digit()) || (colons < 4 && !s.contains("::")) {
            continue;
        }
        if boundary_ok(text, m.start(), m.end()) && Ipv6Addr::from_str(s).is_ok() {
            out.push(det(m.start(), m.end(), "ipv6", "tier0.ipv6"));
        }
    }
}

fn detect_mac(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"([0-9A-Fa-f]{2}[:\-]){5}[0-9A-Fa-f]{2}");
    for m in re.find_iter(text) {
        // A MAC candidate inside a longer hex-colon run is an IPv6 fragment.
        if boundary_ok(text, m.start(), m.end())
            && !text[..m.start()].ends_with(':')
            && !text[m.end()..].starts_with(':')
        {
            out.push(det(m.start(), m.end(), "mac", "tier0.mac"));
        }
    }
}

/// URLs carrying credentials in the authority (`scheme://user:pass@host/…`).
/// The whole URL is the span — the userinfo is meaningless to protect while
/// the rest of the URL pins where it works.
fn detect_url_userinfo(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[A-Za-z][A-Za-z0-9+.\-]*://[^/\s@]+@[^\s]+");
    for m in re.find_iter(text) {
        out.push(det(m.start(), m.end(), "url_userinfo", "tier0.url_userinfo"));
    }
}

fn detect_date(text: &str, out: &mut Vec<Detection>) {
    static ISO: OnceLock<Regex> = OnceLock::new();
    static SLASH: OnceLock<Regex> = OnceLock::new();
    let iso = re(&ISO, r"[0-9]{4}-[0-9]{2}-[0-9]{2}");
    for m in iso.find_iter(text) {
        let parts: Vec<u32> = m.as_str().split('-').map(|p| p.parse().unwrap_or(0)).collect();
        if boundary_ok(text, m.start(), m.end())
            && (1..=12).contains(&parts[1])
            && (1..=31).contains(&parts[2])
        {
            out.push(det(m.start(), m.end(), "date", "tier0.date"));
        }
    }
    let slash = re(&SLASH, r"[0-9]{1,2}/[0-9]{1,2}/[0-9]{2,4}");
    for m in slash.find_iter(text) {
        if plausible_dmy(m.as_str(), '/') && boundary_ok(text, m.start(), m.end()) {
            out.push(det(m.start(), m.end(), "date", "tier0.date"));
        }
    }
    // Day-first dashed and dotted dates with a four-digit year (#418):
    // `12-05-2024`, `12.05.2024`. Until this, the dashed form was reported
    // as a `phone` by the dash-grouped phone shape.
    static DASHED: OnceLock<Regex> = OnceLock::new();
    static DOTTED: OnceLock<Regex> = OnceLock::new();
    let shapes = [
        (re(&DASHED, r"[0-9]{1,2}-[0-9]{1,2}-[0-9]{4}"), '-'),
        (re(&DOTTED, r"[0-9]{1,2}\.[0-9]{1,2}\.[0-9]{4}"), '.'),
    ];
    for (shape, sep) in shapes {
        for m in shape.find_iter(text) {
            // A dotted run continued by another `.digit` is an IP address or
            // a version string, not a date.
            let continues = text[m.end()..].starts_with(sep)
                || text[..m.start()].ends_with(sep);
            if !continues
                && plausible_dmy(m.as_str(), sep)
                && boundary_ok(text, m.start(), m.end())
            {
                out.push(det(m.start(), m.end(), "date", "tier0.date"));
            }
        }
    }
}

/// `D<sep>M<sep>Y` (or `M<sep>D<sep>Y`): both leading parts in 1..=31 and at
/// least one of them a plausible month.
fn plausible_dmy(s: &str, sep: char) -> bool {
    let parts: Vec<u32> = s.split(sep).map(|p| p.parse().unwrap_or(0)).collect();
    if parts.len() != 3 {
        return false;
    }
    let plausible_dm = (1..=12).contains(&parts[0]) || (1..=12).contains(&parts[1]);
    (1..=31).contains(&parts[0]) && (1..=31).contains(&parts[1]) && plausible_dm
}

fn luhn_ok(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(*d);
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// Card numbers: 13–19 digits with optional single space/dash separators,
/// gated on the Luhn checksum — a digit run that fails Luhn is not a card.
fn detect_credit_card(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[0-9](?:[ \-]?[0-9]){12,18}");
    for m in re.find_iter(text) {
        let digits: Vec<u8> =
            m.as_str().chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect();
        if (13..=19).contains(&digits.len())
            && luhn_ok(&digits)
            && boundary_ok(text, m.start(), m.end())
        {
            out.push(det(m.start(), m.end(), "credit_card", "tier0.credit_card"));
        }
    }
}

fn iban_mod97_ok(s: &str) -> bool {
    // Move the first four chars to the end, map A→10…Z→35, take mod 97 == 1.
    let rearranged: String = format!("{}{}", &s[4..], &s[..4]);
    let mut rem: u32 = 0;
    for c in rearranged.chars() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'A'..='Z' => c as u32 - 'A' as u32 + 10,
            'a'..='z' => c as u32 - 'a' as u32 + 10,
            _ => return false,
        };
        rem = if v < 10 { (rem * 10 + v) % 97 } else { (rem * 100 + v) % 97 };
    }
    rem == 1
}

fn detect_iban(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[A-Z]{2}[0-9]{2}[A-Za-z0-9]{11,30}");
    for m in re.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && iban_mod97_ok(m.as_str()) {
            out.push(det(m.start(), m.end(), "iban", "tier0.iban"));
        }
    }
}


// ---------------------------------------------------------------------------
// National / health identifiers (issue #47)
// ---------------------------------------------------------------------------
//
// These are the identifiers a health regulator actually asks about, and the
// reason they belong in a precision-first tier is that the important ones
// carry checksums: an NRIC-shaped string that fails its check digit is not an
// NRIC, so the false-positive rate is near zero rather than "one letter
// followed by seven digits". Where a family has no checksum (MRN), the
// detector is gated on a nearby cue word instead of matching bare digit runs.

/// Singapore NRIC / FIN — `[STFGM]` + 7 digits + a check letter.
///
/// Weighted mod-11 over the seven digits (weights 2,7,6,5,4,3,2), an offset
/// that encodes the issue era (T/G +4, M +3), and a prefix-class letter table.
/// Wrong check letter → not a detection.
fn detect_sg_nric(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"[STFGMstfgm][0-9]{7}[A-Za-z]");
    for m in re.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && sg_nric_ok(m.as_str()) {
            out.push(det(m.start(), m.end(), "sg_nric", "tier0.sg_nric"));
        }
    }
}

fn sg_nric_ok(s: &str) -> bool {
    let up = s.to_ascii_uppercase();
    let b = up.as_bytes();
    if b.len() != 9 {
        return false;
    }
    const W: [u32; 7] = [2, 7, 6, 5, 4, 3, 2];
    let mut sum: u32 = 0;
    for i in 0..7 {
        let d = (b[1 + i] as char).to_digit(10);
        match d {
            Some(v) => sum += v * W[i],
            None => return false,
        }
    }
    // Era offset: T/G are the 2000s series, M the 2022 foreigner series.
    let (offset, table): (u32, &[u8; 11]) = match b[0] {
        b'S' => (0, b"JZIHGFEDCBA"),
        b'T' => (4, b"JZIHGFEDCBA"),
        b'F' => (0, b"XWUTRQPNMLK"),
        b'G' => (4, b"XWUTRQPNMLK"),
        b'M' => (3, b"KLJNPQRTUWX"),
        _ => return false,
    };
    table[((sum + offset) % 11) as usize] == b[8]
}

/// UAE Emirates ID — 15 digits, `784-YYYY-NNNNNNN-C`, Luhn-checked.
///
/// The `784` issuer prefix plus a Luhn check makes this specific enough to
/// run unconditionally; without the prefix check, any 15-digit run that
/// happened to satisfy Luhn would match.
fn detect_ae_eid(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"784[- ]?[0-9]{4}[- ]?[0-9]{7}[- ]?[0-9]");
    for m in re.find_iter(text) {
        let digits: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
        let vals: Vec<u8> = digits
            .chars()
            .filter_map(|c| c.to_digit(10).map(|d| d as u8))
            .collect();
        if vals.len() == 15 && boundary_ok(text, m.start(), m.end()) && luhn_ok(&vals) {
            out.push(det(m.start(), m.end(), "ae_eid", "tier0.ae_eid"));
        }
    }
}

/// End offsets of every cue match, for the cue-proximity rule shared by the
/// bare-digit-run detectors (`detect_mrn`'s 40-character window, #281).
fn cue_ends(text: &str, cell: &'static OnceLock<Regex>, pattern: &str) -> Vec<usize> {
    re(cell, pattern).find_iter(text).map(|m| m.end()).collect()
}

/// A cue sits within 40 characters before `start`.
fn cue_near(cues: &[usize], start: usize) -> bool {
    cues.iter()
        .any(|c| start >= *c && start.saturating_sub(*c) <= 40)
}

/// The three digit groups of an `AAA-GG-SSSS`-shaped candidate, dashed,
/// spaced, or bare. Returns `None` when the run is not nine digits.
fn us_id_groups(s: &str) -> Option<(u32, u32, u32)> {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() != 9 {
        return None;
    }
    let n = |sl: &[u32]| sl.iter().fold(0u32, |a, d| a * 10 + d);
    Some((n(&digits[0..3]), n(&digits[3..5]), n(&digits[5..9])))
}

/// US Social Security Numbers — `AAA-GG-SSSS` / `AAA GG SSSS` unconditionally,
/// a bare nine-digit run only with an SSN cue within 40 characters before it.
///
/// Structure-validated rather than checksummed (the SSA issues no check
/// digit): area ≠ 000, ≠ 666 and not 900–999 (which is the ITIN range, so the
/// two detectors are disjoint by construction), group ≠ 00, serial ≠ 0000.
/// Without the bare-run cue gate this would redact every nine-digit quantity.
fn detect_us_ssn(text: &str, out: &mut Vec<Detection>) {
    static SEP: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let sep = re(&SEP, r"[0-9]{3}[- ][0-9]{2}[- ][0-9]{4}");
    for m in sep.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && us_ssn_ok(m.as_str()) {
            out.push(det(m.start(), m.end(), "us_ssn", "tier0.us_ssn"));
        }
    }
    let cues = cue_ends(
        text,
        &CUE,
        r"(?i)\b(ssn|ss#|social security (number|no\.?)|soc sec|taxpayer id|tin)\b",
    );
    if cues.is_empty() {
        return;
    }
    let bare = re(&BARE, r"[0-9]{9}");
    for m in bare.find_iter(text) {
        if boundary_ok(text, m.start(), m.end())
            && cue_near(&cues, m.start())
            && us_ssn_ok(m.as_str())
        {
            out.push(det(m.start(), m.end(), "us_ssn", "tier0.us_ssn"));
        }
    }
}

fn us_ssn_ok(s: &str) -> bool {
    match us_id_groups(s) {
        Some((area, group, serial)) => {
            area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
        }
        None => false,
    }
}

/// US Individual Taxpayer Identification Numbers — the SSN shapes with an
/// area beginning in 9 and a group in one of the IRS-assigned ranges. Bare
/// runs are cue-gated; the dashed form is specific enough on its own.
fn detect_us_itin(text: &str, out: &mut Vec<Detection>) {
    static SEP: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let sep = re(&SEP, r"9[0-9]{2}[- ][0-9]{2}[- ][0-9]{4}");
    for m in sep.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && us_itin_ok(m.as_str()) {
            out.push(det(m.start(), m.end(), "us_itin", "tier0.us_itin"));
        }
    }
    let cues = cue_ends(
        text,
        &CUE,
        r"(?i)\b(itin|individual taxpayer identification( number)?)\b",
    );
    if cues.is_empty() {
        return;
    }
    let bare = re(&BARE, r"9[0-9]{8}");
    for m in bare.find_iter(text) {
        if boundary_ok(text, m.start(), m.end())
            && cue_near(&cues, m.start())
            && us_itin_ok(m.as_str())
        {
            out.push(det(m.start(), m.end(), "us_itin", "tier0.us_itin"));
        }
    }
}

fn us_itin_ok(s: &str) -> bool {
    match us_id_groups(s) {
        Some((area, group, _serial)) => {
            (900..=999).contains(&area)
                && matches!(group, 50..=65 | 70..=88 | 90..=92 | 94..=99)
        }
        None => false,
    }
}

/// ABA routing transit numbers — nine digits, a valid Federal Reserve
/// district prefix, and the standard 3-7-1 weighted checksum.
///
/// ALWAYS cue-gated: the checksum passes roughly one random nine-digit run in
/// ten, so shape plus checksum alone would redact ordinary figures.
fn detect_aba_routing(text: &str, out: &mut Vec<Detection>) {
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let cues = cue_ends(
        text,
        &CUE,
        r"(?i)\b(routing|aba|rtn|routing transit)\b",
    );
    if cues.is_empty() {
        return;
    }
    let num = re(&NUM, r"[0-9]{9}");
    for m in num.find_iter(text) {
        if boundary_ok(text, m.start(), m.end())
            && cue_near(&cues, m.start())
            && aba_routing_ok(m.as_str())
        {
            out.push(det(m.start(), m.end(), "aba_routing", "tier0.aba_routing"));
        }
    }
}

fn aba_routing_ok(s: &str) -> bool {
    let d: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if d.len() != 9 {
        return false;
    }
    let prefix = d[0] * 10 + d[1];
    let prefix_ok =
        matches!(prefix, 0..=12 | 21..=32 | 61..=72 | 80);
    if !prefix_ok {
        return false;
    }
    let sum = 3 * (d[0] + d[3] + d[6]) + 7 * (d[1] + d[4] + d[7]) + (d[2] + d[5] + d[8]);
    sum.is_multiple_of(10)
}

// ---------------------------------------------------------------------------
// Indian tax identifiers (issue #347)
// ---------------------------------------------------------------------------
//
// A GSTIN and a PAN sit on almost every Indian invoice, receipt and bank
// statement, and a counterparty's values cannot be listed in a term set in
// advance. The GSTIN carries a mod-36 check character, so it runs
// unconditionally; the PAN has no public checksum, so it is cue-gated.

/// Indian GST identification numbers — 15 characters: a 2-digit state code,
/// the holder's 10-character PAN, an entity character `[1-9A-Z]`, the letter
/// `Z`, and a mod-36 check character.
///
/// Case-insensitive input, upper-cased before checking. The state code must
/// be `01`–`38`, `97` (other territory) or `99` (centre jurisdiction), and
/// the check character must match. Wrong check → not a detection.
fn detect_in_gstin(text: &str, out: &mut Vec<Detection>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(
        &RE,
        r"[0-9]{2}[A-Za-z]{5}[0-9]{4}[A-Za-z][1-9A-Za-z][Zz][0-9A-Za-z]",
    );
    for m in re.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && in_gstin_ok(m.as_str()) {
            out.push(det(m.start(), m.end(), "in_gstin", "tier0.in_gstin"));
        }
    }
}

fn in_gstin_ok(s: &str) -> bool {
    const ALPHABET: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let up = s.to_ascii_uppercase();
    let b = up.as_bytes();
    if b.len() != 15 {
        return false;
    }
    let state = match (
        (b[0] as char).to_digit(10),
        (b[1] as char).to_digit(10),
    ) {
        (Some(t), Some(u)) => t * 10 + u,
        _ => return false,
    };
    if !matches!(state, 1..=38 | 97 | 99) {
        return false;
    }
    // Luhn-style mod 36 over the first 14 characters: weights alternate
    // 1, 2 from the left; each product contributes quotient + remainder.
    let mut sum: u32 = 0;
    for (i, c) in b[..14].iter().enumerate() {
        let Some(v) = (*c as char).to_digit(36) else {
            return false;
        };
        let p = v * if i % 2 == 0 { 1 } else { 2 };
        sum += p / 36 + p % 36;
    }
    ALPHABET[((36 - sum % 36) % 36) as usize] == b[14]
}

/// Indian Permanent Account Numbers — `[A-Z]{3}` + a holder-type letter
/// (`P C H F A T B L J G`) + `[A-Z]` + four digits + `[A-Z]`.
///
/// ALWAYS cue-gated: the PAN has no public checksum, and its shape collides
/// with product codes and SKUs. A PAN embedded in a GSTIN is never a
/// separate detection — the boundary check rejects it, and the GSTIN
/// detection covers the span.
fn detect_in_pan(text: &str, out: &mut Vec<Detection>) {
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let cues = cue_ends(
        text,
        &CUE,
        r"(?i)\b(permanent account number|income tax pan|pan no\.?|pan)\b",
    );
    if cues.is_empty() {
        return;
    }
    let num = re(
        &NUM,
        r"[A-Za-z]{3}[PCHFATBLJGpchfatbljg][A-Za-z][0-9]{4}[A-Za-z]",
    );
    for m in num.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) && cue_near(&cues, m.start()) {
            out.push(det(m.start(), m.end(), "in_pan", "tier0.in_pan"));
        }
    }
}

// ---------------------------------------------------------------------------
// Indian personal identifiers (issue #418)
// ---------------------------------------------------------------------------
//
// Citizens type these the way Indians write them — `9843122760`, `98431
// 22760`, `+91 98431 22760`, `2345 6789 0124` — usually inside Tamil, Hindi or
// mixed-script prose. Two things follow:
//
// * Boundaries are SCRIPT-aware, not merely Unicode-aware. The shared
//   `boundary_ok` treats any alphanumeric neighbour as "inside a word", and
//   Tamil letters and vowel signs are alphabetic, so `எண்9843122760` (a number
//   glued to the word for "number") would be refused. For a digit run, the
//   neighbour is "inside" only when it is another digit or a Latin
//   letter/underscore; a letter of another script is a boundary.
// * Collisions with what already ships are resolved deliberately: a shape the
//   generic `phone` detector already reports keeps its `phone` category (so a
//   value-derived pseudonym minted before this detector existed does not
//   change), dates, amounts and pincodes stay clean, and the Aadhaar check
//   digit is the Verhoeff dihedral checksum, so a single wrong digit — or any
//   adjacent transposition — is not a detection.

/// The digit-run boundary for the Indian detectors (see the section comment).
/// Also refuses a run continued by a decimal/thousands separator and more
/// digits (`1,9843122760`, `9843122760.50`), and one directly after `+` — a
/// `+`-prefixed number is an international number, which `detect_phone` owns.
fn digit_run_boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let inside = |c: char| c.is_numeric() || c.is_ascii_alphabetic() || c == '_';
    let mut before = text[..start].chars().rev();
    let b1 = before.next();
    let b2 = before.next();
    let mut after = text[end..].chars();
    let a1 = after.next();
    let a2 = after.next();
    if b1.is_some_and(|c| inside(c) || c == '+') || a1.is_some_and(inside) {
        return false;
    }
    let sep = |c: char| matches!(c, '.' | ',');
    let continues_before = b1.is_some_and(sep) && b2.is_some_and(|c| c.is_ascii_digit());
    let continues_after = a1.is_some_and(sep) && a2.is_some_and(|c| c.is_ascii_digit());
    !(continues_before || continues_after)
}

/// A currency marker directly before the run, or a money word directly
/// after it: `₹9843122760`, `Rs. 7500000000`, `9500000000 rupees`. Ten-digit
/// amounts starting 6–9 are rare, but an amount is the canonical must-not-
/// redact case and costs one look in each direction.
fn looks_like_amount(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].trim_end().to_lowercase();
    let after = text[end..].trim_start().to_lowercase();
    ["₹", "rs", "rs.", "inr", "$", "usd", "€", "£", "ரூ", "ரூ."]
        .iter()
        .any(|m| before.ends_with(m))
        || ["rupees", "rupee", "crore", "lakh", "/-", "ரூபாய்"]
            .iter()
            .any(|w| after.starts_with(w))
}

/// End offsets of every cue match for the Indian detectors. Unlike
/// [`cue_ends`] (which uses `\b`), these cues include Tamil and Hindi words
/// ending in a combining sign (`எண்` ends in the virama), after which `\b`
/// never fires; so the patterns carry no `\b` and the boundary is checked on
/// the leading edge only.
fn script_cue_ends(text: &str, cell: &'static OnceLock<Regex>, pattern: &str) -> Vec<usize> {
    re(cell, pattern)
        .find_iter(text)
        .filter(|m| {
            text[..m.start()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        })
        .map(|m| m.end())
        .collect()
}

/// Indian mobile numbers — ten digits starting 6–9, with an optional `+91`,
/// `91` or `0` prefix and the common `5 5` / `3 3 4` groupings (space or
/// dash).
///
/// Shape alone is enough (the 6–9 first digit is the numbering-plan rule, and
/// it excludes epoch-second timestamps and most reference numbers); a cue word
/// within 40 characters before the number (`mobile`, `cell`, `ph`, `எண்`,
/// `கைபேசி`, `தொலைபேசி`, …) raises confidence from 0.9 to 1.0. Amounts are
/// refused. A span `detect_phone` already reported as `phone` is NOT reported
/// again — the number keeps the category (and therefore the value-derived
/// pseudonym) it had before this detector existed.
fn detect_in_mobile(text: &str, out: &mut Vec<Detection>) {
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let num = re(
        &NUM,
        r"(?:\+91[ \-]?|91[ \-]?|0[ \-]?)?(?:[6-9][0-9]{4}[ \-]?[0-9]{5}|[6-9][0-9]{2}[ \-][0-9]{3}[ \-][0-9]{4})",
    );
    let cues = script_cue_ends(
        text,
        &CUE,
        r"(?i)(mobile|mob\.?|cell|phone|ph\.?|contact|whatsapp|call|எண்|கைபேசி|தொலைபேசி|அலைபேசி|मोबाइल|फ़ोन|फोन)",
    );
    let phone_spans: Vec<(usize, usize)> = out
        .iter()
        .filter(|d| d.category == "phone")
        .map(|d| (d.start, d.end))
        .collect();
    for m in num.find_iter(text) {
        let (start, end) = (m.start(), m.end());
        if !digit_run_boundary_ok(text, start, end) || looks_like_amount(text, start, end) {
            continue;
        }
        if phone_spans.contains(&(start, end)) {
            continue;
        }
        let mut d = det(start, end, "in_mobile", "tier0.in_mobile");
        if !cue_near(&cues, start) {
            d.confidence = 0.9;
        }
        out.push(d);
    }
}

// Verhoeff tables: the dihedral group D5 multiplication and the position
// permutation (Verhoeff 1969; UIDAI uses it for the Aadhaar check digit).
const VERHOEFF_D: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];
const VERHOEFF_P: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 8, 7, 6, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];

/// Verhoeff check over a digit sequence whose LAST digit is the check digit.
/// Catches every single-digit error and every adjacent transposition.
fn verhoeff_ok(digits: &[u8]) -> bool {
    let mut c = 0u8;
    for (i, d) in digits.iter().rev().enumerate() {
        c = VERHOEFF_D[c as usize][VERHOEFF_P[i % 8][*d as usize] as usize];
    }
    c == 0
}

fn digits_of(s: &str) -> Vec<u8> {
    s.chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect()
}

/// Aadhaar numbers — twelve digits, first digit 2–9, Verhoeff-checked — and
/// 16-digit Aadhaar Virtual IDs (VID), also Verhoeff-checked.
///
/// The `4 4 4` grouped form (space or dash) runs unconditionally: that
/// grouping is how the number is printed on the card, and the checksum keeps
/// nine in ten random grouped runs out. A BARE twelve-digit run is cue-gated
/// (`Aadhaar`, `UID`, `ஆதார்`, `आधार` within 40 characters) because Indian
/// payments are full of other twelve-digit numbers (UPI reference / UTR) and
/// one in ten of those passes Verhoeff by chance. A VID is always cue-gated
/// (`VID`, `virtual id`, or an Aadhaar cue) — sixteen digits is also a card
/// number's length.
fn detect_in_aadhaar(text: &str, out: &mut Vec<Detection>) {
    static GROUPED: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    static VID: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    static VID_CUE: OnceLock<Regex> = OnceLock::new();
    let grouped = re(&GROUPED, r"[2-9][0-9]{3}[ \-][0-9]{4}[ \-][0-9]{4}");
    for m in grouped.find_iter(text) {
        let digits = digits_of(m.as_str());
        if digits.len() == 12
            && digit_run_boundary_ok(text, m.start(), m.end())
            && verhoeff_ok(&digits)
        {
            out.push(det(m.start(), m.end(), "in_aadhaar", "tier0.in_aadhaar"));
        }
    }
    let cues = script_cue_ends(
        text,
        &CUE,
        r"(?i)(aadhaar|aadhar|adhaar|uidai|uid|ஆதார்|आधार)",
    );
    let vid_cues = script_cue_ends(text, &VID_CUE, r"(?i)(vid|virtual id)");
    if !cues.is_empty() {
        let bare = re(&BARE, r"[2-9][0-9]{11}");
        for m in bare.find_iter(text) {
            if digit_run_boundary_ok(text, m.start(), m.end())
                && cue_near(&cues, m.start())
                && verhoeff_ok(&digits_of(m.as_str()))
            {
                out.push(det(m.start(), m.end(), "in_aadhaar", "tier0.in_aadhaar"));
            }
        }
    }
    if cues.is_empty() && vid_cues.is_empty() {
        return;
    }
    let vid = re(&VID, r"[0-9]{4}[ \-]?[0-9]{4}[ \-]?[0-9]{4}[ \-]?[0-9]{4}");
    for m in vid.find_iter(text) {
        let digits = digits_of(m.as_str());
        if digits.len() == 16
            && digit_run_boundary_ok(text, m.start(), m.end())
            && (cue_near(&vid_cues, m.start()) || cue_near(&cues, m.start()))
            && verhoeff_ok(&digits)
        {
            out.push(det(m.start(), m.end(), "in_aadhaar", "tier0.in_aadhaar_vid"));
        }
    }
}

/// Indian voter ID (EPIC) numbers — three letters and seven digits.
///
/// ALWAYS cue-gated (`EPIC`, `voter id`, `voter`, `வாக்காளர்`, `मतदाता`
/// within 40 characters): there is no public checksum, and the shape collides
/// with order and ticket references.
fn detect_in_epic(text: &str, out: &mut Vec<Detection>) {
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let cues = script_cue_ends(
        text,
        &CUE,
        r"(?i)(epic|voter id|voter|elector|வாக்காளர்|मतदाता)",
    );
    if cues.is_empty() {
        return;
    }
    let num = re(&NUM, r"[A-Za-z]{3}[0-9]{7}");
    for m in num.find_iter(text) {
        if digit_run_boundary_ok(text, m.start(), m.end()) && cue_near(&cues, m.start()) {
            out.push(det(m.start(), m.end(), "in_epic", "tier0.in_epic"));
        }
    }
}

/// The state / union-territory codes of Indian registration marks (including
/// the retired `OR`, `UA`, `DD`, `DN` still on older plates).
const IN_STATE_CODES: &[&str] = &[
    "AN", "AP", "AR", "AS", "BR", "CG", "CH", "DD", "DL", "DN", "GA", "GJ", "HP", "HR",
    "JH", "JK", "KA", "KL", "LA", "LD", "MH", "ML", "MN", "MP", "MZ", "NL", "OD", "OR",
    "PB", "PY", "RJ", "SK", "TN", "TR", "TS", "UA", "UK", "UP", "WB",
];

/// Indian vehicle registration marks — `TN 37 AB 1234`: a state code, a one-
/// or two-digit RTO code, a series of up to three letters, and a number of up
/// to four digits, with spaces, dashes or nothing between the parts.
///
/// Always needs a listed state code, plus one of:
/// * the canonical plate shape — state code and series written UPPERCASE, a
///   one-to-three letter series and a four-digit number — which ordinary
///   prose does not produce (`up 12 am 2024` is lowercase and stays clean);
/// * a vehicle cue within 40 characters before (`vehicle`, `registration`,
///   `reg no`, `வாகன`, `वाहन`, …), which also admits lowercase and the short
///   forms (`dl 1 c 123`).
fn detect_in_vehicle(text: &str, out: &mut Vec<Detection>) {
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    let num = re(
        &NUM,
        r"([A-Za-z]{2})[ \-]?([0-9]{1,2})[ \-]?([A-Za-z]{0,3})[ \-]?([0-9]{1,4})",
    );
    let cues = script_cue_ends(
        text,
        &CUE,
        r"(?i)(vehicle|registration|reg\.? ?no|regn|number plate|bike|lorry|வாகன|வண்டி|वाहन|गाड़ी)",
    );
    for c in num.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        if !digit_run_boundary_ok(text, m.start(), m.end())
            || !boundary_ok(text, m.start(), m.end())
        {
            continue;
        }
        let state = &c[1];
        if !IN_STATE_CODES.contains(&state.to_ascii_uppercase().as_str()) {
            continue;
        }
        let series = &c[3];
        let number = &c[4];
        let canonical = state.chars().all(|ch| ch.is_ascii_uppercase())
            && series.chars().all(|ch| ch.is_ascii_uppercase())
            && (1..=3).contains(&series.len())
            && number.len() == 4;
        if canonical || cue_near(&cues, m.start()) {
            out.push(det(m.start(), m.end(), "in_vehicle", "tier0.in_vehicle"));
        }
    }
}

/// Medical record numbers — shape alone is a bare digit run, which is why
/// this is CUE-GATED rather than pattern-only: a 6–12 digit run counts only
/// when an MRN cue word sits within 40 characters before it. Matching bare
/// digit runs would redact every quantity in a clinical note.
fn detect_mrn(text: &str, out: &mut Vec<Detection>) {
    static CUE: OnceLock<Regex> = OnceLock::new();
    static NUM: OnceLock<Regex> = OnceLock::new();
    let cue = re(
        &CUE,
        r"(?i)\b(mrn|medical record (no\.?|number)|patient (id|number)|chart (no\.?|number)|hospital number)\b",
    );
    let num = re(&NUM, r"[A-Za-z]{0,3}[0-9]{6,12}");
    let cues: Vec<usize> = cue.find_iter(text).map(|m| m.end()).collect();
    if cues.is_empty() {
        return;
    }
    for m in num.find_iter(text) {
        if !boundary_ok(text, m.start(), m.end()) {
            continue;
        }
        let near = cues
            .iter()
            .any(|c| m.start() >= *c && m.start().saturating_sub(*c) <= 40);
        if near {
            out.push(det(m.start(), m.end(), "mrn", "tier0.mrn"));
        }
    }
}

fn shannon_bits_per_byte(s: &str) -> f64 {
    let bytes = s.as_bytes();
    let mut counts = [0u32; 256];
    for b in bytes {
        counts[*b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

/// Secrets: known credential prefixes, PEM private-key blocks, and a
/// high-entropy heuristic for token-shaped runs. Exactly-64-lowercase-hex is
/// excluded from the entropy arm — that silhouette is a grain hash, an
/// identifier agents legitimately pass around, not a credential.
fn detect_secret(text: &str, out: &mut Vec<Detection>) {
    static PREFIXED: OnceLock<Regex> = OnceLock::new();
    static PEM: OnceLock<Regex> = OnceLock::new();
    static ENTROPY: OnceLock<Regex> = OnceLock::new();
    let prefixed = re(
        &PREFIXED,
        r"(sk-[A-Za-z0-9_\-]{16,}|sk_live_[A-Za-z0-9]{8,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[bpasr]-[A-Za-z0-9\-]{10,}|AIza[0-9A-Za-z_\-]{30,})",
    );
    for m in prefixed.find_iter(text) {
        if boundary_ok(text, m.start(), m.end()) {
            out.push(det(m.start(), m.end(), "secret", "tier0.secret_prefix"));
        }
    }
    let pem = re(
        &PEM,
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?s:.*?)(-----END [A-Z ]*PRIVATE KEY-----|\z)",
    );
    for m in pem.find_iter(text) {
        out.push(det(m.start(), m.end(), "secret", "tier0.secret_pem"));
    }
    let entropy = re(&ENTROPY, r"[A-Za-z0-9+/=_\-]{24,}");
    for m in entropy.find_iter(text) {
        let s = m.as_str();
        let is_grain_hash = s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit());
        let mixed = s.chars().any(|c| c.is_ascii_uppercase())
            && s.chars().any(|c| c.is_ascii_lowercase())
            && s.chars().any(|c| c.is_ascii_digit());
        if !is_grain_hash
            && mixed
            && shannon_bits_per_byte(s) >= 3.7
            && boundary_ok(text, m.start(), m.end())
        {
            out.push(det(m.start(), m.end(), "secret", "tier0.secret_entropy"));
        }
    }
}

/// Keyword-proximity (proposal §5.1): a cue word plus a value shape nearby
/// (`pin number is 1462`). The span is the value only — the cue is context
/// that keeps the anonymized text readable. Bare digit runs with no cue
/// never match.
fn detect_keyword_proximity(text: &str, out: &mut Vec<Detection>) {
    static PIN: OnceLock<Regex> = OnceLock::new();
    static OTP: OnceLock<Regex> = OnceLock::new();
    static PASSWORD: OnceLock<Regex> = OnceLock::new();
    static ACCOUNT: OnceLock<Regex> = OnceLock::new();
    const FILLER: &str = r#"(?:\s|is|was|are|be|my|the|your|number|no\.?|code|:|=|#|"|')"#;
    let rules: [(&Regex, &str, &str); 4] = [
        (
            // `\b` after the cue (#418): "pincode" is the Indian POSTAL code,
            // and `pin` + the filler word `code` used to read it as a secret
            // PIN, so every `pincode 641001` was redacted as one.
            re(&PIN, &format!(r"(?i)\b(?:pin|passcode)\b{FILLER}{{0,6}}([0-9]{{4,8}})\b")),
            "pin",
            "tier0.kw_pin",
        ),
        (
            re(
                &OTP,
                &format!(r"(?i)\b(?:otp|one[ \-]time (?:code|password)){FILLER}{{0,6}}([0-9]{{4,10}})\b"),
            ),
            "otp",
            "tier0.kw_otp",
        ),
        (
            re(
                &PASSWORD,
                &format!(r#"(?i)\b(?:password|passphrase|pwd){FILLER}{{0,6}}([^\s"']{{4,64}})"#),
            ),
            "password",
            "tier0.kw_password",
        ),
        (
            re(
                &ACCOUNT,
                &format!(r"(?i)\b(?:account|acct\.?){FILLER}{{0,6}}([A-Za-z0-9\-]{{6,20}})\b"),
            ),
            "account_number",
            "tier0.kw_account",
        ),
    ];
    for (rule, category, detector) in rules {
        for c in rule.captures_iter(text) {
            let m = c.get(1).expect("value group");
            if category == "account_number"
                && m.as_str().chars().filter(char::is_ascii_digit).count() < 4
            {
                continue;
            }
            // Free-form password captures may drag sentence punctuation in;
            // trim it off the span so the mapping holds the credential alone.
            let mut end = m.end();
            if category == "password" {
                let trimmed = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
                end = m.start() + trimmed.len();
                if trimmed.len() < 4 {
                    continue;
                }
            }
            out.push(det(m.start(), end, category, detector));
        }
    }
}

/// Identity strings become prose match terms: the full identity, and — when
/// it is namespace-shaped (`caller:john`) — the tail after the last colon.
/// Tails shorter than three characters are skipped (they would match
/// everywhere).
fn identity_match_terms(identities: &[String]) -> Vec<String> {
    let mut terms = Vec::new();
    for id in identities {
        let id = id.trim();
        if id.len() >= 3 {
            terms.push(id.to_string());
        }
        if let Some((_, tail)) = id.rsplit_once(':') {
            let tail = tail.trim();
            if tail.len() >= 3 {
                terms.push(tail.to_string());
            }
        }
    }
    terms
}

/// `AnonPolicy.known` — caller-supplied identities (issue #32), grouped by
/// each entry's own category rather than tier0's fixed `"person"`. Same
/// boundary-checked verbatim matching and 3-char floor as
/// [`identity_match_terms`], just without the colon-tail heuristic (that
/// one is specific to store subject id shapes like `caller:john`; a bare
/// caller-supplied value has no such convention to lean on).
pub(super) fn run_known(
    text: &str,
    known: &[super::KnownIdentity],
) -> Result<Vec<Detection>> {
    let mut by_category: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for k in known {
        let value = k.value.trim();
        if value.len() >= 3 {
            by_category.entry(k.category.as_str()).or_default().push(value.to_string());
        }
    }
    let mut out = Vec::new();
    for (category, terms) in by_category {
        detect_terms(text, &terms, category, "tier0.policy_known", &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Policy-declared regex detectors (issue #418)
// ---------------------------------------------------------------------------

/// Longest pattern a policy may declare, in bytes.
pub const MAX_POLICY_PATTERN_LEN: usize = 1024;
/// Compiled-program size cap for a policy pattern (`RegexBuilder::size_limit`).
/// The `regex` crate is a finite-automaton engine — no backtracking, so no
/// catastrophic matching time — but a pattern like `(a{1000}){1000}` still
/// blows up at COMPILE time; this bounds that and refuses it at declaration.
pub const POLICY_REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Lazy-DFA cache cap per compiled policy pattern.
const POLICY_REGEX_DFA_LIMIT: usize = 2 << 20;
/// Compiled patterns kept process-wide; past this the cache is cleared rather
/// than grown (policies are few, and a clear only costs recompiles).
const POLICY_REGEX_CACHE_CAP: usize = 256;

/// Compile (and validate) one policy-declared pattern. Refuses an over-long
/// pattern, one that exceeds the compile size limit, and one that matches the
/// empty string — an empty match is a zero-width "detection" that redacts
/// nothing while reporting that it did.
pub(super) fn compile_policy_pattern(pattern: &str) -> Result<std::sync::Arc<Regex>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(rx) = cache.lock().expect("regex cache poisoned").get(pattern) {
        return Ok(rx.clone());
    }
    let invalid = |why: String| {
        crate::error::AreevError::Validation(format!(
            "invalid anonymization policy: detector pattern {pattern:?} {why}"
        ))
    };
    if pattern.is_empty() {
        return Err(invalid("is empty".into()));
    }
    if pattern.len() > MAX_POLICY_PATTERN_LEN {
        return Err(invalid(format!("is longer than {MAX_POLICY_PATTERN_LEN} bytes")));
    }
    let rx = regex::RegexBuilder::new(pattern)
        .size_limit(POLICY_REGEX_SIZE_LIMIT)
        .dfa_size_limit(POLICY_REGEX_DFA_LIMIT)
        .build()
        .map_err(|e| invalid(format!("does not compile within the size limit: {e}")))?;
    if rx.is_match("") {
        return Err(invalid(
            "matches the empty string — a detector must match at least one character".into(),
        ));
    }
    let rx = Arc::new(rx);
    let mut guard = cache.lock().expect("regex cache poisoned");
    if guard.len() >= POLICY_REGEX_CACHE_CAP {
        guard.clear();
    }
    guard.insert(pattern.to_string(), rx.clone());
    Ok(rx)
}

/// Run the policy's regex detectors. A detector with `cue` words only fires
/// when one of them (case-insensitive, leading-edge boundary — the same
/// script-aware rule the Indian detectors use) ends within `within_chars`
/// characters before the match. Zero-width matches are skipped defensively
/// (declaration refuses patterns that match "", but `\b`-anchored ones can
/// still match empty mid-text).
pub(super) fn run_policy_regex(
    text: &str,
    specs: &[&super::RegexDetector],
) -> Result<Vec<Detection>> {
    let mut out = Vec::new();
    for spec in specs {
        let rx = compile_policy_pattern(&spec.pattern)?;
        let cues: Vec<usize> = if spec.cue.is_empty() {
            Vec::new()
        } else {
            // The cue words are literals: escaped into one case-insensitive
            // alternation, compiled through the same bounded cache.
            let alternation: Vec<String> =
                spec.cue.iter().map(|c| regex::escape(c.trim())).collect();
            let cue_rx = compile_policy_pattern(&format!("(?i)(?:{})", alternation.join("|")))?;
            cue_rx
                .find_iter(text)
                .filter(|m| {
                    text[..m.start()]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_alphanumeric())
                })
                .map(|m| m.end())
                .collect()
        };
        for m in rx.find_iter(text) {
            if m.start() == m.end() {
                continue;
            }
            if !spec.cue.is_empty()
                && !cues.iter().any(|c| {
                    m.start() >= *c && text[*c..m.start()].chars().count() <= spec.within_chars
                })
            {
                continue;
            }
            out.push(det(m.start(), m.end(), &spec.category, "policy.regex"));
        }
    }
    Ok(out)
}

/// Case-insensitive, boundary-checked term matching for the user dictionary
/// and known identities. Terms are matched verbatim (multi-word allowed).
fn detect_terms(
    text: &str,
    terms: &[String],
    category: &str,
    detector: &str,
    out: &mut Vec<Detection>,
) -> Result<()> {
    for term in terms {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        let pattern = format!("(?i){}", regex::escape(term));
        let rx = Regex::new(&pattern).map_err(|e| {
            crate::error::AreevError::Validation(format!(
                "invalid anonymization policy: term '{term}' does not compile: {e}"
            ))
        })?;
        for m in rx.find_iter(text) {
            if boundary_ok(text, m.start(), m.end()) {
                out.push(det(m.start(), m.end(), category, detector));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats(text: &str) -> Vec<(String, String)> {
        let dets = run_tier0(text, &[], &[], &Default::default()).unwrap();
        dets.iter().map(|d| (d.category.clone(), text[d.start..d.end].to_string())).collect()
    }

    #[test]
    fn luhn_and_mod97_gates() {
        assert!(luhn_ok(&[4, 5, 3, 9, 1, 4, 8, 8, 0, 3, 4, 3, 6, 4, 6, 7]));
        assert!(!luhn_ok(&[4, 5, 3, 9, 1, 4, 8, 8, 0, 3, 4, 3, 6, 4, 6, 8]));
        assert!(iban_mod97_ok("GB82WEST12345698765432"));
        assert!(!iban_mod97_ok("GB82WEST12345698765433"));
    }

    #[test]
    fn ipv6_skips_rust_paths() {
        assert!(cats("use areev_core::anon::scan;").is_empty());
        let found = cats("node at fe80::1 responded");
        assert_eq!(found, vec![("ipv6".into(), "fe80::1".into())]);
    }

    #[test]
    fn plain_digit_runs_are_not_phones() {
        assert!(cats("in 2026 there were 1462 cases").is_empty());
    }
}

#[cfg(test)]
mod national_id_tests {
    use super::*;

    fn cats(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        run_tier0(text, &[], &[], &Default::default()).unwrap();
        let mut d = Vec::new();
        detect_sg_nric(text, &mut d);
        detect_ae_eid(text, &mut d);
        detect_mrn(text, &mut d);
        for x in d {
            out.push(format!("{}:{}", x.category, &text[x.start..x.end]));
        }
        out
    }

    #[test]
    fn sg_nric_checksum_gates_the_detection() {
        // One valid value per prefix class, each with its era offset applied.
        for valid in ["S1234567D", "T1234567J", "F1234567N", "G1234567X", "M1234567X"] {
            assert!(sg_nric_ok(valid), "{valid} must validate");
            assert_eq!(cats(&format!("nric {valid} on file")).len(), 1, "{valid}");
        }
        // Same shape, wrong check letter — precision-first means this is NOT a
        // detection, not a low-confidence one.
        for invalid in ["S1234567A", "T1234567D", "F1234567D", "Z1234567D"] {
            assert!(!sg_nric_ok(invalid), "{invalid} must not validate");
            assert!(cats(&format!("nric {invalid}")).is_empty(), "{invalid}");
        }
    }

    #[test]
    fn sg_nric_is_case_insensitive_but_bounded() {
        assert_eq!(cats("s1234567d").len(), 1);
        // Embedded in a longer alphanumeric run: not a standalone identifier.
        assert!(cats("XS1234567D9").is_empty());
    }

    #[test]
    fn ae_eid_needs_the_issuer_prefix_and_luhn() {
        // 784-1985-1234567-C: solve for the Luhn check digit.
        let base = "78419851234567";
        let vals: Vec<u8> = base.chars().map(|c| c.to_digit(10).unwrap() as u8).collect();
        let check = (0..10u8)
            .find(|c| {
                let mut v = vals.clone();
                v.push(*c);
                luhn_ok(&v)
            })
            .expect("a Luhn check digit always exists");
        let eid = format!("784-1985-1234567-{check}");
        assert_eq!(cats(&format!("emirates id {eid}")).len(), 1, "{eid}");

        // Wrong check digit, and a 15-digit run without the 784 prefix.
        let bad = format!("784-1985-1234567-{}", (check + 1) % 10);
        assert!(cats(&bad).is_empty(), "{bad} must fail Luhn");
        assert!(cats("123456789012345").is_empty(), "no issuer prefix");
    }

    #[test]
    fn mrn_needs_a_cue_word() {
        // Cue-gated: the same digit run is PHI next to "MRN" and a quantity
        // otherwise. Matching bare runs would redact every number in a note.
        assert_eq!(cats("MRN 00456123 admitted").len(), 1);
        assert_eq!(cats("Medical Record Number: 4471820").len(), 1);
        assert!(
            cats("the ward handled 4471820 samples last year").is_empty(),
            "a bare digit run is not an MRN"
        );
        // Cue present but far away — outside the proximity window.
        let far = format!("MRN was not recorded.{} 4471820", " ".repeat(60));
        assert!(cats(&far).is_empty(), "proximity window must bound the cue");
    }
}

#[cfg(test)]
mod us_id_tests {
    use super::*;

    /// Only the US detectors, so a phone/date overlap does not hide a miss.
    fn cats(text: &str) -> Vec<String> {
        let mut d = Vec::new();
        detect_us_ssn(text, &mut d);
        detect_us_itin(text, &mut d);
        detect_aba_routing(text, &mut d);
        d.iter()
            .map(|x| format!("{}:{}", x.category, &text[x.start..x.end]))
            .collect()
    }

    #[test]
    fn ssn_separated_forms_need_no_cue() {
        assert_eq!(cats("123-45-6789"), vec!["us_ssn:123-45-6789"]);
        assert_eq!(cats("123 45 6789"), vec!["us_ssn:123 45 6789"]);
    }

    #[test]
    fn ssn_bare_run_needs_a_cue() {
        assert_eq!(cats("SSN 123456789"), vec!["us_ssn:123456789"]);
        // No cue: an ordinary nine-digit quantity stays clean.
        assert!(cats("bare 123456789").is_empty());
        assert!(cats("EBITDA 123456789").is_empty());
    }

    #[test]
    fn ssn_structure_is_validated() {
        for bad in [
            "000-45-6789",
            "666-45-6789",
            "900-45-6789",
            "123-00-6789",
            "123-45-0000",
        ] {
            assert!(cats(bad).is_empty(), "{bad} must not be an SSN");
        }
    }

    #[test]
    fn ssn_respects_word_boundaries() {
        assert!(cats("X123-45-6789Y").is_empty());
    }

    #[test]
    fn itin_group_ranges_are_validated() {
        for ok in ["912-70-1234", "900-50-1234", "999-88-0001", "950-94-1234"] {
            assert_eq!(cats(ok), vec![format!("us_itin:{ok}")], "{ok}");
        }
        for bad in ["912-49-1234", "912-66-1234", "912-89-1234", "912-93-1234"] {
            assert!(cats(bad).is_empty(), "{bad} must not be an ITIN");
        }
    }

    #[test]
    fn ssn_and_itin_are_disjoint() {
        // The ITIN area range (900-999) is exactly what `us_ssn_ok` excludes.
        assert!(!us_ssn_ok("912-70-1234"));
        assert!(!us_itin_ok("123-45-6789"));
    }

    #[test]
    fn aba_needs_cue_prefix_and_checksum() {
        for ok in ["021000021", "011000015", "121000248"] {
            assert_eq!(
                cats(&format!("routing {ok}")),
                vec![format!("aba_routing:{ok}")],
                "{ok}"
            );
        }
        // Bad checksum.
        assert!(cats("routing 021000022").is_empty());
        // Passes the checksum but starts outside every Federal Reserve
        // district prefix.
        assert!(aba_checksum_only("130000006"));
        assert!(cats("routing 130000006").is_empty());
        // No cue at all, and a cue too far away.
        assert!(cats("invoice 021000021").is_empty());
        let far = format!("routing{} 021000021", " ".repeat(45));
        assert!(cats(&far).is_empty());
    }

    fn aba_checksum_only(s: &str) -> bool {
        let d: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
        (3 * (d[0] + d[3] + d[6]) + 7 * (d[1] + d[4] + d[7]) + (d[2] + d[5] + d[8])).is_multiple_of(10)
    }

    #[test]
    fn a_real_phone_is_not_a_us_identifier() {
        assert!(cats("(212) 555-0142").is_empty());
        let mut d = Vec::new();
        detect_phone("(212) 555-0142", &mut d);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].category, "phone");
    }

    #[test]
    fn a_dashed_ssn_also_matches_the_phone_shape() {
        // Both fire; `resolve_overlaps` picks the validated one (see
        // `anon::tests`). Detection stays additive so a phone-only policy
        // still redacts the span.
        let mut d = Vec::new();
        detect_phone("123-45-6789", &mut d);
        assert_eq!(d.len(), 1, "the dashed phone pattern still matches");
    }
}

#[cfg(test)]
mod in_tax_id_tests {
    use super::*;

    /// Only the Indian detectors, so an overlap does not hide a miss.
    fn cats(text: &str) -> Vec<String> {
        let mut d = Vec::new();
        detect_in_gstin(text, &mut d);
        detect_in_pan(text, &mut d);
        d.iter()
            .map(|x| format!("{}:{}", x.category, &text[x.start..x.end]))
            .collect()
    }

    #[test]
    fn gstin_check_character_is_mod_36_weighted_from_the_left() {
        // The published example GSTIN validates under weights 1,2,1,2,… from
        // the first character; the reverse weighting would demand `T`.
        assert!(in_gstin_ok("27AAPFU0939F1ZV"));
        assert!(!in_gstin_ok("27AAPFU0939F1ZT"));
        for ok in ["29AAACI1681G1ZL", "07AAACR5055K1Z9", "33ABCPE1234F2ZH", "97AAAAA0000A1ZV"] {
            assert!(in_gstin_ok(ok), "{ok} must validate");
        }
    }

    #[test]
    fn a_valid_gstin_is_one_detection_over_all_15_characters() {
        let text = "Supplier GSTIN 27AAPFU0939F1ZV";
        let mut d = Vec::new();
        detect_in_gstin(text, &mut d);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].category, "in_gstin");
        assert_eq!(d[0].end - d[0].start, 15);
        assert_eq!(&text[d[0].start..d[0].end], "27AAPFU0939F1ZV");
        // Case-insensitive input.
        assert_eq!(cats("gstin 27aapfu0939f1zv"), vec!["in_gstin:27aapfu0939f1zv"]);
    }

    #[test]
    fn gstin_check_and_state_code_gate_the_detection() {
        // Last two characters transposed: the check fails.
        assert!(cats("27AAPFU0939F1VZ").is_empty());
        // One middle character changed: the check fails.
        assert!(cats("27AAPFU0938F1ZV").is_empty());
        // State codes 00, 39 and 98 are not assigned.
        assert!(cats("00AAPFU0939F1ZV").is_empty());
        assert!(!in_gstin_ok("39AAPFU0939F1ZV"));
        assert!(!in_gstin_ok("98AAPFU0939F1ZV"));
        // Embedded in a longer alphanumeric run.
        assert!(cats("X27AAPFU0939F1ZV").is_empty());
        assert!(cats("27AAPFU0939F1ZV9").is_empty());
    }

    #[test]
    fn pan_needs_a_cue() {
        assert_eq!(cats("PAN: AAPFU0939F"), vec!["in_pan:AAPFU0939F"]);
        assert_eq!(cats("PAN No. AAPFU0939F"), vec!["in_pan:AAPFU0939F"]);
        assert_eq!(
            cats("Permanent Account Number AAPFU0939F"),
            vec!["in_pan:AAPFU0939F"]
        );
        assert_eq!(cats("Income Tax PAN ABCPE1234F"), vec!["in_pan:ABCPE1234F"]);
        // No cue: a PAN-shaped product code stays clean.
        assert!(cats("SKU AAPFU0939F").is_empty());
        // Cue too far away.
        let far = format!("PAN{} AAPFU0939F", " ".repeat(45));
        assert!(cats(&far).is_empty());
        // "pan" inside another word is not a cue.
        assert!(cats("company AAPFU0939F").is_empty());
    }

    #[test]
    fn pan_holder_type_is_validated() {
        // The holder type is the FOURTH character. `X` is not a holder-type
        // letter, so `AAPXU0939F` is not a PAN even after a cue.
        assert!(cats("PAN: AAPXU0939F").is_empty());
        // Issue #347 lists `AAXFU0939F` as the invalid-holder case, but its
        // fourth character is `F` (firm) — a valid holder type. The `X` sits
        // in the free third position, so by the PAN format it IS PAN-shaped.
        assert_eq!(cats("PAN: AAXFU0939F"), vec!["in_pan:AAXFU0939F"]);
    }

    #[test]
    fn a_pan_inside_a_gstin_is_covered_by_the_gstin() {
        // The PAN inside the GSTIN is not a separate detection, even with a
        // PAN cue in range: the GSTIN detection covers the span.
        assert_eq!(
            cats("PAN / GSTIN 27AAPFU0939F1ZV"),
            vec!["in_gstin:27AAPFU0939F1ZV"]
        );
    }
}

#[cfg(test)]
mod in_personal_id_tests {
    use super::*;

    /// Every Tier-0 detection, resolved the way `scan` resolves them, as
    /// `category:span` — so an overlap with phone/date/card shows up here.
    fn resolved(text: &str) -> Vec<String> {
        let out = super::super::scan(text, &super::super::AnonPolicy::default(), &[]).unwrap();
        out.detections
            .iter()
            .map(|d| format!("{}:{}", d.category, &out.text[d.start..d.end]))
            .collect()
    }

    #[test]
    fn verhoeff_matches_the_published_vector() {
        // Wikipedia's worked example: 236 → check digit 3.
        assert!(verhoeff_ok(&[2, 3, 6, 3]));
        assert!(!verhoeff_ok(&[2, 3, 6, 4]));
        assert!(verhoeff_ok(&digits_of("234567890124")));
        // Every single-digit change of a valid number fails.
        let valid = digits_of("234567890124");
        for i in 0..valid.len() {
            for d in 0..10u8 {
                if d == valid[i] {
                    continue;
                }
                let mut v = valid.clone();
                v[i] = d;
                assert!(!verhoeff_ok(&v), "single-digit change at {i} to {d} passed");
            }
        }
    }

    /// The three reproduction strings of #418, with the Aadhaar corrected to
    /// a checksum-valid number: the issue's `2345 6789 0123` fails Verhoeff
    /// (the valid check digit for `23456789012` is 4), and by the issue's own
    /// acceptance a number that fails it must NOT be reported.
    #[test]
    fn the_issue_repro_strings_detect_every_number() {
        assert_eq!(
            resolved("என் பெயர் முருகன், என் எண் +91 98431 22760"),
            vec!["phone:+91 98431 22760"],
            "the +91 form keeps its pre-#418 category, so its pseudonym is stable"
        );
        assert_eq!(
            resolved("Call me 9843122760, Aadhaar 2345 6789 0124"),
            vec!["in_mobile:9843122760", "in_aadhaar:2345 6789 0124"]
        );
        assert_eq!(
            resolved("முருகன் 98431 22760 காந்திபுரம் 3வது தெரு"),
            vec!["in_mobile:98431 22760"]
        );
        // The issue's literal Aadhaar: one digit off a valid number.
        assert_eq!(
            resolved("Call me 9843122760, Aadhaar 2345 6789 0123"),
            vec!["in_mobile:9843122760"]
        );
    }

    #[test]
    fn mobile_shapes_and_prefixes() {
        for (text, want) in [
            ("9843122760", "9843122760"),
            ("98431 22760", "98431 22760"),
            ("98431-22760", "98431-22760"),
            ("098431-22760", "098431-22760"),
            ("919843122760", "919843122760"),
            ("91 98431 22760", "91 98431 22760"),
            ("984 312 2760", "984 312 2760"),
        ] {
            let mut d = Vec::new();
            detect_in_mobile(text, &mut d);
            assert_eq!(d.len(), 1, "{text}");
            assert_eq!(&text[d[0].start..d[0].end], want, "{text}");
        }
    }

    #[test]
    fn mobile_first_digit_and_boundaries() {
        for text in [
            "1234567890",        // first digit outside 6-9
            "5843122760",        // first digit outside 6-9
            "98431227601",       // eleven digits
            "x9843122760",       // glued to a Latin letter
            "Rs. 9843122760",    // an amount
            "₹9843122760",       // an amount
            "9843122760 rupees", // an amount
            "1,9843122760",      // inside a larger number
            "9843122760.50",     // a decimal
            "+9843122760",       // an international number, phone's
        ] {
            let mut d = Vec::new();
            detect_in_mobile(text, &mut d);
            assert!(d.is_empty(), "{text} must not be an Indian mobile: {d:?}");
        }
    }

    #[test]
    fn a_number_glued_to_tamil_script_is_still_detected() {
        // `எண்` (number) directly followed by the digits, no space, and a
        // Tamil case suffix glued to the end.
        let text = "என் எண்9843122760ஐ அழைக்கவும்";
        let mut d = Vec::new();
        detect_in_mobile(text, &mut d);
        assert_eq!(d.len(), 1);
        assert_eq!(&text[d[0].start..d[0].end], "9843122760");
        // A cue raises confidence; shape alone is 0.9.
        assert_eq!(d[0].confidence, 1.0);
        let mut bare = Vec::new();
        detect_in_mobile("ref 9843122760", &mut bare);
        assert_eq!(bare[0].confidence, 0.9);
        // Non-Latin text around a detection survives byte for byte.
        let out = super::super::anonymize(text, &super::super::AnonPolicy::default(), &[], None)
            .unwrap();
        assert!(out.text.starts_with("என் எண்[IN_MOBILE_1]ஐ அழைக்கவும்"), "{}", out.text);
    }

    #[test]
    fn aadhaar_gates() {
        let mut d = Vec::new();
        detect_in_aadhaar("2345-6789-0124", &mut d);
        assert_eq!(d.len(), 1, "dash-grouped form");
        // Bare twelve digits: only with a cue.
        let mut d = Vec::new();
        detect_in_aadhaar("UTR 234567890124", &mut d);
        assert!(d.is_empty(), "an uncued bare 12-digit run is a payment reference");
        let mut d = Vec::new();
        detect_in_aadhaar("ஆதார் 234567890124", &mut d);
        assert_eq!(d.len(), 1, "Tamil cue");
        // First digit 0/1 is never an Aadhaar.
        let first: Vec<u8> = digits_of("12345678901");
        let check = (0..10u8)
            .find(|c| {
                let mut v = first.clone();
                v.push(*c);
                verhoeff_ok(&v)
            })
            .unwrap();
        let mut d = Vec::new();
        detect_in_aadhaar(&format!("1234 5678 901{check}"), &mut d);
        assert!(d.is_empty());
    }

    #[test]
    fn a_dashed_aadhaar_is_aadhaar_not_phone() {
        assert_eq!(resolved("2345-6789-0124"), vec!["in_aadhaar:2345-6789-0124"]);
    }

    #[test]
    fn vid_is_cue_gated_and_verhoeff_checked() {
        let base = digits_of("912345678901234");
        let check = (0..10u8)
            .find(|c| {
                let mut v = base.clone();
                v.push(*c);
                verhoeff_ok(&v)
            })
            .unwrap();
        let vid = format!("9123 4567 8901 234{check}");
        let mut d = Vec::new();
        detect_in_aadhaar(&format!("VID {vid}"), &mut d);
        assert_eq!(d.len(), 1, "{vid}");
        assert_eq!(d[0].detector, "tier0.in_aadhaar_vid");
        let mut d = Vec::new();
        detect_in_aadhaar(&format!("card {vid}"), &mut d);
        assert!(d.is_empty(), "no cue");
        let bad = format!("9123 4567 8901 234{}", (check + 1) % 10);
        let mut d = Vec::new();
        detect_in_aadhaar(&format!("VID {bad}"), &mut d);
        assert!(d.is_empty(), "bad check digit");
    }

    #[test]
    fn epic_needs_a_cue() {
        let mut d = Vec::new();
        detect_in_epic("Voter ID: ABC1234567", &mut d);
        assert_eq!(d.len(), 1);
        let mut d = Vec::new();
        detect_in_epic("வாக்காளர் அட்டை TNX0123456", &mut d);
        assert_eq!(d.len(), 1, "Tamil cue");
        let mut d = Vec::new();
        detect_in_epic("order ABC1234567 shipped", &mut d);
        assert!(d.is_empty());
    }

    #[test]
    fn vehicle_registration_gates() {
        for text in ["TN 37 AB 1234", "TN37AB1234", "TN-37-AB-1234", "KA 01 MJ 2024"] {
            let mut d = Vec::new();
            detect_in_vehicle(text, &mut d);
            assert_eq!(d.len(), 1, "{text}");
            assert_eq!(d[0].end - d[0].start, text.len(), "{text}");
        }
        // Lowercase or short forms need a cue.
        let mut d = Vec::new();
        detect_in_vehicle("we left up 12 am 2024", &mut d);
        assert!(d.is_empty(), "lowercase prose is not a plate");
        let mut d = Vec::new();
        detect_in_vehicle("vehicle no tn 37 ab 1234", &mut d);
        assert_eq!(d.len(), 1, "a cue admits lowercase");
        // Not a listed state code.
        let mut d = Vec::new();
        detect_in_vehicle("XX 37 AB 1234", &mut d);
        assert!(d.is_empty());
    }

    #[test]
    fn dates_amounts_and_pincodes_stay_clean() {
        for text in [
            "complaint dated 12-05-2024",
            "complaint dated 12.05.2024",
            "complaint dated 12/05/2024",
            "pincode 641001",
            "Coimbatore - 641 001",
            "amount Rs. 7,50,000",
            "₹ 98,43,122",
            "total 2026 cases in ward 37",
        ] {
            let got: Vec<String> = resolved(text)
                .into_iter()
                .filter(|c| c.starts_with("in_") || c.starts_with("phone") || c.starts_with("pin:"))
                .collect();
            assert!(got.is_empty(), "{text}: {got:?}");
        }
        // A day-first dashed date is a date now, not a phone.
        assert_eq!(resolved("12-05-2024"), vec!["date:12-05-2024"]);
    }
}

#[cfg(test)]
mod policy_regex_tests {
    use super::super::{scan, AnonPolicy};

    #[test]
    fn a_declared_regex_detector_runs_with_its_cue() {
        let policy = AnonPolicy::from_json(
            r#"{"detectors": [{"category": "ward_ref", "pattern": "W[0-9]{3}-[0-9]{4}",
                "cue": ["ward", "வார்டு"]}]}"#,
        )
        .unwrap();
        let out = scan("ward complaint W012-2024 filed", &policy, &[]).unwrap();
        assert_eq!(out.detections.len(), 1);
        assert_eq!(out.detections[0].category, "ward_ref");
        assert_eq!(out.detections[0].detector, "policy.regex");
        // No cue → no detection.
        let out = scan("part number W012-2024", &policy, &[]).unwrap();
        assert!(out.detections.is_empty());
        // A regex-only list keeps the tier0 link.
        let out = scan("mail jane@example.com", &policy, &[]).unwrap();
        assert_eq!(out.detections[0].category, "email");
    }

    #[test]
    fn declaration_refuses_bad_patterns() {
        for (pattern, why) in [
            ("(", "does not compile"),
            ("a*", "matches the empty string"),
            ("(?:x|)", "matches the empty string"),
            ("(a{1000}){1000}", "size limit"),
        ] {
            let json = serde_json::json!({"detectors": [{"category": "c", "pattern": pattern}]});
            let err = AnonPolicy::from_json(&json.to_string()).unwrap_err().to_string();
            assert!(err.contains("VAL-E001") && err.contains(why), "{pattern}: {err}");
        }
        let long = "a".repeat(super::MAX_POLICY_PATTERN_LEN + 1);
        let json = serde_json::json!({"detectors": [{"category": "c", "pattern": long}]});
        assert!(AnonPolicy::from_json(&json.to_string()).is_err());
        // Unknown keys on a detector object are refused (fail-closed policy).
        assert!(AnonPolicy::from_json(
            r#"{"detectors": [{"category": "c", "pattern": "x+", "flags": "i"}]}"#
        )
        .is_err());
    }

    #[test]
    fn a_string_only_detector_list_reserializes_unchanged() {
        // The canonical policy JSON feeds mapping_id, so pre-#418 policies
        // must serialize byte-for-byte as before.
        let p = AnonPolicy::from_json(r#"{"detectors": ["tier0"]}"#).unwrap();
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""detectors":["tier0"]"#), "{s}");
        assert!(!s.contains("reveal"), "{s}");
    }

    #[test]
    fn reveal_policy_validates() {
        let p = AnonPolicy::from_json(r#"{"reveal": {"mode": "dual", "ttl_secs": 1800}}"#).unwrap();
        assert!(p.reveal_policy().is_dual());
        assert!(!AnonPolicy::default().reveal_policy().is_dual());
        assert!(AnonPolicy::from_json(r#"{"reveal": {"mode": "triple"}}"#).is_err());
        assert!(AnonPolicy::from_json(r#"{"reveal": {"mode": "dual", "ttl_secs": 0}}"#).is_err());
        assert!(AnonPolicy::from_json(r#"{"reveal": {"mode": "dual", "ttl_secs": 99999999}}"#)
            .is_err());
    }
}
