//! When is this trigger next due?
//!
//! Interval, cron and one-shot are three declarations of one primitive: a
//! function from "when did it last fire" to "when should it fire next". They
//! are kept apart in the vocabulary because they read differently to a person,
//! not because the evaluator treats them differently.
//!
//! Two behaviours here are worth stating because they are the ones people get
//! wrong:
//!
//! **Rate drifts; cron does not.** `interval_secs` anchors to the *last firing*,
//! so a 3-minute job on a 5-minute interval leaves a 2-minute gap and the
//! effective period depends on runtime. `*/5 * * * *` anchors to wall-clock
//! boundaries and does not. Both are correct; users need to know which they
//! chose, so the two are separate kinds rather than one field with a flag.
//!
//! **Catch-up collapses by default.** A laptop closed over a weekend has missed
//! hundreds of occurrences. `Catchup::Last` fires once on resume, which is what
//! anacron does and what systemd's `Persistent=true` means. `All` replays them
//! and `None` drops them; the three-way is Quartz's misfire vocabulary — fire
//! once now / skip / replay all.

use areev_core::types::{Catchup, Trigger, TriggerKind};
use croner::Cron;

use crate::error::{Result, TriggerError};

/// This release evaluates cron in UTC only.
///
/// IANA timezone support means `chrono-tz` (~794 KB, and it pulls `phf`) or a
/// second time library, and it means picking a side in a disagreement that real
/// implementations have not settled: AWS EventBridge and robfig/cron *skip* an
/// occurrence that falls in a spring-forward gap, while Vixie cron fires it
/// immediately. They agree on the fall-back fold — fire once, do not repeat —
/// so only the gap is contested, but that is enough to make a silent guess the
/// wrong move. Refusing a timezone we would only mishandle is better than
/// firing at the wrong hour and being believed.
const SUPPORTED_TIMEZONE: &str = "UTC";

/// Parse and validate a declaration's schedule without evaluating it.
///
/// Called at authoring time so a bad expression is refused when it is written
/// rather than discovered as silence.
pub fn validate(trigger: &Trigger) -> Result<()> {
    if let Some(why) = trigger.incoherence() {
        return Err(TriggerError::Malformed { what: why });
    }
    if trigger.kind == TriggerKind::Schedule {
        let expr = trigger.cron.as_deref().unwrap_or_default();
        parse_cron(expr)?;
        if let Some(tz) = trigger.config_str(areev_core::types::config_keys::TIMEZONE) {
            if !tz.eq_ignore_ascii_case(SUPPORTED_TIMEZONE) {
                return Err(TriggerError::BadSchedule {
                    expr: expr.to_string(),
                    why: format!(
                        "timezone {tz:?} is not supported in this release — cron is evaluated \
                         in UTC. Declare the expression in UTC rather than having it fire at \
                         the wrong local hour across a DST boundary"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn parse_cron(expr: &str) -> Result<Cron> {
    Cron::new(expr)
        .with_seconds_optional()
        .parse()
        .map_err(|e| TriggerError::BadSchedule { expr: expr.to_string(), why: e.to_string() })
}

/// The next instant at or after `after_ms` at which `trigger` should fire.
///
/// `None` means "never again", which only a one-shot past its instant can be.
pub fn next_due_after(trigger: &Trigger, after_ms: i64) -> Result<Option<i64>> {
    match trigger.kind {
        TriggerKind::Once => {
            let at = trigger.at_ms.ok_or_else(|| TriggerError::Malformed {
                what: "a once trigger needs at_ms".into(),
            })?;
            Ok(if at > after_ms { Some(at) } else { None })
        }
        TriggerKind::Interval | TriggerKind::Polling => {
            let secs = trigger.interval_secs.ok_or_else(|| TriggerError::Malformed {
                what: format!("a {} trigger needs interval_secs", trigger.kind.as_str()),
            })?;
            Ok(Some(after_ms.saturating_add(i64::from(secs) * 1000)))
        }
        TriggerKind::Schedule => {
            let expr = trigger.cron.as_deref().unwrap_or_default();
            let cron = parse_cron(expr)?;
            let after = chrono::DateTime::from_timestamp_millis(after_ms).ok_or_else(|| {
                TriggerError::BadSchedule {
                    expr: expr.to_string(),
                    why: format!("{after_ms} is not a representable instant"),
                }
            })?;
            let next = cron.find_next_occurrence(&after, false).map_err(|e| {
                TriggerError::BadSchedule { expr: expr.to_string(), why: e.to_string() }
            })?;
            Ok(Some(next.timestamp_millis()))
        }
        // Push and gate kinds are not clocked: they fire when something arrives.
        TriggerKind::Memory | TriggerKind::Webhook | TriggerKind::Manual | TriggerKind::Composite => {
            Ok(None)
        }
    }
}

/// Where to set `next_due_at` after a firing that happened at `fired_at_ms`,
/// given that it is now `now_ms`.
///
/// This is where catch-up policy lands. If evaluation stalled — the host was
/// asleep, the process was down — the naive next instant can be far in the
/// past, and the question is whether to replay the gap.
pub fn advance_after_firing(trigger: &Trigger, fired_at_ms: i64, now_ms: i64) -> Result<Option<i64>> {
    let Some(mut next) = next_due_after(trigger, fired_at_ms)? else {
        return Ok(None);
    };
    if next > now_ms {
        return Ok(Some(next));
    }
    match trigger.catchup {
        // Replay: leave the schedule where it is and let each stale occurrence
        // fire in turn.
        Catchup::All => Ok(Some(next)),
        // Collapse the backlog into a single firing. Walk forward to the first
        // occurrence still ahead of us; one firing already happened, and the
        // others are missed rather than owed.
        Catchup::Last | Catchup::None => {
            // Bounded so a long-dormant memory with a fast interval cannot spin:
            // a year of one-second occurrences would otherwise be 31 million
            // steps. Past the bound we jump straight off `now`.
            const MAX_STEPS: u32 = 10_000;
            let mut steps = 0;
            while next <= now_ms && steps < MAX_STEPS {
                match next_due_after(trigger, next)? {
                    Some(n) if n > next => next = n,
                    // A schedule that does not advance would loop forever.
                    _ => break,
                }
                steps += 1;
            }
            if next <= now_ms {
                next = next_due_after(trigger, now_ms)?.unwrap_or(next);
            }
            Ok(Some(next))
        }
    }
}

/// Back off `next_due_at` after a failure, so a broken connector does not
/// hot-loop against a source that is down.
///
/// Exponential on consecutive failures, capped, and never shorter than the
/// declared interval — a trigger that fails fast should not end up polling
/// *more* often than a healthy one.
pub fn backoff_until(trigger: &Trigger, consecutive_failures: u32, now_ms: i64) -> i64 {
    const BASE_MS: i64 = 30_000;
    const CAP_MS: i64 = 3_600_000;
    let shift = consecutive_failures.saturating_sub(1).min(7);
    let penalty = (BASE_MS << shift).min(CAP_MS);
    let floor = trigger.interval_secs.map(|s| i64::from(s) * 1000).unwrap_or(0);
    now_ms.saturating_add(penalty.max(floor))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    /// 2026-01-01T00:00:00Z
    const T0: i64 = 1_767_225_600_000;

    fn interval(secs: u32) -> Trigger {
        Trigger::new(TriggerKind::Interval, WF).interval_secs(secs)
    }
    fn schedule(expr: &str) -> Trigger {
        Trigger::new(TriggerKind::Schedule, WF).cron(expr)
    }

    #[test]
    fn interval_anchors_to_the_last_firing() {
        let t = interval(120);
        assert_eq!(next_due_after(&t, T0).unwrap(), Some(T0 + 120_000));
        // …and therefore drifts: firing late pushes the next one out.
        assert_eq!(next_due_after(&t, T0 + 5_000).unwrap(), Some(T0 + 125_000));
    }

    #[test]
    fn cron_anchors_to_wall_clock_boundaries() {
        let t = schedule("*/5 * * * *");
        let first = next_due_after(&t, T0).unwrap().unwrap();
        assert_eq!(first, T0 + 300_000, "next boundary from midnight");
        // Firing late still lands on the boundary — no drift.
        let after_late = next_due_after(&t, T0 + 301_000).unwrap().unwrap();
        assert_eq!(after_late, T0 + 600_000);
    }

    #[test]
    fn a_five_field_weekday_expression_parses_with_posix_numbering() {
        // The trap this guards: the most-downloaded Rust cron crate requires 7
        // fields with mandatory seconds and Quartz day numbering where 1=Sun and
        // 0 is rejected, so it reads this expression as the wrong days.
        let t = schedule("0 9 * * 1-5");
        let next = next_due_after(&t, T0).unwrap().unwrap();
        let dt = chrono::DateTime::from_timestamp_millis(next).unwrap();
        use chrono::{Datelike, Timelike};
        assert_eq!(dt.hour(), 9);
        assert_eq!(dt.minute(), 0);
        let wd = dt.weekday().num_days_from_monday();
        assert!(wd < 5, "must land Mon-Fri, got {:?}", dt.weekday());
    }

    #[test]
    fn a_one_shot_fires_once_and_then_never() {
        let t = Trigger::new(TriggerKind::Once, WF).at_ms(T0 + 1_000);
        assert_eq!(next_due_after(&t, T0).unwrap(), Some(T0 + 1_000));
        assert_eq!(next_due_after(&t, T0 + 1_000).unwrap(), None, "not due at its own instant again");
        assert_eq!(next_due_after(&t, T0 + 9_999).unwrap(), None);
    }

    #[test]
    fn push_and_gate_kinds_are_not_clocked() {
        for kind in [TriggerKind::Webhook, TriggerKind::Manual, TriggerKind::Memory] {
            let t = Trigger::new(kind, WF);
            assert_eq!(next_due_after(&t, T0).unwrap(), None, "{kind:?} should not be clocked");
        }
    }

    #[test]
    fn catchup_last_collapses_a_weekend_of_missed_occurrences() {
        // Fired Friday, evaluated Monday: a 5-minute schedule missed ~800
        // occurrences. Exactly one firing is owed, not eight hundred.
        let t = schedule("*/5 * * * *").catchup(Catchup::Last);
        let three_days = 3 * 86_400_000;
        let next = advance_after_firing(&t, T0, T0 + three_days).unwrap().unwrap();
        assert!(next > T0 + three_days, "must be scheduled ahead of now, not in the backlog");
        assert!(next <= T0 + three_days + 300_000, "and at the very next boundary");
    }

    #[test]
    fn catchup_all_leaves_the_backlog_in_place() {
        let t = schedule("*/5 * * * *").catchup(Catchup::All);
        let three_days = 3 * 86_400_000;
        let next = advance_after_firing(&t, T0, T0 + three_days).unwrap().unwrap();
        assert_eq!(next, T0 + 300_000, "the stale occurrence stays due, to be replayed");
    }

    #[test]
    fn collapsing_is_bounded_for_a_fast_interval_over_a_long_gap() {
        // A one-second interval dormant for a year is ~31M occurrences. The walk
        // must not attempt them.
        let t = interval(1).catchup(Catchup::Last);
        let a_year = 365 * 86_400_000i64;
        let started = std::time::Instant::now();
        let next = advance_after_firing(&t, T0, T0 + a_year).unwrap().unwrap();
        assert!(next > T0 + a_year);
        assert!(started.elapsed().as_millis() < 500, "the collapse walk must be bounded");
    }

    #[test]
    fn a_firing_that_is_still_ahead_of_now_is_left_alone() {
        let t = interval(300);
        let next = advance_after_firing(&t, T0, T0 + 1_000).unwrap().unwrap();
        assert_eq!(next, T0 + 300_000);
    }

    #[test]
    fn backoff_grows_then_caps_and_never_polls_faster_than_the_interval() {
        let t = interval(600); // 10 minutes
        let first = backoff_until(&t, 1, T0) - T0;
        let second = backoff_until(&t, 2, T0) - T0;
        assert!(second >= first, "backoff must not shrink as failures accumulate");

        // Never more often than the declared interval: a failing trigger must
        // not end up hammering a source harder than a healthy one.
        assert!(first >= 600_000, "floor is the declared interval");

        let far = backoff_until(&t, 99, T0) - T0;
        assert!(far <= 3_600_000, "and it caps rather than growing without bound");
    }

    #[test]
    fn an_unparseable_expression_is_refused_with_its_code() {
        let t = schedule("not a cron expression");
        let e = validate(&t).unwrap_err();
        assert_eq!(e.code(), "TRG-E006");
        assert!(e.to_string().starts_with("TRG-E006"));
    }

    #[test]
    fn a_non_utc_timezone_is_refused_rather_than_mishandled() {
        // Firing at the wrong local hour and being believed is worse than
        // refusing, because nobody notices a schedule that is quietly an hour
        // off for half the year.
        let t = schedule("0 9 * * *").config(serde_json::json!({
            "int:timezone": "America/New_York"
        }));
        let e = validate(&t).unwrap_err();
        assert_eq!(e.code(), "TRG-E006");
        assert!(e.to_string().contains("UTC"), "the message should say what IS supported");

        // UTC spelled either way is fine.
        let ok = schedule("0 9 * * *").config(serde_json::json!({ "int:timezone": "utc" }));
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn validate_rejects_an_incoherent_declaration_before_the_schedule() {
        let t = Trigger::new(TriggerKind::Interval, WF); // no interval_secs
        let e = validate(&t).unwrap_err();
        assert_eq!(e.code(), "TRG-E001");
    }
}
