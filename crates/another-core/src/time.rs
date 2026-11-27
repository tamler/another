//! Timestamps are stored as integer milliseconds since the Unix epoch (UTC) and rendered
//! as RFC 3339 strings at the API boundary.

use jiff::Timestamp;

use crate::error::{Error, Result};

/// `now` plus `secs` seconds, in milliseconds. Refuses a result that overflows or that
/// cannot be shown as a date, instead of wrapping around.
pub(crate) fn later_secs(now: i64, secs: i64, what: &str) -> Result<i64> {
    secs.checked_mul(1000)
        .and_then(|ms| now.checked_add(ms))
        .filter(|&t| Timestamp::from_millisecond(t).is_ok())
        .ok_or_else(|| Error::Invalid(format!("{what} is too far in the future")))
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// Parse a nonnegative duration in seconds, with an `s`, `m`, `h`, `d` or `w`
/// suffix. Callers choose whether a bare number is also accepted as seconds.
pub fn parse_duration(raw: &str, allow_bare_seconds: bool) -> Result<i64> {
    let raw = raw.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid duration {raw:?}")))?;
    let mult = match unit {
        "" if allow_bare_seconds => 1,
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(Error::Invalid(format!(
                "invalid duration {raw:?}: use s, m, h, d or w"
            )))
        }
    };
    n.checked_mul(mult)
        .ok_or_else(|| Error::Invalid(format!("invalid duration {raw:?}: too large")))
}

pub fn fmt_ts(ms: i64) -> String {
    Timestamp::from_millisecond(ms).map_or_else(|_| ms.to_string(), |t| format!("{t:.3}"))
}

/// Milliseconds since the epoch for an RFC 3339 time such as [`fmt_ts`] produces.
pub fn parse_ts(text: &str) -> Option<i64> {
    text.trim()
        .parse::<Timestamp>()
        .ok()
        .map(Timestamp::as_millisecond)
}

pub fn fmt_opt(ms: Option<i64>) -> Option<String> {
    ms.map(fmt_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_milliseconds_as_utc_rfc3339() {
        assert_eq!(fmt_ts(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(fmt_ts(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(fmt_ts(1_795_766_400_123), "2026-11-27T08:00:00.123Z");
        assert_eq!(fmt_opt(None), None);
    }

    #[test]
    fn parses_the_rfc3339_forms_it_accepted_before() {
        let at = 1_795_766_400_000;
        assert_eq!(parse_ts("2026-11-27T08:00:00Z"), Some(at));
        assert_eq!(parse_ts("  2026-11-27T08:00:00.000Z\n"), Some(at));
        assert_eq!(parse_ts("2026-11-27T08:00:00.5Z"), Some(at + 500));
        assert_eq!(parse_ts("2026-11-27T16:00:00+08:00"), Some(at));
        assert_eq!(parse_ts("2026-11-27T08:00:00-00:00"), Some(at));
        assert_eq!(parse_ts("2026-11-27t08:00:00z"), Some(at));
        assert_eq!(parse_ts("2026-11-27 08:00:00Z"), Some(at));
        assert_eq!(parse_ts(&fmt_ts(at + 7)), Some(at + 7));
    }

    #[test]
    fn refuses_times_without_an_offset() {
        assert_eq!(parse_ts("2026-11-27T08:00:00"), None);
        assert_eq!(parse_ts("2026-11-27"), None);
        assert_eq!(parse_ts("not a time"), None);
        assert_eq!(parse_ts(""), None);
    }

    #[test]
    fn later_secs_refuses_overflow() {
        assert_eq!(later_secs(1_000, 2, "x").unwrap(), 3_000);
        assert!(later_secs(0, i64::MAX, "x").is_err());
        assert!(later_secs(i64::MAX - 10, 1, "x").is_err());
    }
}
