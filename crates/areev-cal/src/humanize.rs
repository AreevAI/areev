//! Content projection — relation and time humanization for CAL output.
//!
//! This module implements the CAL specification Section 10.3 content
//! projection model.  It provides two pure functions:
//!
//! - [`humanize_relation`]: transforms an OMS relation string into
//!   human-readable text (e.g. `"mg:likes"` -> `"likes"`).
//! - [`humanize_time`]: transforms a Unix epoch timestamp into a relative
//!   time string (e.g. `"5m ago"`, `"3d ago"`, `"2025-06-15"`).
//!
//! # Security invariant (S-08)
//!
//! Namespace prefixes are stripped during humanization.  The output must NOT
//! leak the namespace prefix to downstream consumers.  For example,
//! `"acme_internal:secret_field"` becomes `"secret field"` — the `acme_internal`
//! prefix is removed.  Callers must ensure that the namespace itself is not
//! sensitive information that would be lost by stripping.  If namespace
//! preservation is required, do not apply humanization to that field.

// ---------------------------------------------------------------------------
// Relation humanization
// ---------------------------------------------------------------------------

/// Transform an OMS relation string into human-readable text.
///
/// Rules (CAL spec Section 10.3.3):
/// 1. Strip namespace prefix: `"mg:likes"` -> `"likes"`.
/// 2. Replace underscores with spaces: `"similar_to"` -> `"similar to"`.
/// 3. Custom relations with any prefix: `"acme:coffee"` -> `"coffee"`.
///
/// # Security (S-08)
///
/// The namespace prefix is always stripped.  This is intentional: the
/// humanized output is for LLM consumption, not for programmatic use.
/// The raw relation string is always available in the grain's `fields` object.
pub fn humanize_relation(relation: &str) -> String {
    let stripped = match relation.find(':') {
        Some(pos) => &relation[pos + 1..],
        None => relation,
    };
    stripped.replace('_', " ")
}

// ---------------------------------------------------------------------------
// Time humanization
// ---------------------------------------------------------------------------

/// Humanize a Unix timestamp to a relative time string.
///
/// Uses the CAL specification's age brackets (Section 10.3.4):
///
/// | Age            | Output           |
/// |----------------|------------------|
/// | < 1 minute     | `"just now"`     |
/// | < 1 hour       | `"Xm ago"`       |
/// | < 24 hours     | `"Xh ago"`       |
/// | < 7 days       | `"Xd ago"`       |
/// | < 30 days      | `"~Xw ago"`      |
/// | >= 30 days     | `"YYYY-MM-DD"`   |
///
/// If `now_secs` < `epoch_secs` (future timestamp), returns `"in the future"`.
/// If `epoch_secs` is 0, returns `"unknown"`.
pub fn humanize_time(epoch_secs: i64, now_secs: i64) -> String {
    if epoch_secs == 0 {
        return "unknown".to_string();
    }

    if epoch_secs > now_secs {
        return "in the future".to_string();
    }

    let delta = (now_secs - epoch_secs) as u64;

    const MINUTE: u64 = 60;
    const HOUR: u64 = 3600;
    const DAY: u64 = 86400;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;

    if delta < MINUTE {
        "just now".to_string()
    } else if delta < HOUR {
        format!("{}m ago", delta / MINUTE)
    } else if delta < DAY {
        format!("{}h ago", delta / HOUR)
    } else if delta < WEEK {
        format!("{}d ago", delta / DAY)
    } else if delta < MONTH {
        format!("~{}w ago", delta / WEEK)
    } else {
        // Format as YYYY-MM-DD.
        epoch_to_date_string(epoch_secs)
    }
}

/// Convert a Unix epoch timestamp to a `YYYY-MM-DD` date string.
///
/// Uses a simple calendar computation without external dependencies.
/// Handles dates from 1970 onwards.
fn epoch_to_date_string(epoch_secs: i64) -> String {
    // Days since Unix epoch.
    let total_days = (epoch_secs / 86400) as i32;

    // Algorithm: convert day count to (year, month, day).
    // Based on the civil_from_days algorithm (Howard Hinnant).
    let z = total_days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}", year, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- humanize_relation -------------------------------------------------

    #[test]
    fn test_humanize_mg_prefix() {
        assert_eq!(humanize_relation("mg:likes"), "likes");
        assert_eq!(humanize_relation("mg:knows"), "knows");
        assert_eq!(humanize_relation("mg:agrees_with"), "agrees with");
        assert_eq!(humanize_relation("mg:has_capability"), "has capability");
    }

    #[test]
    fn test_humanize_custom_prefix() {
        assert_eq!(humanize_relation("acme:similar_to"), "similar to");
        assert_eq!(humanize_relation("hc:patient_id"), "patient id");
    }

    #[test]
    fn test_humanize_no_prefix() {
        assert_eq!(humanize_relation("prefers"), "prefers");
        assert_eq!(humanize_relation("similar_to"), "similar to");
        assert_eq!(humanize_relation("works_at"), "works at");
    }

    #[test]
    fn test_humanize_no_underscores() {
        assert_eq!(humanize_relation("mg:likes"), "likes");
        assert_eq!(humanize_relation("knows"), "knows");
    }

    #[test]
    fn test_humanize_empty() {
        assert_eq!(humanize_relation(""), "");
    }

    #[test]
    fn test_humanize_colon_only() {
        assert_eq!(humanize_relation(":"), "");
    }

    #[test]
    fn test_humanize_multiple_colons() {
        // Only the first colon is treated as namespace separator.
        assert_eq!(humanize_relation("mg:foo:bar"), "foo:bar");
    }

    // -- humanize_time -----------------------------------------------------

    #[test]
    fn test_humanize_time_just_now() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now, now), "just now");
        assert_eq!(humanize_time(now - 30, now), "just now");
        assert_eq!(humanize_time(now - 59, now), "just now");
    }

    #[test]
    fn test_humanize_time_minutes_ago() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now - 60, now), "1m ago");
        assert_eq!(humanize_time(now - 300, now), "5m ago");
        assert_eq!(humanize_time(now - 3599, now), "59m ago");
    }

    #[test]
    fn test_humanize_time_hours_ago() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now - 3600, now), "1h ago");
        assert_eq!(humanize_time(now - 7200, now), "2h ago");
        assert_eq!(humanize_time(now - 86399, now), "23h ago");
    }

    #[test]
    fn test_humanize_time_days_ago() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now - 86400, now), "1d ago");
        assert_eq!(humanize_time(now - 3 * 86400, now), "3d ago");
        assert_eq!(humanize_time(now - 6 * 86400, now), "6d ago");
    }

    #[test]
    fn test_humanize_time_weeks_ago() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now - 7 * 86400, now), "~1w ago");
        assert_eq!(humanize_time(now - 14 * 86400, now), "~2w ago");
        assert_eq!(humanize_time(now - 29 * 86400, now), "~4w ago");
    }

    #[test]
    fn test_humanize_time_old_date() {
        let now = 1_700_000_000; // 2023-11-14
                                 // 100 days ago -> should produce a date string.
        let epoch = now - 100 * 86400;
        let result = humanize_time(epoch, now);
        assert!(result.starts_with("2023-"));
        assert!(result.len() == 10); // YYYY-MM-DD
    }

    #[test]
    fn test_humanize_time_zero() {
        assert_eq!(humanize_time(0, 1_700_000_000), "unknown");
    }

    #[test]
    fn test_humanize_time_future() {
        let now = 1_700_000_000;
        assert_eq!(humanize_time(now + 1000, now), "in the future");
    }

    #[test]
    fn test_epoch_to_date_known_value() {
        // 2023-11-14 22:13:20 UTC = 1700000000
        let date = epoch_to_date_string(1_700_000_000);
        assert_eq!(date, "2023-11-14");
    }

    #[test]
    fn test_epoch_to_date_unix_epoch() {
        assert_eq!(epoch_to_date_string(0), "1970-01-01");
    }

    #[test]
    fn test_epoch_to_date_y2k() {
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(epoch_to_date_string(946_684_800), "2000-01-01");
    }
}
