//! A tiny CAL *writer*. The engine never parses CAL (that is the substrate's
//! job — `validate_cal`/`execute_cal`); it only emits the handful of statements
//! built-in analyzers propose. Statements are newline-separated to form a
//! batch. Keeping this a writer, not a parser, is what lets the engine claim
//! zero CAL-grammar ownership (proposal §10).

use serde_json::{Map, Value};

/// `FORGET <hash>` — the only destructive statement the writer emits,
/// single-grain (§6.4).
pub fn forget(hash: &str) -> String {
    format!("FORGET {hash}")
}

/// `ADD <type> {json}` — a Tier-1 non-destructive evolve write.
pub fn add(grain_type: &str, fields: &Map<String, Value>) -> String {
    format!("ADD {grain_type} {}", Value::Object(fields.clone()))
}

/// `SUPERSEDE <hash> WITH <type> {json}` — replace a head non-destructively.
pub fn supersede(target_hash: &str, grain_type: &str, fields: &Map<String, Value>) -> String {
    format!(
        "SUPERSEDE {target_hash} WITH {grain_type} {}",
        Value::Object(fields.clone())
    )
}

/// Join statements into a batch.
pub fn batch(statements: &[String]) -> String {
    statements.join("\n")
}

/// Round-trip a line this module's own [`supersede`] emitted back into
/// `(target_hash, grain_type, fields)`. This is a strict inverse of the
/// writer's own output — **not** a CAL parser (any other shape returns
/// `None`; the substrate's grammar stays authoritative). The auto-apply gate
/// uses it to value-verify a replacement against the grain it supersedes.
pub fn parse_own_supersede(
    line: &str,
) -> Option<(String, String, Map<String, Value>)> {
    let rest = line.trim().strip_prefix("SUPERSEDE ")?;
    let (target, after) = rest.split_once(" WITH ")?;
    let (grain_type, json) = after.trim().split_once(' ')?;
    match serde_json::from_str(json.trim()).ok()? {
        Value::Object(fields) => {
            Some((target.trim().to_string(), grain_type.to_string(), fields))
        }
        _ => None,
    }
}

/// Cheap engine-side destructive check (defense in depth; the substrate's
/// `validate_cal` is authoritative). True if any statement is a FORGET.
pub fn contains_forget(cal: &str) -> bool {
    any_line_keyword(cal, &["FORGET"])
}

/// True if any statement is destructive: FORGET (single-grain or SUBJECT)
/// or PURGE. `Recommendation::destructive` is stamped from this and the
/// apply gate (admin scope + `allow_destructive`) keys off that stamp, so
/// this list must cover every destructive statement a proposal could carry —
/// LLM-enriched and `--analyzer-cmd` proposals are arbitrary CAL text, not
/// just what this writer emits.
pub fn contains_destructive(cal: &str) -> bool {
    any_line_keyword(cal, &["FORGET", "PURGE"])
}

/// True if any line's leading keyword (case-insensitive) is in `keywords`.
fn any_line_keyword(cal: &str, keywords: &[&str]) -> bool {
    cal.lines().any(|l| {
        let kw = l
            .trim_start()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .unwrap_or("");
        keywords.iter().any(|k| kw.eq_ignore_ascii_case(k))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forget_is_detected() {
        assert!(contains_forget("FORGET sha256:abc"));
        assert!(contains_forget("ADD fact {}\nforget sha256:x"));
        assert!(!contains_forget("ADD fact {}\nSUPERSEDE a WITH fact {}"));
    }

    #[test]
    fn destructive_covers_purge_and_forget_subject() {
        assert!(contains_destructive("FORGET sha256:abc"));
        assert!(contains_destructive(r#"FORGET SUBJECT "pat" BECAUSE "gdpr""#));
        assert!(contains_destructive(r#"PURGE OLDER THAN 90d BECAUSE "retention""#));
        assert!(contains_destructive("ADD fact {}\n  purge older than 30d because \"x\""));
        assert!(!contains_destructive("ADD fact {}\nSUPERSEDE a WITH fact {}"));
        // Keyword match, not substring: reads that mention the words don't trip it.
        assert!(!contains_destructive(r#"RECALL facts WHERE subject = "purge""#));
    }

    #[test]
    fn add_and_supersede_shapes() {
        let mut f = Map::new();
        f.insert("subject".into(), json!("acme"));
        assert!(add("fact", &f).starts_with("ADD fact {"));
        assert!(supersede("sha256:x", "fact", &f).starts_with("SUPERSEDE sha256:x WITH fact {"));
    }

    #[test]
    fn parse_own_supersede_round_trips_the_writer() {
        let mut f = Map::new();
        f.insert("subject".into(), json!("acme"));
        f.insert("object".into(), json!("Enterprise"));
        let line = supersede("sha256:abc", "fact", &f);
        let (target, gtype, fields) = parse_own_supersede(&line).expect("round-trip");
        assert_eq!(target, "sha256:abc");
        assert_eq!(gtype, "fact");
        assert_eq!(fields, f);
    }

    #[test]
    fn parse_own_supersede_rejects_other_shapes() {
        assert!(parse_own_supersede("ADD fact {}").is_none());
        assert!(parse_own_supersede("SUPERSEDE x WITH fact").is_none(), "no json");
        assert!(parse_own_supersede("SUPERSEDE x WITH fact []").is_none(), "non-object json");
        assert!(parse_own_supersede("FORGET x").is_none());
    }
}
