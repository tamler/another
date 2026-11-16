//! Herdr-compatible pane status (D70).
//!
//! Inside a Herdr pane (`HERDR_ENV=1` with `HERDR_PANE_ID` and `HERDR_BIN_PATH` set), the
//! hooks report the agent's Another state as display-only pane metadata:
//!
//! ```text
//! "$HERDR_BIN_PATH" pane report-metadata "$HERDR_PANE_ID" --source another \
//!   --token another_state=blocked --token "another=blocked: src/a.rs is held by alice" \
//!   --seq <microseconds> --ttl-ms 1800000
//! ```
//!
//! Another's hooks run inside a client's pane (Claude Code, Codex, ...) that Herdr's own
//! integration already owns, so they never call `report-agent` or `release-agent`: that
//! would take over the pane's semantic state and session restore. Herdr's documentation
//! asks hooks that run next to a Herdr integration to use metadata instead. The tokens
//! show up in `herdr pane get` and `herdr agent list` and can be rendered in the sidebar as
//! `$another_state` and `$another`.
//!
//! Reports run in the background and their failures are ignored. The latest state per pane
//! is cached under `$ANOTHER_HOME/hooks/state/`, so an unchanged state is not sent again
//! (only refreshed before its tokens expire) and every report's `--seq` is higher than the
//! last one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use another_core::{AgentState, AgentStatus};
use serde::{Deserialize, Serialize};

/// The `--source` of every report.
pub const SOURCE: &str = "another";
/// Token with the agent's state: `idle`, `working` or `blocked`.
pub const STATE_TOKEN: &str = "another_state";
/// Token with the reason the agent is blocked; cleared otherwise.
pub const NOTE_TOKEN: &str = "another";
/// Tokens expire after this long unless refreshed.
pub const TOKEN_TTL_MS: u64 = 30 * 60 * 1000;
/// An unchanged state is reported again after this long, before its tokens expire.
const REFRESH_MS: u64 = 10 * 60 * 1000;

/// The Herdr pane this process runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub bin: PathBuf,
    pub id: String,
}

impl Pane {
    /// `Some` only inside Herdr: `HERDR_ENV=1` and non-empty `HERDR_PANE_ID` and
    /// `HERDR_BIN_PATH`.
    pub fn from_env() -> Option<Pane> {
        Self::from_vars(|k| std::env::var_os(k))
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Option<Pane> {
        if var("HERDR_ENV")? != "1" {
            return None;
        }
        let id = var("HERDR_PANE_ID")?.into_string().ok()?;
        let bin = PathBuf::from(var("HERDR_BIN_PATH")?);
        if id.trim().is_empty() || bin.as_os_str().is_empty() {
            return None;
        }
        Some(Pane { bin, id })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Last {
    state: Option<String>,
    note: Option<String>,
    seq: u64,
    at_ms: u64,
}

/// What to send: the state token and the blocked note, or `None` to clear both.
fn tokens(status: Option<&AgentStatus>) -> (Option<&'static str>, Option<String>) {
    match status {
        Some(s) if s.state != AgentState::Unknown => (
            Some(s.state.as_str()),
            s.message
                .as_deref()
                .filter(|_| s.state == AgentState::Blocked)
                .map(|m| format!("blocked: {m}")),
        ),
        _ => (None, None),
    }
}

/// The arguments of one report.
fn args(pane: &str, state: Option<&str>, note: Option<&str>, seq: u64) -> Vec<String> {
    let mut a: Vec<String> = ["pane", "report-metadata", pane, "--source", SOURCE]
        .iter()
        .map(|s| s.to_string())
        .collect();
    match state {
        Some(state) => {
            a.push("--token".into());
            a.push(format!("{STATE_TOKEN}={state}"));
            match note {
                Some(note) => {
                    a.push("--token".into());
                    a.push(format!("{NOTE_TOKEN}={note}"));
                }
                None => {
                    a.push("--clear-token".into());
                    a.push(NOTE_TOKEN.into());
                }
            }
        }
        None => {
            for token in [STATE_TOKEN, NOTE_TOKEN] {
                a.push("--clear-token".into());
                a.push(token.into());
            }
        }
    }
    a.push("--seq".into());
    a.push(seq.to_string());
    if state.is_some() {
        a.push("--ttl-ms".into());
        a.push(TOKEN_TTL_MS.to_string());
    }
    a
}

fn cache_path(another_home: &Path, pane: &str) -> PathBuf {
    let mut safe = String::from("herdr-");
    for b in pane.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            safe.push(b as char);
        } else {
            safe.push_str(&format!("_{b:02x}"));
        }
    }
    another_home
        .join("hooks")
        .join("state")
        .join(format!("{safe}.json"))
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Report `status` for `pane` (`None`, or an `unknown` state, clears the tokens). Returns
/// the arguments sent, or `None` when nothing needed sending. Never fails: a missing or
/// failing `herdr` is ignored.
pub fn report(
    another_home: &Path,
    pane: &Pane,
    status: Option<&AgentStatus>,
) -> Option<Vec<String>> {
    let (state, note) = tokens(status);
    let path = cache_path(another_home, &pane.id);
    let last: Last = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now_us = now_micros();
    let now_ms = now_us / 1000;
    let unchanged = last.state.as_deref() == state && last.note == note;
    if unchanged && (state.is_none() || now_ms.saturating_sub(last.at_ms) < REFRESH_MS) {
        return None;
    }
    let seq = now_us.max(last.seq + 1);
    // Record first, so parallel hooks in the same pane do not all send.
    let next = Last {
        state: state.map(str::to_string),
        note: note.clone(),
        seq,
        at_ms: now_ms,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&next) {
        let _ = std::fs::write(&path, bytes);
    }
    let argv = args(&pane.id, state, note.as_deref(), seq);
    // In the background: the hook does not wait for Herdr, and Herdr's answer is unused.
    let spawned = Command::new(&pane.bin)
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "herdr status report skipped");
    }
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn only_a_complete_herdr_environment_is_a_pane() {
        let full = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_BIN_PATH", "/opt/herdr"),
        ];
        assert_eq!(
            Pane::from_vars(vars(&full)),
            Some(Pane {
                bin: PathBuf::from("/opt/herdr"),
                id: "w1:p1".into()
            })
        );
        for missing in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH"] {
            let some: Vec<_> = full
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            assert_eq!(Pane::from_vars(vars(&some)), None, "without {missing}");
        }
        for (k, v) in [
            ("HERDR_ENV", "0"),
            ("HERDR_PANE_ID", ""),
            ("HERDR_BIN_PATH", ""),
        ] {
            let mut some: Vec<_> = full.iter().copied().filter(|(n, _)| *n != k).collect();
            some.push((k, v));
            assert_eq!(Pane::from_vars(vars(&some)), None, "{k}={v:?}");
        }
    }

    fn status(state: AgentState, message: Option<&str>) -> AgentStatus {
        AgentStatus {
            state,
            message: message.map(str::to_string),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn states_map_to_tokens_and_unknown_clears_them() {
        let blocked = status(AgentState::Blocked, Some("src/a.rs is held by alice"));
        assert_eq!(
            args(
                "w1:p1",
                tokens(Some(&blocked)).0,
                tokens(Some(&blocked)).1.as_deref(),
                7
            ),
            [
                "pane",
                "report-metadata",
                "w1:p1",
                "--source",
                "another",
                "--token",
                "another_state=blocked",
                "--token",
                "another=blocked: src/a.rs is held by alice",
                "--seq",
                "7",
                "--ttl-ms",
                "1800000"
            ]
        );
        let working = status(AgentState::Working, None);
        let (s, n) = tokens(Some(&working));
        assert_eq!((s, n.clone()), (Some("working"), None));
        assert!(args("p", s, n.as_deref(), 1).contains(&"--clear-token".to_string()));
        for gone in [None, Some(status(AgentState::Unknown, None))] {
            let (s, n) = tokens(gone.as_ref());
            assert_eq!((s, n.clone()), (None, None));
            let a = args("p", s, n.as_deref(), 1);
            assert_eq!(
                a[5..],
                [
                    "--clear-token",
                    "another_state",
                    "--clear-token",
                    "another",
                    "--seq",
                    "1"
                ]
            );
        }
    }

    #[test]
    fn reports_coalesce_and_sequence_numbers_only_grow() {
        let dir = tempfile::tempdir().unwrap();
        let pane = Pane {
            bin: dir.path().join("no-such-herdr"),
            id: "w1:p1".into(),
        };
        let working = status(AgentState::Working, None);
        let blocked = status(AgentState::Blocked, Some("x is held by y"));
        let first = report(dir.path(), &pane, Some(&working)).expect("first report is sent");
        assert!(
            report(dir.path(), &pane, Some(&working)).is_none(),
            "unchanged"
        );
        let second = report(dir.path(), &pane, Some(&blocked)).expect("a change is sent");
        let cleared = report(dir.path(), &pane, None).expect("clearing is sent");
        assert!(report(dir.path(), &pane, None).is_none(), "already clear");
        let seq = |a: &[String]| -> u64 {
            let i = a.iter().position(|x| x == "--seq").unwrap();
            a[i + 1].parse().unwrap()
        };
        assert!(seq(&first) < seq(&second) && seq(&second) < seq(&cleared));

        // A cached sequence number from a clock that ran ahead still only grows.
        let path = cache_path(dir.path(), &pane.id);
        let ahead = Last {
            state: None,
            note: None,
            seq: u64::MAX / 2,
            at_ms: 0,
        };
        std::fs::write(&path, serde_json::to_vec(&ahead).unwrap()).unwrap();
        let next = report(dir.path(), &pane, Some(&working)).unwrap();
        assert_eq!(seq(&next), u64::MAX / 2 + 1);
    }

    #[test]
    fn a_pane_id_is_a_safe_file_name() {
        let p = cache_path(Path::new("/h"), "w1:p1/../x");
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            "herdr-w1_3ap1_2f_2e_2e_2fx.json"
        );
    }
}
