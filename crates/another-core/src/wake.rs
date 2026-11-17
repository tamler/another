//! Wake targets: where to wake an agent's session when it has mail.
//! Another only stores the transport's name and the target; the CLI runs the transport.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::identity::resolve_agent;
use crate::store::Store;
use crate::time::fmt_ts;

/// The name of a transport: `herdr`, or one the owner configured on this node. Whether a
/// name is configured is the CLI's question; this type only holds a well-formed name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct WakeVia(String);

/// Whether `name` can name a transport: a lowercase letter, then up to 31 lowercase
/// letters, digits, `_` or `-`.
pub fn valid_transport_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl WakeVia {
    pub fn herdr() -> WakeVia {
        WakeVia("herdr".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for WakeVia {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        if !valid_transport_name(s) {
            return Err(Error::Invalid(format!(
                "{s:?} cannot name a transport: a name is a lowercase letter, then up to 31 \
                 lowercase letters, digits, `_` or `-`"
            )));
        }
        Ok(WakeVia(s.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WakeTarget {
    pub agent: String,
    pub via: WakeVia,
    /// Where that transport delivers a line: a Herdr pane or agent name, a tmux pane
    /// (`%3`, `session:1.0`), a cmux surface id.
    pub target: String,
    /// True when captured automatically by a session hook.
    pub auto: bool,
    pub updated_at: String,
}

/// The rule for every target: 1 to 200 printable characters, not starting with `-`.
pub fn validate_target(target: &str) -> Result<()> {
    if target.trim().is_empty()
        || target.chars().count() > 200
        || target.chars().any(char::is_control)
    {
        return Err(Error::Invalid(
            "wake target must be 1-200 printable characters".into(),
        ));
    }
    refuse_option_like_target(target)
}

/// Transports pass the target as a command-line argument, and Herdr takes it positionally,
/// so a target that starts with `-` would be read as an option. No tmux pane, cmux surface
/// or Herdr pane starts with one.
pub fn refuse_option_like_target(target: &str) -> Result<()> {
    if target.starts_with('-') {
        return Err(Error::Invalid(format!(
            "wake target {target:?} starts with '-', which an adapter would read as an option"
        )));
    }
    Ok(())
}

impl Store {
    /// Set where to wake an agent. Automatic captures never replace a manual setting.
    pub fn set_wake_target(
        &self,
        agent: &str,
        via: &WakeVia,
        target: &str,
        auto: bool,
    ) -> Result<Option<WakeTarget>> {
        validate_target(target)?;
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let manual_exists = tx
                .query_row(
                    "SELECT 1 FROM wake_targets WHERE agent_id = ?1 AND auto = 0",
                    [&me.id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if auto && manual_exists {
                return Ok(None);
            }
            tx.execute(
                "INSERT INTO wake_targets (agent_id, via, target, auto, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent_id) DO UPDATE SET via = excluded.via, target = excluded.target,
                   auto = excluded.auto, updated_at = excluded.updated_at",
                params![me.id, via.as_str(), target, auto, tx.now],
            )?;
            Ok(Some(WakeTarget {
                agent: me.address,
                via: via.clone(),
                target: target.to_string(),
                auto,
                updated_at: fmt_ts(tx.now),
            }))
        })
    }

    pub fn clear_wake_target(&self, agent: &str) -> Result<bool> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            Ok(tx.execute("DELETE FROM wake_targets WHERE agent_id = ?1", [&me.id])? > 0)
        })
    }

    /// Forget an automatically captured target (never a manual one). With `only`, forget it
    /// only while it still names that transport and target, so a session ending in one pane
    /// does not drop the capture a newer session made elsewhere.
    pub fn clear_auto_wake_target(&self, agent: &str, only: Option<(&str, &str)>) -> Result<bool> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let (via, target) = only.map_or((None, None), |(v, t)| (Some(v), Some(t)));
            Ok(tx.execute(
                "DELETE FROM wake_targets WHERE agent_id = ?1 AND auto = 1
                   AND (?2 IS NULL OR (via = ?2 AND target = ?3))",
                params![me.id, via, target],
            )? > 0)
        })
    }

    pub fn wake_target(&self, agent: &str) -> Result<Option<WakeTarget>> {
        Ok(self
            .wake_targets()?
            .into_iter()
            .find(|t| t.agent == self.agent(agent).map(|a| a.address).unwrap_or_default()))
    }

    pub fn wake_targets(&self) -> Result<Vec<WakeTarget>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT a.address, w.via, w.target, w.auto, w.updated_at FROM wake_targets w
                 JOIN agents a ON a.agent_id = w.agent_id ORDER BY a.address",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(WakeTarget {
                    agent: r.get(0)?,
                    // As stored, with no guess: a name nothing provides is reported by
                    // `another wake` and `another doctor`.
                    via: WakeVia(r.get(1)?),
                    target: r.get(2)?,
                    auto: r.get(3)?,
                    updated_at: fmt_ts(r.get(4)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}
