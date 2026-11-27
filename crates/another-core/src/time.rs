//! Timestamps are stored as integer milliseconds since the Unix epoch (UTC) and rendered
//! as RFC 3339 strings at the API boundary.

use chrono::{DateTime, SecondsFormat};

use crate::error::{Error, Result};

/// `now` plus `secs` seconds, in milliseconds. Refuses a result that overflows or that
/// cannot be shown as a date, instead of wrapping around.
pub(crate) fn later_secs(now: i64, secs: i64, what: &str) -> Result<i64> {
    secs.checked_mul(1000)
        .and_then(|ms| now.checked_add(ms))
        .filter(|&t| DateTime::from_timestamp_millis(t).is_some())
        .ok_or_else(|| Error::Invalid(format!("{what} is too far in the future")))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
    DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.to_rfc3339_opts(SecondsFormat::Millis, true),
    )
}

/// Milliseconds since the epoch for an RFC 3339 time such as [`fmt_ts`] produces.
pub fn parse_ts(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

pub fn fmt_opt(ms: Option<i64>) -> Option<String> {
    ms.map(fmt_ts)
}
