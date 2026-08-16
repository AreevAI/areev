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
    detect_secret(text, &mut out);
    detect_keyword_proximity(text, &mut out);
    detect_terms(text, custom_terms, "custom", "tier0.dictionary", &mut out)?;
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
        let dets = run_tier0(text, &[], &[]).unwrap();
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
