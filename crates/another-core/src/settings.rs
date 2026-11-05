//! Node settings: small, local configuration stored in the node database (never federated,
//! never set remotely). `another config [KEY [VALUE]]`.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::store::Store;

/// A known setting: key, default, what it means.
pub struct SettingSpec {
    pub key: &'static str,
    pub default: &'static str,
    pub help: &'static str,
}

/// Settings this build understands.
pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        key: "decider",
        default: crate::decisions::DEFAULT_DECIDER,
        help: "who is asked when a decision (or a spend approval) names nobody",
    },
    SettingSpec {
        key: "remote_idle_hours",
        default: "4",
        help: "hours an ephemeral remote agent may go unseen before it is removed (1-720)",
    },
    SettingSpec {
        key: crate::webhooks::REMOTE_PRIVATE_HOSTS,
        default: "off",
        help: "on = webhooks a token creates may target this machine and link-local or \
               metadata addresses; off refuses them (on or off)",
    },
    SettingSpec {
        key: "summary_daily",
        default: "",
        help: "local time (HH:MM) `another serve` sends the daily summary; empty = off",
    },
    SettingSpec {
        key: "summary_to",
        default: "webhook",
        help: "where the scheduled summary goes: webhook (daily_summary filter) or mail:NAME",
    },
];

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Setting {
    pub key: String,
    pub value: String,
    /// True when no value is stored and the default applies.
    pub default: bool,
    pub help: String,
}

fn spec(key: &str) -> Result<&'static SettingSpec> {
    SETTINGS.iter().find(|s| s.key == key).ok_or_else(|| {
        Error::Invalid(format!(
            "unknown setting {key:?}; known: {}",
            SETTINGS
                .iter()
                .map(|s| s.key)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// The stored value of `key`, or its default.
pub(crate) fn get(conn: &Connection, key: &str) -> Result<String> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM node_settings WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(stored.unwrap_or_else(|| spec(key).map(|s| s.default.to_string()).unwrap_or_default()))
}

fn validate(key: &str, value: &str) -> Result<()> {
    match key {
        "decider" => crate::identity::validate_name(value),
        "remote_idle_hours" => match value.parse::<u32>() {
            Ok(h) if (1..=720).contains(&h) => Ok(()),
            _ => Err(Error::Invalid(
                "remote_idle_hours must be a whole number of hours, 1-720".into(),
            )),
        },
        crate::webhooks::REMOTE_PRIVATE_HOSTS => match value {
            "on" | "off" => Ok(()),
            _ => Err(Error::Invalid(format!("{key} must be on or off"))),
        },
        "summary_daily" => {
            if value.is_empty() {
                Ok(())
            } else {
                crate::summary::parse_hhmm(value).map(|_| ())
            }
        }
        "summary_to" => match value.split_once(':') {
            _ if value == "webhook" => Ok(()),
            Some(("mail", name)) => crate::identity::validate_name(name),
            _ => Err(Error::Invalid(format!(
                "summary_to must be webhook or mail:NAME, not {value:?}"
            ))),
        },
        _ => Ok(()),
    }
}

impl Store {
    pub fn settings(&self) -> Result<Vec<Setting>> {
        self.read(|conn| {
            SETTINGS
                .iter()
                .map(|s| {
                    let stored: Option<String> = conn
                        .query_row(
                            "SELECT value FROM node_settings WHERE key = ?1",
                            [s.key],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok(Setting {
                        key: s.key.into(),
                        default: stored.is_none(),
                        value: stored.unwrap_or_else(|| s.default.into()),
                        help: s.help.into(),
                    })
                })
                .collect()
        })
    }

    pub fn setting(&self, key: &str) -> Result<Setting> {
        spec(key)?;
        self.settings()?
            .into_iter()
            .find(|s| s.key == key)
            .ok_or_else(|| Error::Invalid(format!("unknown setting {key:?}")))
    }

    /// Store a setting (local only; recorded as a node-local event).
    pub fn set_setting(&self, key: &str, value: &str) -> Result<Setting> {
        spec(key)?;
        let value = value.trim();
        validate(key, value)?;
        self.write(|tx| {
            tx.execute(
                "INSERT INTO node_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![key, value, tx.now],
            )?;
            tx.emit("local", "node.setting_changed", serde_json::json!({ "key": key, "value": value }))?;
            Ok(())
        })?;
        self.setting(key)
    }
}
