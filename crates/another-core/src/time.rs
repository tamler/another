//! Timestamps are stored as integer milliseconds since the Unix epoch (UTC) and rendered
//! as RFC 3339 strings at the API boundary.

use chrono::{DateTime, SecondsFormat};

use crate::error::{Error, Result};

/// `now` plus `secs` seconds, in milliseconds. Refuses a result that overflows or that
/// cannot be shown as a date, instead of wrapping around.
pub(crate) fn after_secs(now: i64, secs: i64, what: &str) -> Result<i64> {
    secs.checked_mul(1000)
        .and_then(|ms| now.checked_add(ms))
        .filter(|&t| DateTime::from_timestamp_millis(t).is_some())
        .ok_or_else(|| Error::Invalid(format!("{what} of {secs} seconds is too far ahead")))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn fmt_ts(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

pub fn fmt_opt(ms: Option<i64>) -> Option<String> {
    ms.map(fmt_ts)
}
