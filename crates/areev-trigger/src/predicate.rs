//! Predicates over grains, and boolean gates over triggers.
//!
//! Both reuse `areev_cal::ast::Condition` rather than introducing a language.
//! That AST already has `And`/`Or`/`Not` with parenthesised grouping and the
//! right `OR < AND < NOT` precedence, it already serializes as internally
//! tagged JSON, and `areev_cal::executor::grain_matches_condition_tree` is a
//! total evaluator for it — a missing field is false, never an error.
//!
//! Two constraints made this the only workable shape:
//!
//! - **`areev_run_core::cond` is frozen.** Its module doc states "No expression
//!   language beyond this in v1: that is a spec-shaped surface and a standing
//!   exclusion", repeated as a hard exclusion in the governed-agents proposal.
//!   So Argo Events' `conditions: "(a && b) || c"` — the best syntax in the
//!   survey — is the one thing that cannot be copied.
//! - **New CAL syntax is an OMS conformance decision.** Adding `&&`/`||` tokens
//!   to the lexer is a spec-level change against an external standard.
//!
//! Kestra's shape is the compatible alternative: a data structure with explicit
//! combinator nodes rather than an infix string. A serialized `Condition` tree
//! is exactly that. Knative reached the same conclusion independently, shipping
//! its structural `all`/`any`/`not` filters ahead of its SQL dialect because the
//! structural form was "much more stable".

use areev_cal::ast::Condition;
use areev_cal::executor::{grain_matches_condition_tree, CalGrainResult};
use areev_core::format::deserialize::DeserializedGrain;

use crate::error::{Result, TriggerError};

/// Parse a predicate written as CAL `WHERE` syntax into a `Condition` tree.
///
/// The author writes `enabled = true AND connector = "gmail"`; we wrap it in a
/// throwaway `RECALL` so the *existing* CAL parser does the work, then lift the
/// condition out. No new grammar, no new lexer token, nothing for the spec to
/// ratify.
pub fn parse_predicate(where_src: &str) -> Result<Condition> {
    let probe = format!("RECALL grains WHERE {where_src}");
    let query = areev_cal::parse(&probe).map_err(|e| TriggerError::Malformed {
        what: format!("predicate {where_src:?} does not parse: {e}"),
    })?;
    match query.statement {
        areev_cal::ast::CalStatement::Recall(r) => r
            .where_clause
            .map(|w| w.condition)
            .ok_or_else(|| TriggerError::Malformed { what: "predicate is empty".into() }),
        _ => Err(TriggerError::Malformed {
            what: format!("predicate {where_src:?} is not a filter expression"),
        }),
    }
}

/// Serialize a predicate for storage on a Trigger grain.
pub fn to_value(c: &Condition) -> Result<serde_json::Value> {
    serde_json::to_value(c)
        .map_err(|e| TriggerError::Malformed { what: format!("predicate is not serializable: {e}") })
}

/// Read a predicate back off a Trigger grain.
pub fn from_value(v: &serde_json::Value) -> Result<Condition> {
    serde_json::from_value(v.clone())
        .map_err(|e| TriggerError::Malformed { what: format!("stored predicate is unreadable: {e}") })
}

/// Does this grain satisfy the predicate?
///
/// Projects the stored grain into the shape the CAL evaluator expects. `score`
/// is meaningless here — nothing ranked this grain, it either matched or it
/// did not — so it is zero rather than an invented number.
pub fn grain_matches(grain: &DeserializedGrain, condition: &Condition) -> bool {
    let projected = CalGrainResult {
        hash: grain.hash.to_hex(),
        grain_type: grain.grain_type.as_str().to_string(),
        score: 0.0,
        fields: serde_json::to_value(&grain.fields).unwrap_or(serde_json::Value::Null),
        score_breakdown: None,
        explanation: None,
        relative_time: None,
        is_deterministic: false,
        contested_by: None,
    };
    grain_matches_condition_tree(&projected, condition)
}

/// Evaluate a gate over member triggers that have fired.
///
/// The condition's field names are member ids; a member that has fired is
/// `true`. Reusing the same `Condition` shape means `(a AND b) OR c` over
/// triggers is written and evaluated exactly like a predicate over grains.
pub fn gate_satisfied(condition: &Condition, fired: &[String]) -> bool {
    let fields: serde_json::Map<String, serde_json::Value> = fired
        .iter()
        .map(|m| (m.clone(), serde_json::Value::Bool(true)))
        .collect();
    let projected = CalGrainResult {
        hash: String::new(),
        grain_type: "gate".into(),
        score: 0.0,
        fields: serde_json::Value::Object(fields),
        score_breakdown: None,
        explanation: None,
        relative_time: None,
        is_deterministic: false,
        contested_by: None,
    };
    grain_matches_condition_tree(&projected, condition)
}

/// Every field name a gate expression references.
///
/// Used to refuse a composite whose expression names a member it does not
/// declare — otherwise the gate can never be satisfied and the trigger is
/// silently dead, which is the failure mode with no symptom.
pub fn referenced_members(condition: &Condition) -> Vec<String> {
    fn walk(c: &Condition, out: &mut Vec<String>) {
        match c {
            Condition::Comparison { field, .. }
            | Condition::In { field, .. }
            | Condition::NotIn { field, .. }
            | Condition::IsNull { field, .. }
            | Condition::IsNotNull { field, .. }
            | Condition::Contains { field, .. }
            | Condition::StartsWith { field, .. }
            | Condition::IsCategory { field, .. } => out.push(field.clone()),
            Condition::And { left, right, .. } | Condition::Or { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            Condition::Not { inner, .. } => walk(inner, out),
        }
    }
    let mut out = Vec::new();
    walk(condition, &mut out);
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(src: &str) -> Condition {
        parse_predicate(src).unwrap()
    }

    #[test]
    fn boolean_composition_round_trips_through_storage() {
        // Behaviour, not bytes: source spans are `#[serde(skip)]`, so a stored
        // predicate legitimately comes back without them. What has to survive
        // is what it decides.
        let c = gate("(a = true AND b = true) OR c = true");
        let back = from_value(&to_value(&c).unwrap()).unwrap();

        for fired in [
            vec![],
            vec!["a".to_string()],
            vec!["c".to_string()],
            vec!["a".to_string(), "b".to_string()],
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        ] {
            assert_eq!(
                gate_satisfied(&c, &fired),
                gate_satisfied(&back, &fired),
                "storage changed the meaning for {fired:?}"
            );
        }
        assert_eq!(referenced_members(&c), referenced_members(&back));
    }

    #[test]
    fn and_requires_both_members() {
        let c = gate("a = true AND b = true");
        assert!(!gate_satisfied(&c, &["a".into()]));
        assert!(!gate_satisfied(&c, &["b".into()]));
        assert!(gate_satisfied(&c, &["a".into(), "b".into()]));
    }

    #[test]
    fn or_requires_either() {
        let c = gate("a = true OR b = true");
        assert!(gate_satisfied(&c, &["a".into()]));
        assert!(gate_satisfied(&c, &["b".into()]));
        assert!(!gate_satisfied(&c, &[]));
    }

    #[test]
    fn parenthesised_precedence_matches_the_written_expression() {
        // The Argo Events shape, in Kestra's form: (a && b) || c.
        let c = gate("(a = true AND b = true) OR c = true");
        assert!(gate_satisfied(&c, &["c".into()]), "c alone satisfies the right branch");
        assert!(gate_satisfied(&c, &["a".into(), "b".into()]));
        assert!(!gate_satisfied(&c, &["a".into()]), "a alone satisfies neither branch");
    }

    #[test]
    fn precedence_is_or_below_and_not_left_to_right() {
        // `a AND b OR c` must parse as `(a AND b) OR c`. Read left-to-right it
        // would be `a AND (b OR c)`, which `a` alone would then fail and `c`
        // alone would also fail — so the two readings genuinely differ.
        let c = gate("a = true AND b = true OR c = true");
        assert!(gate_satisfied(&c, &["c".into()]), "c alone must satisfy it");
    }

    #[test]
    fn negation_works() {
        let c = gate("NOT (a = true)");
        assert!(gate_satisfied(&c, &[]), "a absent means the negation holds");
        assert!(!gate_satisfied(&c, &["a".into()]));
    }

    #[test]
    fn referenced_members_finds_every_name() {
        let c = gate("(a = true AND b = true) OR NOT (c = true)");
        assert_eq!(referenced_members(&c), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_predicate_that_does_not_parse_is_refused_with_its_code() {
        let e = parse_predicate("this is not a predicate").unwrap_err();
        assert_eq!(e.code(), "TRG-E001");
    }

    #[test]
    fn an_empty_predicate_is_refused() {
        // A gate that matches everything is a trigger that fires constantly.
        assert!(parse_predicate("").is_err());
    }
}
