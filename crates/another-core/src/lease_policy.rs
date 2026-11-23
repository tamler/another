//! The duration policy for every temporary right. Maintenance and security deadlines
//! do not use this policy because they do not grant ownership.

use crate::{Error, Result};

pub const DEFAULT_TTL_SECONDS: i64 = 120;
pub const MIN_TTL_SECONDS: i64 = 30;
pub const MAX_TTL_SECONDS: i64 = 24 * 60 * 60;

/// Select a checkout duration once. Renewal always uses the stored selection.
pub fn checkout_ttl(requested: Option<i64>) -> Result<i64> {
    let seconds = requested.unwrap_or(DEFAULT_TTL_SECONDS);
    if seconds <= 0 {
        return Err(Error::Invalid("ttl_seconds must be positive".into()));
    }
    Ok(seconds.clamp(MIN_TTL_SECONDS, MAX_TTL_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_requested_and_shared_bounds() {
        assert_eq!(checkout_ttl(None).unwrap(), 120);
        assert_eq!(checkout_ttl(Some(600)).unwrap(), 600);
        assert_eq!(checkout_ttl(Some(1)).unwrap(), 30);
        assert_eq!(checkout_ttl(Some(i64::MAX)).unwrap(), 86_400);
        assert!(checkout_ttl(Some(0)).is_err());
        assert!(checkout_ttl(Some(-1)).is_err());
    }
}
