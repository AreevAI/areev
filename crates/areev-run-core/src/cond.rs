//! The frozen v1 condition grammar (governed-agents §6.4):
//!
//! ```text
//! cond    := path op literal | path "exists" | ["!"] path
//! op      := "==" | "!="
//! path    := segment ("." segment)*      segment := [A-Za-z0-9_-]+
//! literal := JSON string | number | true | false | null
//! ```
//!
//! Strict JSON equality — no coercion (`"1" != 1`; numbers compare by
//! serde_json's own representation semantics, so `1` and `1.0` are distinct,
//! deliberately: coercion rules are exactly the kind of quiet complexity a
//! frozen grammar exists to refuse). Truthiness: `false`, `null`, missing,
//! `""`, `0`, `[]`, `{}` are false; everything else is true. Parse errors are
//! load-time (`RUN-E005`); **evaluation is total** — a missing path is falsey
//! / `exists`-false, never an error. No expression language beyond this in
//! v1: that is a spec-shaped surface and a standing exclusion.
//!
//! Host evaluators plug in driver-side; this module is the built-in. Purity
//! of any evaluator is enforced by the journaled decision record + replay
//! assertion (§5.1), not by trust.

use serde_json::Value;

/// A parsed condition — the only forms the v1 grammar admits.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Cond {
    /// `path == literal` / `path != literal`.
    Compare {
        path: Vec<String>,
        negated: bool,
        literal: Value,
    },
    /// `path exists`.
    Exists { path: Vec<String> },
    /// `path` / `!path`.
    Truthy { path: Vec<String>, negated: bool },
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_path(s: &str) -> Result<Vec<String>, String> {
    let segments: Vec<String> = s.split('.').map(str::to_string).collect();
    if segments.iter().all(|seg| valid_segment(seg)) {
        Ok(segments)
    } else {
        Err(format!(
            "path '{s}' is invalid: segments are [A-Za-z0-9_-]+ joined by '.'"
        ))
    }
}

/// Parse a condition string. Errors carry the reason for `RUN-E005`.
pub fn parse(cond: &str) -> Result<Cond, String> {
    let s = cond.trim();
    if s.is_empty() {
        return Err("empty condition".into());
    }
    // Split at whichever operator occurs FIRST BY POSITION — checking `==`
    // before `!=` over the whole string would mis-split `x != "a == b"` at
    // the literal's `==` and refuse a grammatically legal condition.
    let first = [("==", false), ("!=", true)]
        .into_iter()
        .filter_map(|(op, neg)| s.find(op).map(|i| (i, op, neg)))
        .min_by_key(|(i, _, _)| *i);
    {
        if let Some((idx, op, negated)) = first {
            let path = parse_path(s[..idx].trim())?;
            let lit_text = s[idx + op.len()..].trim();
            if lit_text.is_empty() {
                return Err(format!("'{op}' needs a JSON literal on the right"));
            }
            let literal: Value = serde_json::from_str(lit_text).map_err(|e| {
                format!(
                    "right side of '{op}' must be a JSON literal \
                     (string/number/true/false/null): {e}"
                )
            })?;
            if literal.is_object() || literal.is_array() {
                return Err("object/array literals are not in the v1 grammar".into());
            }
            return Ok(Cond::Compare { path, negated, literal });
        }
    }
    // `path exists`
    if let Some(head) = s.strip_suffix(" exists") {
        return Ok(Cond::Exists { path: parse_path(head.trim())? });
    }
    // `!path` / `path`
    if let Some(rest) = s.strip_prefix('!') {
        return Ok(Cond::Truthy { path: parse_path(rest.trim())?, negated: true });
    }
    Ok(Cond::Truthy { path: parse_path(s)?, negated: false })
}

fn lookup<'a>(state: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = state;
    for seg in path {
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Evaluate a parsed condition against the accumulated state. Total: never
/// errors.
pub fn eval(cond: &Cond, state: &Value) -> bool {
    match cond {
        Cond::Compare { path, negated, literal } => {
            // Strict equality; a missing path equals nothing (and therefore
            // satisfies `!=` — "is not that value" is true of absent).
            let eq = lookup(state, path) == Some(literal);
            eq != *negated
        }
        Cond::Exists { path } => lookup(state, path).is_some(),
        Cond::Truthy { path, negated } => truthy(lookup(state, path)) != *negated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(cond: &str, state: &Value) -> bool {
        eval(&parse(cond).unwrap(), state)
    }

    #[test]
    fn compare_is_strict_json_equality_no_coercion() {
        let state = json!({"n": 1, "s": "1", "f": 1.0, "b": true});
        assert!(ev("n == 1", &state));
        assert!(!ev("s == 1", &state), "\"1\" != 1 — no coercion");
        assert!(!ev("n == \"1\"", &state));
        assert!(!ev("f == 1", &state), "1.0 and 1 are distinct representations");
        assert!(ev("f == 1.0", &state));
        assert!(ev("b == true", &state));
        assert!(ev("s == \"1\"", &state));
        assert!(ev("n != 2", &state));
        assert!(ev("missing != 1", &state), "absent is not that value");
        assert!(!ev("missing == null", &state), "absent is not null either");
        let with_null = json!({"x": null});
        assert!(ev("x == null", &with_null));
    }

    #[test]
    fn truthiness_table_is_exact() {
        let state = json!({
            "t": true, "f": false, "z": 0, "zf": 0.0, "n": 3,
            "e": "", "s": "x", "ea": [], "a": [1], "eo": {}, "o": {"k": 1},
            "nul": null
        });
        for (path, expect) in [
            ("t", true), ("f", false), ("z", false), ("zf", false), ("n", true),
            ("e", false), ("s", true), ("ea", false), ("a", true),
            ("eo", false), ("o", true), ("nul", false), ("missing", false),
        ] {
            assert_eq!(ev(path, &state), expect, "truthy({path})");
            assert_eq!(ev(&format!("!{path}"), &state), !expect, "!{path}");
        }
    }

    #[test]
    fn exists_distinguishes_null_from_absent() {
        let state = json!({"present": null, "nested": {"deep": 0}});
        assert!(ev("present exists", &state));
        assert!(ev("nested.deep exists", &state));
        assert!(!ev("missing exists", &state));
        assert!(!ev("nested.missing exists", &state));
        // But a present-null is falsey.
        assert!(!ev("present", &state));
    }

    #[test]
    fn dotted_paths_traverse_objects_only() {
        let state = json!({"a": {"b": {"c": "deep"}}, "arr": [1, 2]});
        assert!(ev("a.b.c == \"deep\"", &state));
        assert!(!ev("arr.0 exists", &state), "array indexing is not in the grammar");
    }

    /// Every malformed shape is a parse ERROR (load-time RUN-E005), never a
    /// silent false — and evaluation is total, so this is the only place a
    /// condition can fail.
    #[test]
    fn malformed_conditions_are_parse_errors() {
        for bad in [
            "",
            "  ",
            "a == ",
            "a == {\"k\": 1}",
            "a == [1]",
            "a == 1 extra",
            "a b == 1",
            "a..b == 1",
            ".a == 1",
            "a. == 1",
            "path with spaces",
            "a === 1",
            "!",
            "a exists now",
            "café == 1",
        ] {
            assert!(parse(bad).is_err(), "must reject: {bad:?}");
        }
    }

    #[test]
    fn string_literals_containing_operators_survive() {
        let state = json!({"note": "a != b"});
        assert!(!ev("note == \"x\"", &state));
        // The FIRST operator splits: `note == "a != b"` parses as == with the
        // full string literal.
        assert!(ev("note == \"a != b\"", &state));
    }

    /// The parsed form serializes — decision records carry it.
    #[test]
    fn parsed_conditions_round_trip_serde() {
        for c in ["a.b == 3", "x exists", "!flag", "s == \"v\""] {
            let parsed = parse(c).unwrap();
            let json = serde_json::to_string(&parsed).unwrap();
            let back: Cond = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, back);
        }
    }
}
