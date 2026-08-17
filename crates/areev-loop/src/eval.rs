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
}

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
            Some(EvalRun {
                run_id: summary.get("run_id").and_then(Value::as_str)?.to_string(),
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

/// The newest recorded run of `evalset_hash` at or after `since_ms`.
pub fn newest_eval_run<S: SubstrateRead + ?Sized>(
    sub: &S,
    evalset_hash: &str,
    since_ms: Option<i64>,
) -> Result<Option<EvalRun>> {
    Ok(eval_runs(sub, evalset_hash, since_ms)?.pop())
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
