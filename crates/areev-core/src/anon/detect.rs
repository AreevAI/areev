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
];

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
    // No backreferences in the regex crate, so the dash-grouped form is its
    // own pattern; ISO-date-shaped candidates are excluded (the date
    // detector owns those).
    let iso_shape = re(&ISO_SHAPE, r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$");
    let candidates = [
        re(&INTL, r"\+[1-9][0-9 ().\-]{5,18}[0-9]"),
        re(&NANP, r"\(?[0-9]{3}\)?[ .\-][0-9]{3}[ .\-][0-9]{4}"),
        re(&DASHED, r"[0-9]{2,4}-[0-9]{2,4}-[0-9]{2,8}"),
    ];
    for r in candidates {
        for m in r.find_iter(text) {
            if iso_shape.is_match(m.as_str()) {
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
        let parts: Vec<u32> = m.as_str().split('/').map(|p| p.parse().unwrap_or(0)).collect();
        let plausible_dm = (1..=12).contains(&parts[0]) || (1..=12).contains(&parts[1]);
        let in_range =
            (1..=31).contains(&parts[0]) && (1..=31).contains(&parts[1]) && plausible_dm;
        if in_range && boundary_ok(text, m.start(), m.end()) {
            out.push(det(m.start(), m.end(), "date", "tier0.date"));
        }
    }
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
            re(&PIN, &format!(r"(?i)\b(?:pin|passcode){FILLER}{{0,6}}([0-9]{{4,8}})\b")),
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
