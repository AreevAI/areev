//! Timestamp parsing — hand-rolled on purpose (invariant 6: the workspace
//! takes no datetime dependency).
//!
//! This lives in `areev-core` rather than in the crate that first needed it
//! (`areev-store`'s importers) because the credential map's `expires_at`
//! needs the same parse, and `authz` sits below the store. One parser, not
//! two: a second implementation would drift on exactly the inputs that
//! matter least often and cost most — leap years, offsets, fractional
//! seconds.

/// Days from 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse an ISO-8601 timestamp ("2024-07-26T10:29:11.982509-07:00",
/// "2026-01-05 09:00:00Z", "2024-07-26") to epoch milliseconds.
pub fn iso8601_to_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    let mut i = 10;
    if b.len() > i && (b[i] == b'T' || b[i] == b' ') {
        i += 1;
        if b.len() < i + 8 || b[i + 2] != b':' || b[i + 5] != b':' {
            return None;
        }
        let (h, mi, sec) = (num(i..i + 2)?, num(i + 3..i + 5)?, num(i + 6..i + 8)?);
        ms += (h * 3600 + mi * 60 + sec) * 1000;
        i += 8;
        // fractional seconds: keep milliseconds, skip the rest
        if b.len() > i && b[i] == b'.' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let frac = &s[start..i];
            if !frac.is_empty() {
                let ms_str: String = frac.chars().chain("000".chars()).take(3).collect();
                ms += ms_str.parse::<i64>().ok()?;
            }
        }
        // offset: Z | ±HH:MM | ±HHMM | ±HH
        if b.len() > i {
            match b[i] {
                b'Z' | b'z' => {}
                b'+' | b'-' => {
                    let sign: i64 = if b[i] == b'+' { 1 } else { -1 };
                    i += 1;
                    let oh = num(i..i + 2)?;
                    i += 2;
                    if b.len() > i && b[i] == b':' {
                        i += 1;
                    }
                    let om = if b.len() >= i + 2 { num(i..i + 2).unwrap_or(0) } else { 0 };
                    ms -= sign * (oh * 3600 + om * 60) * 1000;
                }
                _ => return None,
            }
        }
    }
    Some(ms)
}

/// Wall-clock epoch milliseconds.
///
/// Kept beside the parser so the few host-plane callers that legitimately
/// need "now" (credential expiry, session lifetimes) share one spelling. The
/// grain path deliberately does NOT use this — `audit_observation` and the
/// serializers take `now_ms` as a parameter so content addresses stay
/// reproducible in tests.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_parses_common_shapes() {
        assert_eq!(iso8601_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso8601_to_ms("2024-01-01"), Some(1_704_067_200_000));
        assert_eq!(iso8601_to_ms("2024-01-01T00:00:00Z"), Some(1_704_067_200_000));
        assert_eq!(iso8601_to_ms("2024-01-01 00:00:00"), Some(1_704_067_200_000));
        assert_eq!(iso8601_to_ms("2024-01-01T00:00:00.500Z"), Some(1_704_067_200_500));
        assert_eq!(iso8601_to_ms("2023-12-31T17:00:00-07:00"), Some(1_704_067_200_000));
        assert_eq!(iso8601_to_ms("2024-01-01T01:00:00+01:00"), Some(1_704_067_200_000));
        assert_eq!(iso8601_to_ms("2024-01-01T00:00:00.982509Z"), Some(1_704_067_200_982));
        assert_eq!(iso8601_to_ms("not a date"), None);
        assert_eq!(iso8601_to_ms("2024-13-01"), None);
    }

    /// The leap-year path the two-implementation risk lived on.
    #[test]
    fn leap_days_and_century_rules() {
        // 2024 is a leap year: Feb 29 exists and March 1 is the next day.
        let feb29 = iso8601_to_ms("2024-02-29T00:00:00Z").unwrap();
        let mar01 = iso8601_to_ms("2024-03-01T00:00:00Z").unwrap();
        assert_eq!(mar01 - feb29, 86_400_000);
        // 2000 was a leap year (divisible by 400); 1900 was not.
        assert_eq!(
            iso8601_to_ms("2000-03-01T00:00:00Z").unwrap()
                - iso8601_to_ms("2000-02-28T00:00:00Z").unwrap(),
            2 * 86_400_000
        );
    }
}
