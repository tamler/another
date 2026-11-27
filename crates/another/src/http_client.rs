use std::time::Duration;

use ureq::config::{AutoHeaderValue, Config};

pub(crate) enum HttpClient {
    Federation { direct: bool },
    Doctor,
    Daemon,
    Cimd { https_only: bool },
}

impl HttpClient {
    pub(crate) fn config(self) -> Config {
        let (seconds, direct, https_only) = match self {
            Self::Federation { direct } => (30, direct, false),
            Self::Doctor | Self::Daemon => (2, true, false),
            Self::Cimd { https_only } => (5, false, https_only),
        };
        let user_agent = match self {
            Self::Cimd { .. } => AutoHeaderValue::from("another-oauth"),
            _ => AutoHeaderValue::Default,
        };
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(seconds)))
            .user_agent(user_agent)
            .https_only(https_only);
        if !matches!(self, Self::Daemon) {
            config = config.max_redirects(0);
        }
        if direct {
            config = config.proxy(None);
        }
        config.build()
    }
}
