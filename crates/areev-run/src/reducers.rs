//! Typed reducers (§6.5): per-key merge functions, declared on the
//! Workflow grain (`reducers: {state_key: name}`), FROZEN into the manifest
//! at resolve — a resume must merge exactly as the original run did, so the
//! choice can never live in host configuration.
//!
//! Every built-in is pure, deterministic, and batching-invariant: merging
//! results one-at-a-time in canonical order equals merging any grouping of
//! them in that order — the §6.5 law the scheduler's canonical-order close
//! depends on. No transcendental float ops (the §5.5 cross-arch hazard);
//! `sum` is plain IEEE addition, bit-stable for a fixed operand order.

use serde_json::Value;
use std::collections::BTreeMap;

/// The v1 reducer names. Append-only — a released name can never change
/// meaning (checkpoints from old runs replay through it).
pub const BUILTIN_REDUCERS: &[&str] = &["lww", "append", "sum", "max", "min"];

/// Is `name` a valid reducer? Manifest resolve refuses unknown names —
/// failing at run start, not at first merge.
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_REDUCERS.contains(&name)
}

/// Merge `new` into `prev` under reducer `name`.
///
/// - `lww` — new value wins (the default for undeclared keys).
/// - `append` — accumulate into an array: arrays concatenate, scalars push;
///   a missing/non-array prev starts a fresh array.
/// - `sum` — numeric addition; a non-numeric operand falls back to LWW
///   (deterministically — the same inputs always fall back the same way).
/// - `max` / `min` — numeric comparison, same non-numeric fallback.
pub fn reduce_with(name: &str, prev: Option<&Value>, new: &Value) -> Value {
    match name {
        "append" => {
            let mut acc = match prev {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            match new {
                Value::Array(items) => acc.extend(items.iter().cloned()),
                other => acc.push(other.clone()),
            }
            Value::Array(acc)
        }
        "sum" => match (prev.and_then(Value::as_f64), new.as_f64()) {
            (Some(p), Some(n)) => number(p + n),
            (None, Some(n)) => number(n),
            _ => new.clone(),
        },
        "max" => match (prev.and_then(Value::as_f64), new.as_f64()) {
            (Some(p), Some(n)) => number(if n > p { n } else { p }),
            (None, Some(n)) => number(n),
            _ => new.clone(),
        },
        "min" => match (prev.and_then(Value::as_f64), new.as_f64()) {
            (Some(p), Some(n)) => number(if n < p { n } else { p }),
            (None, Some(n)) => number(n),
            _ => new.clone(),
        },
        _ => new.clone(), // lww
    }
}

fn number(f: f64) -> Value {
    // Integral results stay integers (JSON round-trip stability: 3, not 3.0
    // — a checkpoint byte-compare must not depend on float formatting).
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// The scheduler's reduce closure over the manifest's frozen table:
/// declared keys use their reducer, everything else LWW.
pub fn make_reduce(
    table: &BTreeMap<String, String>,
) -> impl Fn(&str, Option<&Value>, &Value) -> Value + '_ {
    move |key: &str, prev: Option<&Value>, new: &Value| {
        let name = table.get(key).map(String::as_str).unwrap_or("lww");
        reduce_with(name, prev, new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_accumulates_in_merge_order() {
        let a = reduce_with("append", None, &json!([1, 2]));
        let b = reduce_with("append", Some(&a), &json!(3));
        let c = reduce_with("append", Some(&b), &json!([4]));
        assert_eq!(c, json!([1, 2, 3, 4]));
    }

    #[test]
    fn numeric_reducers_fold() {
        let s = reduce_with("sum", Some(&json!(2)), &json!(3));
        assert_eq!(s, json!(5));
        assert_eq!(reduce_with("max", Some(&json!(2)), &json!(3)), json!(3));
        assert_eq!(reduce_with("min", Some(&json!(2)), &json!(3)), json!(2));
        // Integral results serialize as integers, not 5.0.
        assert_eq!(s.to_string(), "5");
    }

    #[test]
    fn non_numeric_operands_fall_back_deterministically() {
        assert_eq!(reduce_with("sum", Some(&json!("x")), &json!(3)), json!(3));
        assert_eq!(reduce_with("sum", Some(&json!(3)), &json!("x")), json!("x"));
    }

    /// The §6.5 batching-invariance law, checked over sample sequences: for
    /// a fixed canonical order, folding one-at-a-time equals folding any
    /// contiguous grouping.
    #[test]
    fn reducers_are_batching_invariant() {
        let seqs: Vec<Vec<Value>> = vec![
            vec![json!(1), json!(2.5), json!(3), json!(-7)],
            vec![json!([1]), json!(2), json!([3, 4])],
            vec![json!("a"), json!("b"), json!("c")],
        ];
        for name in BUILTIN_REDUCERS {
            for seq in &seqs {
                let one_at_a_time = seq.iter().fold(None::<Value>, |acc, v| {
                    Some(reduce_with(name, acc.as_ref(), v))
                });
                for split in 1..seq.len() {
                    let (l, r) = seq.split_at(split);
                    let left = l.iter().fold(None::<Value>, |acc, v| {
                        Some(reduce_with(name, acc.as_ref(), v))
                    });
                    let both = r.iter().fold(left, |acc, v| {
                        Some(reduce_with(name, acc.as_ref(), v))
                    });
                    assert_eq!(
                        both, one_at_a_time,
                        "{name} not batching-invariant over {seq:?} at {split}"
                    );
                }
            }
        }
    }
}
