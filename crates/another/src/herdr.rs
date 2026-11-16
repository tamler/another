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
//! Reports run in the background and their failures are ignored. The last report per pane
//! is kept in a small SQLite file, `$ANOTHER_HOME/hooks/state/herdr-status.sqlite`
//! (separate from Another's database). Deciding whether to send and reserving the
//! sequence number happen in one write transaction, so parallel hooks in a pane never send
//! the same `--seq` and a backward clock step never lowers it:
//! `seq = max(now in microseconds, last + 1)`. An unchanged state is not sent again (only
//! refreshed before its tokens expire), and a report whose `herdr` could not be started is
//! sent again by the next hook.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use another_core::{AgentState, AgentStatus};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

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
/// Longest a hook waits for another hook's reservation; past it the report is skipped.
const LOCK_WAIT: Duration = Duration::from_millis(200);
/// The cache of last reports, one row per pane.
const CACHE_FILE: &str = "herdr-status.sqlite";

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

/// Longest blocked note sent to Herdr, after cleaning.
const NOTE_CHARS: usize = 200;

/// What to send: the state token and the blocked note, or `None` to clear both.
fn tokens(status: Option<&AgentStatus>) -> (Option<&'static str>, Option<String>) {
    match status {
        Some(s) if s.state != AgentState::Unknown => (
            Some(s.state.as_str()),
            s.message
                .as_deref()
                .filter(|_| s.state == AgentState::Blocked)
                // Herdr draws the token in a terminal; the path inside it was chosen by
                // whoever named the file (D75: one safe line).
                .map(|m| format!("blocked: {}", another_core::single_line(m, NOTE_CHARS))),
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

fn cache_path(another_home: &Path) -> PathBuf {
    another_home.join("hooks").join("state").join(CACHE_FILE)
}

fn open_cache(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(LOCK_WAIT)?;
    // Every hook opens and closes its own connection. By default the last connection to
    // close checkpoints the WAL (with fsyncs), which made a burst of hooks wait on each
    // other's closes; SQLite's automatic checkpoint (every 1000 pages) still runs.
    let _ = conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    );
    // WAL with `synchronous = NORMAL`: commits stay atomic and ordered but skip the per-commit
    // fsync, so a burst of hooks is not held up by slow disks. Power loss can drop the last
    // rows; the next reservation is then still `max(now, last + 1)`. The mode is stored in
    // the file, so it is switched once; switching can answer busy without waiting, which
    // [`reserve_retrying`] retries.
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // `at_ms` 0 marks a report whose `herdr` could not be started: send it again.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS panes (
             pane  TEXT PRIMARY KEY,
             state TEXT,
             note  TEXT,
             seq   INTEGER NOT NULL,
             at_ms INTEGER NOT NULL
         )",
    )?;
    Ok(conn)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// The pane's last report: state, note, sequence number, and when it was sent (0 when its
/// `herdr` could not be started).
type LastReport = (Option<String>, Option<String>, i64, i64);

fn last_report(conn: &Connection, pane: &str) -> rusqlite::Result<Option<LastReport>> {
    conn.query_row(
        "SELECT state, note, seq, at_ms FROM panes WHERE pane = ?1",
        [pane],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
}

/// Whether `state`/`note` must be sent, given the pane's last report.
fn needs_send(
    last: Option<&LastReport>,
    state: Option<&str>,
    note: Option<&str>,
    now_ms: u64,
) -> bool {
    match last {
        Some((last_state, last_note, _, at_ms)) => {
            let unchanged = last_state.as_deref() == state && last_note.as_deref() == note;
            let delivered = *at_ms != 0;
            let fresh = state.is_none() || now_ms.saturating_sub(*at_ms as u64) < REFRESH_MS;
            !(unchanged && delivered && fresh)
        }
        None => true,
    }
}

/// The file in which the build before the SQLite cache kept `pane`'s last report.
fn legacy_cache_path(state_dir: &Path, pane: &str) -> PathBuf {
    let mut safe = String::from("herdr-");
    for b in pane.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            safe.push(b as char);
        } else {
            safe.push_str(&format!("_{b:02x}"));
        }
    }
    state_dir.join(format!("{safe}.json"))
}

/// The sequence number of `pane`'s last report under the previous JSON cache, 0 when there
/// is none or it cannot be read. Herdr may already have accepted it, so the SQLite cache
/// starts above it.
fn legacy_watermark(state_dir: &Path, pane: &str) -> u64 {
    std::fs::read(legacy_cache_path(state_dir, pane))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

/// In one write transaction: decide whether `state`/`note` must be sent for `pane`, and if
/// so record it and reserve its sequence number, `max(now_us, last + 1)`. Parallel callers
/// are serialized by SQLite's write lock, so each reservation is above every earlier one,
/// whatever the clock says. The write lock is only taken when a plain read shows that
/// something may need sending; the decision is made again inside the transaction. A pane
/// with no row yet starts above the previous JSON cache's watermark in `state_dir`, read
/// under the same lock; once the row exists it carries that watermark on.
fn reserve(
    conn: &mut Connection,
    state_dir: &Path,
    pane: &str,
    state: Option<&str>,
    note: Option<&str>,
    now_us: u64,
) -> rusqlite::Result<Option<u64>> {
    let now_ms = now_us / 1000;
    if !needs_send(last_report(conn, pane)?.as_ref(), state, note, now_ms) {
        return Ok(None);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let last = last_report(&tx, pane)?;
    if !needs_send(last.as_ref(), state, note, now_ms) {
        return Ok(None);
    }
    let last_seq = match last {
        Some((_, _, seq, _)) => u64::try_from(seq).unwrap_or(0),
        None => legacy_watermark(state_dir, pane),
    };
    // Sequence numbers are stored as SQLite integers; past `i64::MAX` nothing can follow.
    let Some(seq) = last_seq
        .checked_add(1)
        .map(|next| next.max(now_us))
        .filter(|s| i64::try_from(*s).is_ok())
    else {
        return Ok(None);
    };
    tx.execute(
        "INSERT INTO panes (pane, state, note, seq, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(pane) DO UPDATE SET state = excluded.state, note = excluded.note,
           seq = excluded.seq, at_ms = excluded.at_ms",
        params![pane, state, note, seq as i64, now_ms.max(1) as i64],
    )?;
    tx.commit()?;
    Ok(Some(seq))
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// [`open_cache`] and [`reserve`], retried until [`LOCK_WAIT`] has passed. SQLite answers
/// busy without calling the busy handler in a few cases (switching to WAL, opening while
/// another connection sets up or tears down the WAL index), so the busy timeout alone does
/// not bound the wait for a burst of hooks.
fn reserve_retrying(
    path: &Path,
    pane: &str,
    state: Option<&str>,
    note: Option<&str>,
) -> rusqlite::Result<Option<u64>> {
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    let state_dir = path.parent().unwrap_or(Path::new("."));
    loop {
        let attempt = open_cache(path)
            .and_then(|mut conn| reserve(&mut conn, state_dir, pane, state, note, now_micros()));
        match attempt {
            Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            other => return other,
        }
    }
}

/// Report `status` for `pane` (`None`, or an `unknown` state, clears the tokens). Returns
/// the arguments sent, or `None` when nothing needed sending or the cache could not be
/// used within [`LOCK_WAIT`]. Never fails: a missing or failing `herdr` is ignored.
pub fn report(
    another_home: &Path,
    pane: &Pane,
    status: Option<&AgentStatus>,
) -> Option<Vec<String>> {
    match try_report(another_home, pane, status) {
        Ok(sent) => sent,
        Err(e) => {
            tracing::debug!(error = %e, "herdr status report skipped");
            None
        }
    }
}

/// [`report`], with the reason a report was skipped.
fn try_report(
    another_home: &Path,
    pane: &Pane,
    status: Option<&AgentStatus>,
) -> rusqlite::Result<Option<Vec<String>>> {
    let (state, note) = tokens(status);
    let path = cache_path(another_home);
    let Some(seq) = reserve_retrying(&path, &pane.id, state, note.as_deref())? else {
        return Ok(None);
    };
    let argv = args(&pane.id, state, note.as_deref(), seq);
    // In the background: the hook does not wait for Herdr, and Herdr's answer is unused.
    let spawned = Command::new(&pane.bin)
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "herdr status report not started");
        // Not delivered: let the next hook send it again, unless a later report followed.
        let _ = open_cache(&path).and_then(|conn| {
            conn.execute(
                "UPDATE panes SET at_ms = 0 WHERE pane = ?1 AND seq = ?2",
                params![pane.id, seq as i64],
            )
        });
    }
    Ok(Some(argv))
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

    /// A `herdr` that starts and exits 0.
    fn fake_herdr(dir: &Path) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bin = dir.join("herdr");
            std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            bin
        }
        #[cfg(windows)]
        {
            let bin = dir.join("herdr.cmd");
            std::fs::write(&bin, "@exit /b 0\r\n").unwrap();
            bin
        }
    }

    fn seq_of(argv: &[String]) -> u64 {
        let i = argv.iter().position(|x| x == "--seq").unwrap();
        argv[i + 1].parse().unwrap()
    }

    /// #144 review R2 (P2): the build before the SQLite cache kept each pane's last report
    /// in `hooks/state/herdr-<encoded pane>.json`. Its sequence number is the watermark
    /// Herdr may already have accepted, so the first report after the upgrade must be
    /// above it even when the clock has stepped back since.
    #[test]
    fn upgrading_from_the_json_cache_keeps_its_sequence_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("hooks").join("state");
        std::fs::create_dir_all(&state).unwrap();
        let ahead = now_micros() + 3_600_000_000;
        // Exact file names and contents the previous build wrote.
        for (file, seq) in [
            ("herdr-w1_3ap1.json", ahead),
            ("herdr-w1_3ap2_2f_2e_2e_2fx.json", ahead + 7),
        ] {
            std::fs::write(
                state.join(file),
                format!(r#"{{"state":"working","note":null,"seq":{seq},"at_ms":1}}"#),
            )
            .unwrap();
        }
        let bin = fake_herdr(dir.path());
        let blocked = status(AgentState::Blocked, Some("x is held by y"));
        for (pane, legacy) in [("w1:p1", ahead), ("w1:p2/../x", ahead + 7)] {
            let pane = Pane {
                bin: bin.clone(),
                id: pane.into(),
            };
            let first = report(dir.path(), &pane, Some(&blocked)).expect("a change is sent");
            assert_eq!(seq_of(&first), legacy + 1, "{}", pane.id);
            let next = report(dir.path(), &pane, None).expect("a change is sent");
            assert_eq!(seq_of(&next), legacy + 2, "the watermark is carried on");
        }
        // A pane with no legacy file starts from the clock; an unreadable one is treated as
        // absent rather than failing the report.
        std::fs::write(state.join("herdr-w1_3ap3.json"), "{\"seq\": tru").unwrap();
        for pane in ["w1:p9", "w1:p3"] {
            let pane = Pane {
                bin: bin.clone(),
                id: pane.into(),
            };
            let sent = report(dir.path(), &pane, Some(&blocked)).expect("sent");
            assert!(seq_of(&sent) < ahead, "{}", pane.id);
        }
    }

    #[test]
    fn reports_coalesce_and_sequence_numbers_only_grow() {
        let dir = tempfile::tempdir().unwrap();
        let pane = Pane {
            bin: fake_herdr(dir.path()),
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
        assert!(seq_of(&first) < seq_of(&second) && seq_of(&second) < seq_of(&cleared));

        // Panes are sequenced separately; a pane id is only a key, never a path.
        let other = Pane {
            bin: pane.bin.clone(),
            id: "w1:p2/../x".into(),
        };
        assert!(report(dir.path(), &other, Some(&working)).is_some());
        assert!(report(dir.path(), &other, Some(&working)).is_none());
        let state_dir = dir.path().join("hooks").join("state");
        let files: Vec<String> = std::fs::read_dir(&state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with(CACHE_FILE) || n == CACHE_FILE)
            .collect();
        assert_eq!(files, [CACHE_FILE]);
    }

    #[test]
    fn a_cached_sequence_ahead_of_the_clock_only_grows_until_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pane = Pane {
            bin: fake_herdr(dir.path()),
            id: "w1:p1".into(),
        };
        let working = status(AgentState::Working, None);
        let ahead = i64::MAX as u64 - 1;
        seed_last(dir.path(), "w1:p1", ahead);
        let next = report(dir.path(), &pane, Some(&working)).unwrap();
        assert_eq!(seq_of(&next), ahead + 1);
        // At the largest storable number nothing more can be sent; nothing panics.
        assert!(report(dir.path(), &pane, None).is_none());
    }

    #[test]
    fn a_report_whose_herdr_cannot_start_is_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Pane {
            bin: dir.path().join("no-such-herdr"),
            id: "w1:p1".into(),
        };
        let working = status(AgentState::Working, None);
        let first = report(dir.path(), &missing, Some(&working)).expect("sent");
        let again = report(dir.path(), &missing, Some(&working)).expect("not delivered: resent");
        assert!(seq_of(&again) > seq_of(&first));
        let found = Pane {
            bin: fake_herdr(dir.path()),
            id: "w1:p1".into(),
        };
        assert!(report(dir.path(), &found, Some(&working)).is_some());
        assert!(
            report(dir.path(), &found, Some(&working)).is_none(),
            "delivered: coalesced"
        );
    }

    /// Record `seq` as the pane's last report, as an earlier run would have.
    fn seed_last(another_home: &Path, pane: &str, seq: u64) {
        let conn = open_cache(&cache_path(another_home)).unwrap();
        conn.execute(
            "INSERT INTO panes (pane, state, note, seq, at_ms) VALUES (?1, NULL, NULL, ?2, 1)",
            params![pane, seq as i64],
        )
        .unwrap();
    }

    /// Every sequence sent is above `ahead` and distinct. A caller that could not get the
    /// cache lock in time skips its report (documented), so liveness is only required for
    /// most callers, not all, on a slow machine.
    fn assert_sent_seqs(sent: &[Result<Option<u64>, String>], ahead: u64) {
        let mut seqs: Vec<u64> = sent.iter().flatten().flatten().copied().collect();
        let errors: Vec<&String> = sent.iter().filter_map(|r| r.as_ref().err()).collect();
        eprintln!(
            "{} of {} callers sent; errors {errors:?}; seqs {seqs:?}",
            seqs.len(),
            sent.len()
        );
        assert!(
            seqs.len() * 2 >= sent.len(),
            "most callers must get the lock: {sent:?}"
        );
        assert!(
            seqs.iter().all(|s| *s > ahead),
            "a sequence fell back to the clock: {seqs:?}"
        );
        let n = seqs.len();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), n, "parallel reports reused a sequence");
    }

    /// #144 review R1 (P2): parallel hooks in one pane, after the clock stepped back an
    /// hour, must still send distinct sequence numbers above the last one sent.
    #[test]
    fn parallel_reports_after_a_backward_clock_step_get_unique_increasing_seqs() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let ahead = now_micros() + 3_600_000_000;
        seed_last(dir.path(), "w1:p1", ahead);
        let callers = 32;
        let barrier = Arc::new(Barrier::new(callers));
        let threads: Vec<_> = (0..callers)
            .map(|i| {
                let home = dir.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let pane = Pane {
                        bin: home.join("no-such-herdr"),
                        id: "w1:p1".into(),
                    };
                    let blocked = status(AgentState::Blocked, Some(&format!("block {i}")));
                    barrier.wait();
                    try_report(&home, &pane, Some(&blocked))
                        .map(|sent| sent.map(|argv| seq_of(&argv)))
                        .map_err(|e| e.to_string())
                })
            })
            .collect();
        let sent: Vec<Result<Option<u64>, String>> =
            threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_sent_seqs(&sent, ahead);
    }

    const WORKER_HOME: &str = "ANOTHER_TEST_HERDR_SEQ_HOME";
    const WORKER_INDEX: &str = "ANOTHER_TEST_HERDR_SEQ_INDEX";

    /// The child side of the next test; does nothing unless started by it.
    #[test]
    fn herdr_seq_worker() {
        let Some(home) = std::env::var_os(WORKER_HOME).map(PathBuf::from) else {
            return;
        };
        let i = std::env::var(WORKER_INDEX).unwrap();
        // Start together: wait for the parent's signal file.
        let go = home.join("go");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !go.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let pane = Pane {
            bin: home.join("no-such-herdr"),
            id: "w1:p1".into(),
        };
        let blocked = status(AgentState::Blocked, Some(&format!("process {i}")));
        match try_report(&home, &pane, Some(&blocked)) {
            Ok(Some(argv)) => println!("SEQ={}", seq_of(&argv)),
            Ok(None) => println!("SEQ=none"),
            Err(e) => println!("SEQ=error {}", e.to_string().replace('\n', " ")),
        }
    }

    /// The same as the previous test across processes, as real hooks are.
    #[test]
    fn parallel_report_processes_after_a_backward_clock_step_get_unique_increasing_seqs() {
        if std::env::var_os(WORKER_HOME).is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ahead = now_micros() + 3_600_000_000;
        seed_last(dir.path(), "w1:p1", ahead);
        let exe = std::env::current_exe().unwrap();
        let workers: Vec<_> = (0..16)
            .map(|i| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "herdr::tests::herdr_seq_worker",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(WORKER_HOME, dir.path())
                    .env(WORKER_INDEX, i.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        std::fs::write(dir.path().join("go"), b"").unwrap();
        let sent: Vec<Result<Option<u64>, String>> = workers
            .into_iter()
            .map(|w| {
                let out = w.wait_with_output().unwrap();
                assert!(out.status.success(), "worker failed");
                let line = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find_map(|l| l.split_once("SEQ=").map(|(_, n)| n.to_string()))
                    .expect("worker printed its sequence");
                match line.split_once(' ') {
                    Some(("error", why)) => Err(why.trim().to_string()),
                    _ => {
                        let word = line.split_whitespace().next().unwrap_or("");
                        Ok((word != "none").then(|| word.parse().unwrap()))
                    }
                }
            })
            .collect();
        assert_sent_seqs(&sent, ahead);
    }

    /// Adversarial F1 (#144): the blocked note is built from a held path and a holder, and a
    /// file name can hold ESC, CR, LF or a bidi override. Herdr draws the token in its
    /// terminal UI, so the note must be one line with no control or bidi-override
    /// characters (the D75 rule for anything shown in a terminal).
    #[test]
    fn adv144_the_blocked_note_is_one_safe_line() {
        let hostile = "src/a\u{1b}[2J\r\nX\u{202e}gpj.rs is held by alice";
        let (_, note) = tokens(Some(&status(AgentState::Blocked, Some(hostile))));
        let note = note.expect("a blocked status has a note");
        assert!(
            !note.chars().any(another_core::is_unsafe_text_char),
            "{note:?}"
        );
    }
}
