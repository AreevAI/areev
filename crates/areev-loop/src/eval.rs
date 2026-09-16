//! Evalset runs — the one reader for the `evalset:<hash> mg:eval_run`
//! summaries that `areev eval run` journals into `agent:harness`.
//!
//! Two edges consume these, and they deliberately share this code:
//!
//! - the **gating** edge (`areev loop apply --gating-run <id>`), which names one
//!   run and reads its numbers rather than trusting the command line, and
//! - the **outcome** edge (`measure_metric`'s `evalset:` kind), which re-reads
//!   the newest run at each checkpoint.
//!
//! Sharing matters because the alternative is two parsers of the same JSON
//! drifting apart, so a rule could be *admitted* on one reading of an evalset
//! and *judged* on another.

use crate::error::Result;
use crate::substrate::{ReadOpts, SubstrateRead};
use serde_json::Value;

/// The relation an eval-run summary is recorded under.
pub const EVAL_RUN_RELATION: &str = "mg:eval_run";

/// The namespace those summaries live in.
pub const HARNESS_NS: &str = "agent:harness";

/// One recorded execution of an evalset.
#[derive(Debug, Clone)]
pub struct EvalRun {
    /// The `eval-` run id the cases were journaled under.
    pub run_id: String,
    pub passed: u64,
    pub failed: u64,
    /// The whole summary object, so host-defined fields (`category_accuracy`,
    /// …) are reachable without this module having to know them.
    pub summary: serde_json::Map<String, Value>,
    /// When the summary grain was written.
    pub recorded_ms: i64,
    /// What the run spent, when the runtime journaled it: the `run_outcome`
    /// Observation `areev run` writes for the same run id. Read only for a
    /// cost key the summary does not carry, so an eval run that IS an
    /// `areev run` run quotes one spend to both the Verify gate and the
    /// `run_outcome` analyzer.
    pub spend: Option<RunSpend>,
}

/// The spent figures of a terminal `run_outcome` Observation
/// (`spent_input_tokens` … `spent_wall_ms`), integers as the runtime wrote
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSpend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd_micros: u64,
    pub wall_ms: u64,
}

/// The cost fields `run_value` promotes, beside the quality ones. `tokens`
/// is input + output; `usd` is `usd_micros / 1e6`; `cost_per_pass` is
/// `usd / passed`, undefined when nothing passed.
pub const COST_FIELDS: [&str; 5] = ["effects", "tokens", "usd", "wall_ms", "cost_per_pass"];

impl EvalRun {
    /// A numeric field of the summary. `passed`/`failed` are promoted to typed
    /// fields but stay readable here too, so a metric string may name any of
    /// them uniformly.
    pub fn field(&self, name: &str) -> Option<f64> {
        self.summary.get(name).and_then(Value::as_f64)
    }

    /// Cases that ran. `0` when the summary records neither.
    pub fn total(&self) -> u64 {
        self.passed.saturating_add(self.failed)
    }

    /// An integer cost key, fail-closed: a summary that carries the key as
    /// anything but a non-negative integer makes the cost NOT measurable —
    /// never zero, and never the runtime's figure either, because a present
    /// wrong value is a malformed record, not a missing one. A key the
    /// summary does not carry at all falls back to the runtime's spend.
    fn cost_key(&self, key: &str, from_spend: impl Fn(&RunSpend) -> u64) -> Option<u64> {
        match self.summary.get(key) {
            Some(v) => v.as_u64(),
            None => self.spend.as_ref().map(from_spend),
        }
    }

    /// Tool calls the cases made. Summary key `effects` only — the runtime's
    /// Observation counts supersteps, not effects, so there is no fallback.
    pub fn effects(&self) -> Option<u64> {
        self.summary.get("effects").and_then(Value::as_u64)
    }

    /// Input + output tokens; both keys must be integers.
    pub fn tokens(&self) -> Option<u64> {
        let i = self.cost_key("input_tokens", |s| s.input_tokens)?;
        let o = self.cost_key("output_tokens", |s| s.output_tokens)?;
        Some(i.saturating_add(o))
    }

    pub fn usd_micros(&self) -> Option<u64> {
        self.cost_key("usd_micros", |s| s.usd_micros)
    }

    pub fn wall_ms(&self) -> Option<u64> {
        self.cost_key("wall_ms", |s| s.wall_ms)
    }
}

/// Every recorded run of `evalset_hash`, oldest first.
///
/// `since_ms` bounds the scan to summaries written at or after a moment —
/// which is what keeps an outcome honest: a run journaled *before* a
/// recommendation was applied cannot be evidence of what applying it did.
pub fn eval_runs<S: SubstrateRead + ?Sized>(
    sub: &S,
    evalset_hash: &str,
    since_ms: Option<i64>,
) -> Result<Vec<EvalRun>> {
    let subject = format!("evalset:{evalset_hash}");
    let facts = sub.grains_of_type(
        crate::model::grain_type::FACT,
        Some(HARNESS_NS),
        ReadOpts { live_only: true, since_ms },
    )?;
    let spends = run_spends(sub)?;
    let mut out: Vec<EvalRun> = facts
        .iter()
        .filter(|f| f.str_field("relation") == Some(EVAL_RUN_RELATION))
        .filter(|f| f.str_field("subject") == Some(subject.as_str()))
        .filter_map(|f| {
            let obj = f.str_field("object")?;
            let Ok(Value::Object(summary)) = serde_json::from_str::<Value>(obj) else {
                // A summary we cannot parse is skipped, never guessed at: a
                // fabricated number here would become a receipt.
                return None;
            };
            // `areev eval run` always writes all three, so a summary missing
            // one is malformed. Dropping it rather than defaulting keeps every
            // consumer fail-CLOSED: at the apply gate an absent `failed` must
            // never read as "zero failures", and an outcome must never score
            // against numbers nobody recorded.
            let run_id = summary.get("run_id").and_then(Value::as_str)?.to_string();
            Some(EvalRun {
                spend: spends.get(&run_id).copied(),
                run_id,
                passed: summary.get("passed").and_then(Value::as_u64)?,
                failed: summary.get("failed").and_then(Value::as_u64)?,
                recorded_ms: f.created_at_ms,
                summary,
            })
        })
        .collect();
    // Recording order, with the hash as a deterministic tiebreak for two
    // summaries written in the same millisecond.
    out.sort_by(|a, b| {
        a.recorded_ms
            .cmp(&b.recorded_ms)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    Ok(out)
}

/// The spent figures of every terminal `run_outcome` Observation, by run id
/// — the runtime's own record, read here so a harness that journals an
/// evalset run under the run id it executed does not have to copy the
/// numbers (and cannot copy them wrong). All four keys must be integers, or
/// the run has no spend here.
fn run_spends<S: SubstrateRead + ?Sized>(sub: &S) -> Result<std::collections::BTreeMap<String, RunSpend>> {
    let obs = match sub.grains_of_type(
        crate::model::grain_type::OBSERVATION,
        Some(HARNESS_NS),
        ReadOpts { live_only: true, since_ms: None },
    ) {
        Ok(rows) => rows,
        // No harness observations / no read grant: no spend — the summary's
        // own keys still work, and a missing cost stays not measurable.
        Err(_) => return Ok(Default::default()),
    };
    let mut out = std::collections::BTreeMap::new();
    for g in &obs {
        if g.str_field("observation_kind") != Some("run_outcome") {
            continue;
        }
        let Some(run_id) = g.str_field("run_id") else { continue };
        let int = |k: &str| g.fields.get(k).and_then(Value::as_u64);
        let (Some(i), Some(o), Some(u), Some(w)) = (
            int("spent_input_tokens"),
            int("spent_output_tokens"),
            int("spent_usd_micros"),
            int("spent_wall_ms"),
        ) else {
            continue;
        };
        out.insert(
            run_id.to_string(),
            RunSpend { input_tokens: i, output_tokens: o, usd_micros: u, wall_ms: w },
        );
    }
    Ok(out)
}

/// The newest recorded run of `evalset_hash` at or after `since_ms`.
pub fn newest_eval_run<S: SubstrateRead + ?Sized>(
    sub: &S,
    evalset_hash: &str,
    since_ms: Option<i64>,
) -> Result<Option<EvalRun>> {
    Ok(eval_runs(sub, evalset_hash, since_ms)?.pop())
}

/// The value a metric field takes on one run. `failed`/`passed`/`total` are
/// promoted so a metric can be written against any evalset without the host
/// having to add fields; `error_rate` is derived (undefined, not zero, when
/// no case ran); the cost fields (`COST_FIELDS`) read the integer cost keys
/// fail-closed, with `cost_per_pass` undefined — not zero, not a division —
/// when nothing passed; anything else is read from the summary the host did
/// write. The proposal-time baseline and every later measurement go through
/// this one reader, so a rule cannot be admitted on one reading of a run and
/// judged on another.
pub fn run_value(run: &EvalRun, field: &str) -> Option<f64> {
    match field {
        "failed" => Some(run.failed as f64),
        "passed" => Some(run.passed as f64),
        "total" => Some(run.total() as f64),
        "error_rate" => match run.total() {
            0 => None,
            t => Some(run.failed as f64 / t as f64),
        },
        "effects" => run.effects().map(|n| n as f64),
        "tokens" => run.tokens().map(|n| n as f64),
        "usd" => run.usd_micros().map(|n| n as f64 / 1e6),
        "wall_ms" => run.wall_ms().map(|n| n as f64),
        "cost_per_pass" => match run.passed {
            0 => None,
            p => run.usd_micros().map(|n| n as f64 / 1e6 / p as f64),
        },
        other => run.field(other),
    }
}

/// One recorded run by id, over all of history.
pub fn eval_run_by_id<S: SubstrateRead + ?Sized>(
    sub: &S,
    evalset_hash: &str,
    run_id: &str,
) -> Result<Option<EvalRun>> {
    Ok(eval_runs(sub, evalset_hash, None)?
        .into_iter()
        .find(|r| r.run_id == run_id))
}

/// Parse an `evalset:<hash>:<field>` metric string into its parts.
///
/// The hash is hex, so splitting on the LAST colon is unambiguous and lets a
/// field name contain no colon by construction.
pub fn parse_evalset_metric(metric: &str) -> Option<(&str, &str)> {
    let rest = metric.strip_prefix("evalset:")?;
    let (hash, field) = rest.rsplit_once(':')?;
    if hash.is_empty() || field.is_empty() || hash.contains(':') {
        return None;
    }
    Some((hash, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(summary: serde_json::Value, spend: Option<RunSpend>) -> EvalRun {
        let summary = summary.as_object().unwrap().clone();
        EvalRun {
            run_id: "eval-1".into(),
            passed: summary.get("passed").and_then(Value::as_u64).unwrap_or(0),
            failed: summary.get("failed").and_then(Value::as_u64).unwrap_or(0),
            summary,
            recorded_ms: 0,
            spend,
        }
    }

    /// The cost keys read fail-closed, exactly like `passed`/`failed`: a
    /// string, a float or a negative number is NOT measurable — never zero —
    /// and `cost_per_pass` is undefined when nothing passed.
    #[test]
    fn cost_fields_are_integers_or_not_measurable() {
        let ok = run(
            serde_json::json!({"passed": 4, "failed": 1, "effects": 12, "input_tokens": 1000,
                               "output_tokens": 200, "usd_micros": 2_000_000, "wall_ms": 340}),
            None,
        );
        assert_eq!(run_value(&ok, "effects"), Some(12.0));
        assert_eq!(run_value(&ok, "tokens"), Some(1200.0));
        assert_eq!(run_value(&ok, "usd"), Some(2.0));
        assert_eq!(run_value(&ok, "wall_ms"), Some(340.0));
        assert_eq!(run_value(&ok, "cost_per_pass"), Some(0.5));

        for bad in [
            serde_json::json!({"passed": 4, "failed": 1, "input_tokens": "1000", "output_tokens": 200}),
            serde_json::json!({"passed": 4, "failed": 1, "input_tokens": 1000.5, "output_tokens": 200}),
            serde_json::json!({"passed": 4, "failed": 1, "input_tokens": -1, "output_tokens": 200}),
            serde_json::json!({"passed": 4, "failed": 1, "output_tokens": 200}),
            serde_json::json!({"passed": 4, "failed": 1}),
        ] {
            let r = run(bad.clone(), None);
            assert_eq!(run_value(&r, "tokens"), None, "{bad}");
            assert_eq!(run_value(&r, "cost_per_pass"), None, "{bad}");
        }
        // Nothing passed: undefined, not a division by zero, not zero.
        let none = run(serde_json::json!({"passed": 0, "failed": 5, "usd_micros": 2_000_000}), None);
        assert_eq!(run_value(&none, "usd"), Some(2.0));
        assert_eq!(run_value(&none, "cost_per_pass"), None);
        // The quality fields are untouched by a malformed cost key.
        let r = run(serde_json::json!({"passed": 4, "failed": 1, "wall_ms": "fast"}), None);
        assert_eq!(run_value(&r, "passed"), Some(4.0));
        assert_eq!(run_value(&r, "wall_ms"), None);
    }

    /// A summary that carries no cost key reads the runtime's spend for the
    /// same run id; one that carries the key — right or wrong — does not.
    #[test]
    fn a_missing_cost_key_falls_back_to_the_runtime_spend_and_a_present_one_does_not() {
        let spend = RunSpend { input_tokens: 700, output_tokens: 300, usd_micros: 4_000_000, wall_ms: 9_000 };
        let r = run(serde_json::json!({"passed": 2, "failed": 0}), Some(spend));
        assert_eq!(run_value(&r, "tokens"), Some(1000.0));
        assert_eq!(run_value(&r, "usd"), Some(4.0));
        assert_eq!(run_value(&r, "wall_ms"), Some(9000.0));
        assert_eq!(run_value(&r, "cost_per_pass"), Some(2.0));
        assert_eq!(run_value(&r, "effects"), None, "the runtime records supersteps, not effects");
        let wrong = run(serde_json::json!({"passed": 2, "failed": 0, "input_tokens": "700"}), Some(spend));
        assert_eq!(run_value(&wrong, "tokens"), None, "a malformed key is malformed, not missing");
        let own = run(serde_json::json!({"passed": 2, "failed": 0, "input_tokens": 1, "output_tokens": 1}), Some(spend));
        assert_eq!(run_value(&own, "tokens"), Some(2.0), "the summary's own figure wins");
    }

    #[test]
    fn metric_strings_parse_into_hash_and_field() {
        assert_eq!(
            parse_evalset_metric("evalset:abc123:category_accuracy"),
            Some(("abc123", "category_accuracy"))
        );
        assert_eq!(parse_evalset_metric("evalset:abc123:failed"), Some(("abc123", "failed")));
        // Not an evalset metric at all.
        assert_eq!(parse_evalset_metric("tool_error_recurrence"), None);
        // Malformed shapes must fail rather than half-parse into a lookup that
        // silently finds nothing.
        assert_eq!(parse_evalset_metric("evalset:abc123"), None);
        assert_eq!(parse_evalset_metric("evalset::field"), None);
        assert_eq!(parse_evalset_metric("evalset:abc123:"), None);
        assert_eq!(parse_evalset_metric("evalset:a:b:c"), None);
    }
}
