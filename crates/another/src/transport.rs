//! The transport interface: how Another talks to a tool that runs agent sessions.
//!
//! Three operations: `wake` (deliver one line, and say whether work started), `status`
//! (what is at one target) and `list` (every session the transport can see). No operation
//! returns a Rust error: every failure is a value. Another never reads pane output and
//! never sends keys; that stays inside the transport.

use std::sync::atomic::AtomicBool;

use another_core::Store;
use anyhow::{bail, Result};
use serde::Serialize;

pub use another_core::{Observation, Session, TransportState as State, Tri, WakeOutcome};

pub mod command;
pub mod herdr;

pub use command::{load_config, Config};

/// How one wake call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeResult {
    pub outcome: WakeOutcome,
    /// One line: what the transport answered.
    pub detail: String,
}

impl WakeResult {
    pub fn new(outcome: WakeOutcome, detail: impl Into<String>) -> WakeResult {
        WakeResult {
            outcome,
            detail: another_core::single_line(&detail.into(), 300),
        }
    }

    pub fn delivered(&self) -> Tri {
        self.outcome.delivered()
    }

    pub fn started(&self) -> Tri {
        self.outcome.started()
    }
}

/// What `list` answered. An empty list is a real answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListResult {
    Sessions(Vec<Session>),
    /// The transport has no way to list.
    Unsupported,
    /// The transport could not be asked.
    NoAnswer(String),
}

pub trait Transport: Send + Sync {
    fn name(&self) -> &str;
    /// Deliver `line` to `target` and report what followed.
    fn wake(&self, target: &str, line: &str, cancel: Option<&AtomicBool>) -> WakeResult;
    /// What is at `target` now.
    fn status(&self, target: &str, cancel: Option<&AtomicBool>) -> Observation;
    /// Every session the transport can see.
    fn list(&self, cancel: Option<&AtomicBool>) -> ListResult;
    /// The commands `wake` would run, for `--dry-run`. Runs nothing.
    fn wake_argv(&self, target: &str, line: &str) -> Vec<Vec<String>>;
}

/// The transport of that name: `herdr`, or a command transport of `config`. `rex` is
/// reserved and not in this build.
pub fn transport(name: &str, config: &Config) -> Result<Box<dyn Transport>> {
    if name == "herdr" {
        return Ok(Box::new(herdr::HerdrTransport::from_env()));
    }
    match config.get(name) {
        Some(spec) => Ok(Box::new(command::CommandTransport::new(spec.clone()))),
        None => bail!("{}", config.why_missing(name)),
    }
}

/// The transports config of the home `store` was opened in, read now. A store with no
/// home (in memory) has none.
pub fn config_for(store: &Store) -> Config {
    match store.db_path().and_then(|db| db.parent()) {
        Some(home) => load_config(home),
        None => Config::default(),
    }
}

/// A hook-made row not observed for this long is marked, and with no observation at all in
/// this long the report asks whether the daemon is running.
pub(crate) const STALE_AFTER_MS: i64 = 5 * 60_000;

/// One session row as `another transport status` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct StatusRow {
    pub agent: String,
    pub session_id: String,
    pub transport: String,
    pub target: String,
    /// What the owner is shown: `stuck` after a wake that did not start work, else the
    /// observed state, `not observed` before the first check.
    pub state: String,
    /// What the transport last reported, and its one-line detail.
    pub observed_state: Option<String>,
    pub detail: Option<String>,
    pub state_since: Option<String>,
    /// Whether a transport of that name exists now: `herdr`, or one in `transports.json`.
    pub transport_configured: bool,
    /// `session_id`, `weak` or `none`: how the last check recognised the session.
    pub matched: &'static str,
    /// A hook of the session made the row (false: a wake by hand did, to hold its outcome).
    pub hook_made: bool,
    /// Where the row's target came from: `hook`; or, for a row a wake by hand made,
    /// `hand_set` (a target stored with `another wake set`), `captured` (a pane a hook
    /// recorded as the agent's wake target) or `given` (`--target`, or the agent's name).
    pub origin: &'static str,
    pub hook_seen_at: Option<String>,
    pub observed_at: Option<String>,
    /// Not checked in the last 5 minutes.
    pub stale: bool,
    pub last_wake_outcome: Option<String>,
    pub last_wake_at: Option<String>,
    pub wake_stop: Option<String>,
    pub undelivered_count: i64,
    pub last_alive_at: Option<String>,
    pub gone_confirmed_at: Option<String>,
    /// Why the row needs the owner, if it does.
    pub attention: Option<String>,
    pub attention_since: Option<String>,
    pub hint: Option<String>,
    #[serde(skip)]
    for_ms: Option<i64>,
    #[serde(skip)]
    wake_ago_ms: Option<i64>,
    /// The stop still holds: no hook of the session ran after it
    /// ([`another_core::TransportSession::stopped`]). For `another needs`.
    #[serde(skip)]
    pub(crate) stop_holds: bool,
    /// The times `another needs` compares, in milliseconds.
    #[serde(skip)]
    pub(crate) hook_seen_ms: Option<i64>,
    #[serde(skip)]
    pub(crate) observed_ms: Option<i64>,
    #[serde(skip)]
    pub(crate) state_since_ms: Option<i64>,
    /// A trusted listing has matched the session by its id since the last untrusted one.
    #[serde(skip)]
    pub(crate) seen_live: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub now: String,
    /// Set when sessions or wake targets exist and nothing was checked in the last 5 minutes.
    pub warning: Option<String>,
    /// Set when `transports.json` was refused: no command transport exists until it is
    /// fixed.
    pub config_error: Option<String>,
    pub attention_only: bool,
    pub sessions: Vec<StatusRow>,
}

/// `another transport status`: read the session rows. Calls no transport.
pub fn status_report(store: &Store, attention_only: bool) -> Result<StatusReport> {
    let now = store.now();
    let config = config_for(store);
    let rows = store.transport_sessions()?;
    let observed_lately = rows
        .iter()
        .any(|r| r.observed_at.is_some_and(|at| now - at <= STALE_AFTER_MS));
    let targets = store.wake_targets()?;
    let anything = !rows.is_empty() || !targets.is_empty();
    let origin = |r: &another_core::TransportSession| {
        let stored = targets
            .iter()
            .find(|t| t.agent == r.agent && t.via.as_str() == r.transport && t.target == r.target);
        match stored {
            _ if r.hook_made() => "hook",
            Some(t) if t.auto => "captured",
            Some(_) => "hand_set",
            None => "given",
        }
    };
    let ts = |at: Option<i64>| at.map(another_core::fmt_ts);
    let sessions = rows
        .into_iter()
        .filter(|r| !attention_only || r.attention.is_some())
        .map(|r| {
            let state = another_core::shown_state(&r);
            let hint = (state == "stuck" && r.wake_stop.as_deref() == Some("not_started"))
                .then(|| crate::wake::INPUT_BOX_HINT.to_string());
            StatusRow {
                state,
                hint,
                matched: match r.matched {
                    1 => "session_id",
                    2 => "weak",
                    _ => "none",
                },
                hook_made: r.hook_made(),
                transport_configured: config.provides(&r.transport),
                origin: origin(&r),
                stale: r.hook_made() && r.observed_at.is_none_or(|at| now - at > STALE_AFTER_MS),
                for_ms: r.state_since.map(|at| now - at),
                wake_ago_ms: r.last_wake_at.map(|at| now - at),
                stop_holds: r.stopped(),
                hook_seen_ms: r.hook_seen_at,
                observed_ms: r.observed_at,
                state_since_ms: r.state_since,
                seen_live: r.seen_live_at.is_some(),
                state_since: ts(r.state_since),
                hook_seen_at: ts(r.hook_seen_at),
                observed_at: ts(r.observed_at),
                last_wake_at: ts(r.last_wake_at),
                last_alive_at: ts(r.last_alive_at),
                gone_confirmed_at: ts(r.gone_confirmed_at),
                attention_since: ts(r.attention_since),
                agent: r.agent,
                session_id: r.session_id,
                transport: r.transport,
                target: r.target,
                observed_state: r.state,
                detail: r.detail,
                last_wake_outcome: r.last_wake_outcome,
                wake_stop: r.wake_stop,
                undelivered_count: r.undelivered_count,
                attention: r.attention,
            }
        })
        .collect();
    Ok(StatusReport {
        now: another_core::fmt_ts(now),
        warning: (anything && !observed_lately).then(|| {
            "no transport check in the last 5 minutes; is the daemon running?".to_string()
        }),
        config_error: config.refusal(),
        attention_only,
        sessions,
    })
}

/// A span as the owner reads it: `45s`, `12m`, `3h`, `2d`.
pub(crate) fn span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The human form of [`StatusReport`]: one line per session row.
pub fn render_status(report: &StatusReport) -> String {
    let mut out = String::new();
    if let Some(warning) = &report.warning {
        out.push_str(warning);
        out.push('\n');
    }
    if let Some(error) = &report.config_error {
        out.push_str(error);
        out.push('\n');
    }
    if report.sessions.is_empty() {
        out.push_str(if report.attention_only {
            "no session needs attention"
        } else {
            "no transport sessions (a hook inside Herdr or a configured transport records one)"
        });
        return out;
    }
    for r in &report.sessions {
        let session = if r.session_id.is_empty() {
            "-".to_string()
        } else {
            let head: String = r.session_id.chars().take(4).collect();
            format!("{head}..")
        };
        let mut line = format!(
            "{} {session}  {} {}  {}",
            r.agent, r.transport, r.target, r.state
        );
        if let Some(ms) = r.for_ms {
            line.push_str(&format!(" {}", span(ms)));
        }
        if let (Some(outcome), Some(ago)) = (&r.last_wake_outcome, r.wake_ago_ms) {
            let meaning = WakeOutcome::parse(outcome).map_or(outcome.as_str(), |o| o.meaning());
            line.push_str(&format!("  last wake: {meaning} {} ago", span(ago)));
        }
        if r.matched == "weak" {
            line.push_str("  weak match");
        }
        if !r.transport_configured {
            line.push_str("  transport not configured");
        }
        if !r.hook_made {
            line.push_str(match r.origin {
                "hand_set" => "  target set by hand",
                "captured" => "  captured pane, woken by hand",
                _ => "  woken by hand",
            });
        } else if r.stale {
            match &r.observed_at {
                Some(at) => line.push_str(&format!("  not checked since {at}")),
                None => line.push_str("  not checked yet"),
            }
        }
        if let Some(reason) = &r.attention {
            line.push_str(&format!("  needs attention: {reason}"));
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(hint) = &r.hint {
            out.push_str(&format!("  {hint}\n"));
        }
    }
    out
}

/// An observation that carries a state and nothing else.
pub(crate) fn bare(state: State, detail: impl Into<String>) -> Observation {
    Observation {
        state,
        session_id: None,
        program: None,
        cwd: None,
        detail: another_core::single_line(&detail.into(), 300),
    }
}
