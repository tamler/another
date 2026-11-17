//! Waking agents: typing one line into an idle agent's session when something waits for it.
//!
//! Wakes go through the transport interface ([`crate::transport`]). `another serve` runs a
//! [`Waker`]: every 30 seconds it asks each transport in use what it can see and records it
//! per session, and every pass it wakes an idle, identified session of each agent that has
//! mail no hook has announced yet. A wake that was delivered but did not start work is a
//! recorded failure, and stops further automatic wakes to that session.
//!
//! `another wake --agent` is a direct owner action; `another wake --all` is one wake pass
//! from the command line. tmux and cmux targets are woken by hand only, through fixed
//! commands that run as argv (no shell).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use another_core::{
    identify, newest_at_target, Identity, InboxQuery, Store, TransportSession, TransportWake, Tri,
    WakeKind, WakeOutcome, WakeVia, UNDELIVERED_LIMIT,
};
use anyhow::Result;
use serde::Serialize;

use crate::transport::herdr::{HerdrTransport, HERDR_CONFIRMS_GONE};
use crate::transport::{ListResult, State, Transport, WakeResult};

/// Shown where a wake line may still sit in the agent's input box.
pub const INPUT_BOX_HINT: &str =
    "the wake line may still be in the agent's input box; clear it before typing";

#[derive(Debug, Clone, Serialize)]
pub struct WakePlan {
    pub agent: String,
    pub via: Option<WakeVia>,
    pub target: Option<String>,
    /// The session row the wake went to, when it went to one.
    pub session_id: Option<String>,
    pub message: Option<String>,
    /// The commands that run (or would run, with --dry-run).
    pub commands: Vec<Vec<String>>,
    /// The line was delivered (`delivered` is `yes`).
    pub sent: bool,
    /// How the wake call ended; `None` when no wake call was made.
    pub outcome: Option<WakeOutcome>,
    pub delivered: Option<Tri>,
    pub started: Option<Tri>,
    /// The outcome is a failed wake.
    pub failure: bool,
    /// What the transport answered.
    pub detail: Option<String>,
    /// Set when the wake line may be left in the agent's input box.
    pub hint: Option<String>,
    /// Why nothing was sent.
    pub skipped: Option<String>,
    /// The wake could not be attempted, or was refused before anything was typed.
    pub error: Option<String>,
}

impl WakePlan {
    fn new(agent: &str) -> WakePlan {
        WakePlan {
            agent: agent.to_string(),
            via: None,
            target: None,
            session_id: None,
            message: None,
            commands: Vec::new(),
            sent: false,
            outcome: None,
            delivered: None,
            started: None,
            failure: false,
            detail: None,
            hint: None,
            skipped: None,
            error: None,
        }
    }

    fn finish(&mut self, result: &WakeResult) {
        self.outcome = Some(result.outcome);
        self.delivered = Some(result.delivered());
        self.started = Some(result.started());
        self.failure = result.outcome.failure();
        self.sent = result.delivered() == Tri::Yes;
        self.detail = Some(result.detail.clone());
    }
}

/// Overrides for a single wake.
#[derive(Debug, Clone, Default)]
pub struct WakeOptions {
    pub via: Option<WakeVia>,
    pub target: Option<String>,
    pub message: Option<String>,
    /// Wake even with nothing unread.
    pub force: bool,
    pub dry_run: bool,
    /// Set by the daemon on shutdown; a running adapter command is then stopped.
    pub cancel: Option<Arc<AtomicBool>>,
    /// The program to run as Herdr (default: `$HERDR_BIN_PATH`, else `herdr`).
    pub herdr: Option<String>,
}

impl WakeOptions {
    fn herdr(&self) -> HerdrTransport {
        match &self.herdr {
            Some(program) => HerdrTransport::new(program.clone()),
            None => HerdrTransport::from_env(),
        }
    }
}

/// Longest wake line, in characters.
const MAX_WAKE_CHARS: usize = 1000;
/// Longest subject quoted in a default wake line, in characters.
const MAX_WAKE_SUBJECT_CHARS: usize = 80;

/// The commands for a wake by hand through tmux or cmux (and, for `--dry-run`, Herdr).
///
/// Every adapter types or shows `message` in another agent's terminal, and parts of it
/// (subjects) were chosen by whoever sent the mail. The line is made safe here, for every
/// caller: control characters (CR, LF, ESC, …) become spaces so nothing is pressed as a
/// key, and for cmux, which unescapes backslash sequences such as `\n`, backslashes in
/// the text become `/` so the only escape left is the one that submits the line.
pub fn commands(via: WakeVia, target: &str, message: &str) -> Vec<Vec<String>> {
    let safe = another_core::single_line(message, MAX_WAKE_CHARS);
    let message = safe.as_str();
    let cmux_text = safe.replace('\\', "/");
    let s = |x: &str| x.to_string();
    match via {
        WakeVia::Tmux => vec![
            vec![
                s("tmux"),
                s("send-keys"),
                s("-t"),
                s(target),
                s("-l"),
                s(message),
            ],
            vec![s("tmux"), s("send-keys"), s("-t"), s(target), s("Enter")],
        ],
        WakeVia::Cmux if target == "notify" => vec![vec![
            s("cmux"),
            s("notify"),
            s("--title"),
            s("Another"),
            s("--body"),
            cmux_text.clone(),
        ]],
        // `cmux send` unescapes `\n`, which submits the line.
        WakeVia::Cmux => vec![vec![
            s("cmux"),
            s("send"),
            s("--surface"),
            s(target),
            format!("{cmux_text}\\n"),
        ]],
        WakeVia::Herdr => HerdrTransport::from_env().wake_argv(target, message),
    }
}

/// The wake line for an agent, built in one place: a one-line inbox summary, or `None`
/// with nothing unread and no open requests. Also returns the agent's address.
pub fn default_message(store: &Store, agent: &str) -> Result<(Option<String>, String)> {
    let inbox = store.inbox(
        agent,
        InboxQuery {
            unread_only: true,
            urgent_only: false,
            limit: 3,
        },
    )?;
    let open = inbox.requests.len();
    if inbox.unread == 0 && open == 0 {
        return Ok((None, inbox.agent));
    }
    let subjects: Vec<String> = inbox
        .messages
        .iter()
        .map(|m| {
            format!(
                "\"{}\"",
                another_core::single_line(&m.message.subject, MAX_WAKE_SUBJECT_CHARS)
            )
        })
        .collect();
    let mut msg = format!(
        "Another: {} unread message(s) ({} urgent) and {} open request(s) for {}",
        inbox.unread, inbox.urgent_unread, open, inbox.agent
    );
    if !subjects.is_empty() {
        msg.push_str(&format!(": {}", subjects.join(", ")));
    }
    msg.push_str(&format!(
        ". Read them with the fetch_inbox tool (or `another inbox --agent {a}`), then reply with \
         send_message / `another reply` and mark them done with acknowledge / `another ack`.",
        a = inbox.agent
    ));
    Ok((Some(msg), inbox.agent))
}

/// What waits for an agent and has not been announced to it.
#[derive(Debug, Clone)]
pub struct Waiting {
    /// Unannounced mail, newest first.
    pub message_ids: Vec<String>,
}

/// Is something waiting for this agent that no channel has announced? The one answer for
/// the daemon's wake pass and for `another wake --all`: today, unannounced mail.
pub fn waiting(store: &Store, agent: &str) -> Result<Option<Waiting>> {
    let unread = store.unannounced(agent, "", crate::hooks::NOTICE_BATCH)?;
    if unread.items.is_empty() {
        return Ok(None);
    }
    Ok(Some(Waiting {
        message_ids: unread.items.into_iter().map(|i| i.message_id).collect(),
    }))
}

/// After a wake, leave that session row alone this long.
const ROW_GAP_MS: i64 = 60_000;
/// The same, once three wakes in a row could not be delivered.
const UNDELIVERED_GAP_MS: i64 = 600_000;
/// A row whose pane did not identify it is not asked again before the next sweep.
const REFUSED_MS: i64 = 30_000;
/// The daemon asks each transport what it can see this often.
pub const SWEEP_EVERY_MS: i64 = 30_000;

type RowKey = (String, String);

fn key(row: &TransportSession) -> RowKey {
    (row.agent_id.clone(), row.session_id.clone())
}

/// Step 1 of a wake pass: the agent's idle (or not yet observed) hook-made row whose hooks
/// ran last, leaving out rows that are stopped, were woken too recently, or that `held`
/// names. `Err` says why there is none, and whether a sibling session is working.
fn pick_row<'a>(
    rows: &'a [TransportSession],
    agent: &str,
    via: Option<WakeVia>,
    now: i64,
    held: impl Fn(&TransportSession) -> Option<&'static str>,
) -> std::result::Result<&'a TransportSession, (String, bool)> {
    let mut why = Vec::new();
    let mut best: Option<&TransportSession> = None;
    let mut working = false;
    for row in rows.iter().filter(|r| r.agent == agent && r.hook_made()) {
        let gap = if row.undelivered_count >= UNDELIVERED_LIMIT {
            UNDELIVERED_GAP_MS
        } else {
            ROW_GAP_MS
        };
        let state = row.state.as_deref();
        working |= state == Some("working");
        let skip = if row.transport != "herdr" || via.is_some_and(|v| v.as_str() != row.transport) {
            Some(format!("{} is not woken automatically", row.transport))
        } else if row.stopped() {
            Some("stopped after a wake that did not start work".to_string())
        } else if row.last_wake_at.is_some_and(|at| now - at < gap) {
            Some(format!("woken less than {}s ago", gap / 1000))
        } else if let Some(reason) = held(row) {
            Some(reason.to_string())
        } else if !matches!(state, None | Some("idle")) {
            Some(format!("is {}", state.unwrap_or_default()))
        } else {
            None
        };
        match skip {
            Some(reason) => why.push(format!("{} {reason}", row.target)),
            None if best.is_none_or(|b| row.hook_seen_at > b.hook_seen_at) => best = Some(row),
            None => {}
        }
    }
    best.ok_or_else(|| {
        if why.is_empty() {
            (
                "no session Another can identify (a hook inside Herdr records one)".to_string(),
                false,
            )
        } else {
            (format!("no session to wake: {}", why.join("; ")), working)
        }
    })
}

/// What happened to one row in a wake pass.
enum Step {
    /// Step 2 said no: the pane does not show this session idle. Nothing was written.
    Refused(String),
    /// A hook announced the mail while the transport was answering.
    AlreadyAnnounced,
    /// The wake call was made. `recorded` is false when its outcome could not be written.
    Woke { result: WakeResult, recorded: bool },
}

/// Steps 2 to 4 of a wake pass for one row: check identity and state now, claim the
/// announcements, wake, record.
#[allow(clippy::too_many_arguments)]
fn wake_row(
    store: &Store,
    transport: &dyn Transport,
    row: &TransportSession,
    rows: &[TransportSession],
    line: &str,
    claim: Option<&[String]>,
    trigger: &str,
    cancel: Option<&AtomicBool>,
) -> Result<Step> {
    let seen = transport.status(&row.target, cancel);
    let identity = identify(row, &seen, newest_at_target(row, rows));
    if seen.state != State::Idle {
        return Ok(Step::Refused(format!(
            "{} is {} ({})",
            row.target,
            seen.state.as_str(),
            seen.detail
        )));
    }
    if let Identity::No(reason) = identity {
        return Ok(Step::Refused(refusal(row, reason)));
    }
    // Claim after the answer, not before: a hook that announced these while the transport
    // was answering has already won them.
    let claimed = match claim {
        Some(ids) => {
            let claimed = store.claim_announcement(&row.agent, ids, "wake", None)?;
            if claimed.is_empty() {
                return Ok(Step::AlreadyAnnounced);
            }
            claimed
        }
        None => Vec::new(),
    };
    let result = transport.wake(&row.target, line, cancel);
    let recorded = record(store, row, &result, claimed, trigger);
    Ok(Step::Woke { result, recorded })
}

fn refusal(row: &TransportSession, reason: &str) -> String {
    if reason.is_empty() {
        format!("{} does not show this session", row.target)
    } else {
        format!("{}: {reason}", row.target)
    }
}

fn record(
    store: &Store,
    row: &TransportSession,
    result: &WakeResult,
    message_ids: Vec<String>,
    trigger: &str,
) -> bool {
    let wake = TransportWake {
        agent: row.agent.clone(),
        session_id: Some(row.session_id.clone()),
        transport: row.transport.clone(),
        target: row.target.clone(),
        outcome: result.outcome,
        detail: result.detail.clone(),
        kind: WakeKind::Mail,
        message_ids,
        trigger: trigger.to_string(),
    };
    match store.record_transport_wake(&wake) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(agent = %row.agent, error = %e, "wake outcome not recorded");
            false
        }
    }
}

/// The agent's stored targets: (set by hand, captured automatically).
fn stored_targets(
    store: &Store,
    address: &str,
) -> Result<(
    Option<another_core::WakeTarget>,
    Option<another_core::WakeTarget>,
)> {
    let stored = store
        .wake_targets()?
        .into_iter()
        .find(|t| t.agent == address);
    Ok(match stored {
        Some(t) if t.auto => (None, Some(t)),
        other => (other, None),
    })
}

/// Plan (and unless `dry_run`, send) a wake for one agent: a direct owner action. It
/// ignores a stop and the gap between wakes, takes no announcement claim, and records its
/// outcome.
pub fn wake(store: &Store, agent: &str, opts: &WakeOptions) -> Result<WakePlan> {
    let (auto_message, address) = default_message(store, agent)?;
    let (hand, auto) = stored_targets(store, &address)?;
    let rows = store.transport_sessions()?;
    let mut mine: Vec<&TransportSession> = rows
        .iter()
        .filter(|r| r.agent == address && r.hook_made())
        .collect();
    mine.sort_by_key(|r| std::cmp::Reverse(r.hook_seen_at));
    let row_via = |r: &TransportSession| r.transport.parse::<WakeVia>().ok();
    let mut plan = WakePlan::new(&address);
    plan.via = opts
        .via
        .or(hand.as_ref().map(|t| t.via))
        .or(mine.iter().find_map(|r| row_via(r)))
        .or(auto.as_ref().map(|t| t.via));
    plan.message = opts.message.clone().or(auto_message);
    if plan.message.is_none() {
        if !opts.force {
            plan.skipped = Some("nothing unread and no open requests".into());
            return Ok(plan);
        }
        plan.message = Some(format!(
            "Another: check your inbox with the fetch_inbox tool (or `another inbox --agent {address}`)."
        ));
    }
    let Some(via) = plan.via else {
        plan.skipped = Some(format!(
            "no wake target for {address}; set one with `another wake set --agent {address} --via tmux --target <pane>`"
        ));
        return Ok(plan);
    };
    // Where to wake, in order: `--target`, a target set by hand, the session whose hooks
    // ran last, an automatic capture, and for Herdr the agent's bare name.
    let row = if opts.target.is_some() || hand.is_some() {
        None
    } else {
        mine.iter().find(|r| row_via(r) == Some(via)).copied()
    };
    plan.target = opts
        .target
        .clone()
        .or_else(|| hand.as_ref().map(|t| t.target.clone()))
        .or_else(|| row.map(|r| r.target.clone()))
        .or_else(|| auto.as_ref().map(|t| t.target.clone()));
    if plan.target.is_none() && via == WakeVia::Herdr {
        // Herdr addresses agents by name; default to the Another agent's name.
        plan.target = Some(address.split('@').next().unwrap_or(&address).to_string());
    }
    let Some(target) = plan.target.clone() else {
        plan.skipped = Some(format!("no {} target for {address}", via.as_str()));
        return Ok(plan);
    };
    plan.session_id = row.map(|r| r.session_id.clone());
    // A `--target` override, a target stored before this check, or Herdr's default of the
    // agent's name (which may start with `-`) never reaches an adapter as an option.
    another_core::refuse_option_like_target(&target)?;
    // The plan's message is printed by `another wake --dry-run` and returned in JSON: keep
    // it to the same safe line the adapters type.
    plan.message = plan
        .message
        .as_deref()
        .map(|m| another_core::single_line(m, MAX_WAKE_CHARS));
    let line = plan.message.clone().unwrap_or_default();
    let herdr = opts.herdr();
    plan.commands = match via {
        WakeVia::Herdr => herdr.wake_argv(&target, &line),
        _ => commands(via, &target, &line),
    };
    if opts.dry_run {
        plan.skipped = Some("dry run".into());
        return Ok(plan);
    }
    let cancel = opts.cancel.as_deref();
    if let Some(row) = row {
        // The target came from a session row: type only into the pane that still shows
        // that session. Herdr itself refuses a blocked agent, so `idle` is not required.
        let seen = herdr.status(&row.target, cancel);
        if let Identity::No(reason) = identify(row, &seen, newest_at_target(row, &rows)) {
            let why = match seen.state {
                State::Gone | State::NoAnswer => {
                    format!(
                        "{} is {} ({})",
                        row.target,
                        seen.state.as_str(),
                        seen.detail
                    )
                }
                _ => refusal(row, reason),
            };
            plan.error = Some(format!("not woken, nothing was typed: {why}"));
            return Ok(plan);
        }
    }
    let had_stop = rows.iter().any(|r| {
        r.agent == address
            && r.transport == via.as_str()
            && r.target == target
            && r.wake_stop.is_some()
    });
    let result = match via {
        WakeVia::Herdr => herdr.wake(&target, &line, cancel),
        _ => run_commands(via, &plan.commands, cancel),
    };
    plan.finish(&result);
    if had_stop || result.outcome == WakeOutcome::NotStarted {
        plan.hint = Some(INPUT_BOX_HINT.to_string());
    }
    let recorded = store.record_transport_wake(&TransportWake {
        agent: address.clone(),
        session_id: plan.session_id.clone(),
        transport: via.as_str().to_string(),
        target,
        outcome: result.outcome,
        detail: result.detail.clone(),
        kind: WakeKind::Mail,
        message_ids: Vec::new(),
        trigger: "manual".into(),
    });
    if let Err(e) = recorded {
        tracing::warn!(agent = %address, error = %e, "wake outcome not recorded");
    }
    Ok(plan)
}

/// Run the fixed tmux or cmux commands. These tools cannot say whether the agent started,
/// so the best outcome is `unconfirmed`.
fn run_commands(via: WakeVia, commands: &[Vec<String>], cancel: Option<&AtomicBool>) -> WakeResult {
    for (i, argv) in commands.iter().enumerate() {
        // Before the first command nothing was typed; after it something may have been.
        let failed = if i == 0 {
            WakeOutcome::Undelivered
        } else {
            WakeOutcome::Unknown
        };
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        match run_bounded(&mut cmd, PROMPT_TIMEOUT, cancel) {
            Err(e) => {
                return WakeResult::new(
                    failed,
                    format!("running {} (is it installed?): {e}", argv[0]),
                )
            }
            Ok(Bounded::Done(ran)) if ran.status.success() => {}
            Ok(Bounded::Done(ran)) => {
                return WakeResult::new(
                    failed,
                    format!("{} exited with {}", argv.join(" "), ran.status),
                )
            }
            Ok(Bounded::TimedOut) | Ok(Bounded::TooMuchOutput) => {
                return WakeResult::new(
                    WakeOutcome::Unknown,
                    format!(
                        "{} did not finish within {}s (or the daemon is stopping); it was stopped",
                        argv[0],
                        PROMPT_TIMEOUT.as_secs()
                    ),
                )
            }
        }
    }
    WakeResult::new(
        WakeOutcome::Unconfirmed,
        format!("{} cannot report whether the agent started", via.as_str()),
    )
}

/// Longest a hand wake command (`tmux send-keys`, `cmux send`) may take.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// After the adapter exits, how long to keep collecting what it already wrote. The pipe
/// may stay open when the adapter left a background process holding it; that process is
/// not waited for.
const OUTPUT_GRACE: Duration = Duration::from_millis(250);

/// Most output (stdout and stderr together) kept from one adapter command.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// What a finished adapter command left behind.
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// How a bounded adapter command ended.
#[derive(Debug)]
pub enum Bounded {
    Done(Ran),
    /// It ran past its deadline, or `cancel` was set; it was stopped.
    TimedOut,
    /// It wrote more than 1 MiB; it was stopped.
    TooMuchOutput,
}

impl Bounded {
    /// The finished command, when it finished by itself.
    pub fn done(self) -> Option<Ran> {
        match self {
            Bounded::Done(ran) => Some(ran),
            _ => None,
        }
    }
}

/// Run an adapter command with a deadline, capturing its stdout and stderr (1 MiB in
/// total). When it runs past `timeout`, `cancel` is set (daemon shutdown) or it writes
/// more than the cap, the command is stopped: SIGTERM to its process group (so anything it
/// started stops too), and a hard stop of the command if it is still running half a second
/// later. A stuck adapter, or one that leaves a process behind on its output, cannot hold
/// up the waker or a clean shutdown past the deadline.
pub fn run_bounded(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> std::io::Result<Bounded> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a timeout can stop what the adapter started.
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Read both pipes on the side and pass chunks back, so a chatty adapter cannot fill a
    // pipe, and a lingering holder of a pipe cannot block the caller. The flag says which
    // pipe a chunk came from.
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    fn pump(
        mut pipe: impl Read + Send + 'static,
        err: bool,
        tx: std::sync::mpsc::Sender<(bool, Vec<u8>)>,
    ) {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((err, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
            // Dropping `tx` tells the caller the pipe reached end of file.
        });
    }
    if let Some(stdout) = child.stdout.take() {
        pump(stdout, false, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        pump(stderr, true, tx.clone());
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let keep = |(err, chunk): (bool, Vec<u8>), stdout: &mut Vec<u8>, stderr: &mut Vec<u8>| {
        if err {
            stderr.extend(chunk);
        } else {
            stdout.extend(chunk);
        }
        stdout.len() + stderr.len() <= MAX_OUTPUT_BYTES
    };
    loop {
        while let Ok(chunk) = rx.try_recv() {
            if !keep(chunk, &mut stdout, &mut stderr) {
                stop_child(&mut child);
                return Ok(Bounded::TooMuchOutput);
            }
        }
        if let Some(status) = child.try_wait()? {
            let until = (Instant::now() + OUTPUT_GRACE).min(deadline.max(Instant::now()));
            loop {
                let left = until.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(chunk) => {
                        if !keep(chunk, &mut stdout, &mut stderr) {
                            return Ok(Bounded::TooMuchOutput);
                        }
                    }
                    // End of file, or something it left behind still holds a pipe.
                    Err(_) => break,
                }
            }
            return Ok(Bounded::Done(Ran {
                status,
                stdout,
                stderr,
            }));
        }
        let cancelled = cancel.is_some_and(|c| c.load(Ordering::SeqCst));
        if cancelled || Instant::now() >= deadline {
            stop_child(&mut child);
            return Ok(Bounded::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stop a child this process started: SIGTERM to its process group (it was started as a
/// group leader), then a hard stop of the child itself if it ignores that.
fn stop_child(child: &mut std::process::Child) {
    let group = if cfg!(unix) {
        format!("-{}", child.id())
    } else {
        child.id().to_string()
    };
    let _ = Command::new("kill")
        .args(["-TERM", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for _ in 0..25 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `another wake --all`: one wake pass from the command line, for every agent. The same
/// rules as the daemon's pass: unannounced mail, the announcement claim, the row choice,
/// the identity check, the stop and the gap, and the same record. Each plan says what
/// happened or why the agent was skipped. One agent's failure does not stop the others.
pub fn wake_all(store: &Store, opts: &WakeOptions) -> Result<Vec<WakePlan>> {
    let herdr = opts.herdr();
    let mut agents: Vec<String> = store
        .transport_sessions()?
        .into_iter()
        .filter(|r| r.hook_made())
        .map(|r| r.agent)
        .chain(store.wake_targets()?.into_iter().map(|t| t.agent))
        .collect();
    agents.sort();
    agents.dedup();
    Ok(agents
        .iter()
        .map(|agent| {
            wake_one_of_all(store, &herdr, agent, opts).unwrap_or_else(|e| {
                let mut plan = WakePlan::new(agent);
                plan.error = Some(format!("{e:#}"));
                plan
            })
        })
        .collect())
}

fn wake_one_of_all(
    store: &Store,
    herdr: &HerdrTransport,
    agent: &str,
    opts: &WakeOptions,
) -> Result<WakePlan> {
    let mut plan = WakePlan::new(agent);
    let waiting = waiting(store, agent)?;
    if waiting.is_none() && !opts.force {
        plan.skipped = Some("no unannounced mail".into());
        return Ok(plan);
    }
    let line = match opts.message.clone().or(default_message(store, agent)?.0) {
        Some(line) => line,
        None => format!(
            "Another: check your inbox with the fetch_inbox tool (or `another inbox --agent {agent}`)."
        ),
    };
    let line = another_core::single_line(&line, MAX_WAKE_CHARS);
    plan.message = Some(line.clone());
    let rows = store.transport_sessions()?;
    let row = match pick_row(&rows, agent, opts.via, store.now(), |_| None) {
        Ok(row) => row,
        Err((why, _)) => {
            plan.skipped = Some(why);
            return Ok(plan);
        }
    };
    plan.via = Some(WakeVia::Herdr);
    plan.target = Some(row.target.clone());
    plan.session_id = Some(row.session_id.clone());
    plan.commands = herdr.wake_argv(&row.target, &line);
    if opts.dry_run {
        plan.skipped = Some("dry run".into());
        return Ok(plan);
    }
    let ids = waiting.as_ref().map(|w| w.message_ids.as_slice());
    match wake_row(
        store,
        herdr,
        row,
        &rows,
        &line,
        ids,
        "all",
        opts.cancel.as_deref(),
    )? {
        Step::Refused(why) => plan.skipped = Some(format!("nothing was typed: {why}")),
        Step::AlreadyAnnounced => plan.skipped = Some("already announced by a hook".into()),
        Step::Woke { result, .. } => {
            plan.finish(&result);
            if result.outcome == WakeOutcome::NotStarted {
                plan.hint = Some(INPUT_BOX_HINT.to_string());
            }
        }
    }
    Ok(plan)
}

/// The terminal this process runs in. When multiplexers are nested, `TERM_PROGRAM` names
/// the innermost one; otherwise Herdr wins, then tmux, then cmux.
pub fn current_target() -> Option<(WakeVia, String)> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let herdr = var("HERDR_PANE_ID").map(|p| (WakeVia::Herdr, p));
    let tmux = var("TMUX_PANE").map(|p| (WakeVia::Tmux, p));
    let cmux = var("CMUX_SURFACE_ID").map(|p| (WakeVia::Cmux, p));
    match var("TERM_PROGRAM").as_deref() {
        Some("tmux") if tmux.is_some() => tmux,
        Some("herdr") if herdr.is_some() => herdr,
        _ => herdr.or(tmux).or(cmux),
    }
}

/// A local hook of a session ran: record where that session can be found. Inside Herdr
/// the session gets (or keeps) its row, stamped with the client and directory the hook
/// saw. Inside tmux or cmux, a session start records the pane as the agent's wake target.
pub fn stamp_session(
    store: &Store,
    agent: &str,
    session_id: Option<&str>,
    client: &str,
    dir: &Path,
    start: bool,
) {
    match current_target() {
        Some((WakeVia::Herdr, pane)) => {
            let cwd = another_core::resolve_dir(dir);
            let stamped = store.stamp_transport_session(
                agent,
                session_id.unwrap_or_default(),
                "herdr",
                &pane,
                client,
                &cwd,
            );
            if let Err(e) = stamped {
                tracing::debug!(error = %e, "session row not stamped");
            }
        }
        Some((via, target)) if start => {
            let _ = store.set_wake_target(agent, via, &target, true);
        }
        _ => {}
    }
}

/// At session end, delete that session's row, and forget a captured target that still
/// names this terminal, so nothing is typed into the pane after the agent has left it.
pub fn release_session(store: &Store, agent: &str, session_id: Option<&str>) {
    let here = current_target();
    let place = here
        .as_ref()
        .map(|(via, target)| (via.as_str(), target.as_str()));
    let _ = store.end_transport_session(agent, session_id.unwrap_or_default(), place);
    if let Some((via, target)) = &here {
        let _ = store.clear_auto_wake_target(agent, Some((*via, target)));
    }
}

/// An automatic target whose agent has not been seen for this long is stale.
const STALE_AFTER_MS: i64 = 24 * 3_600_000;

#[derive(Debug, Clone, Serialize)]
pub struct PruneRow {
    pub agent: String,
    pub via: String,
    pub target: String,
    /// Set for a session row; `None` for a stored wake target.
    pub session_id: Option<String>,
    pub pruned: bool,
    pub reason: String,
}

/// Drop what can no longer be woken, without asking any transport. Automatically captured
/// targets go when their agent ended its session or has not been seen for a day; targets
/// set by hand are listed but never removed. Session rows go by their own lifetime rules
/// ([`Store::prune_transport_sessions`]).
pub fn prune(store: &Store, dry_run: bool) -> Result<Vec<PruneRow>> {
    let mut rows = Vec::new();
    for t in store.wake_targets()? {
        let mut row = PruneRow {
            agent: t.agent.clone(),
            via: t.via.as_str().to_string(),
            target: t.target.clone(),
            session_id: None,
            pruned: false,
            reason: String::new(),
        };
        if !t.auto {
            row.reason = "set by hand; kept".into();
            rows.push(row);
            continue;
        }
        let agent = store.agent(&t.agent).ok();
        let offline = agent.as_ref().is_some_and(|a| a.status == "offline");
        let unseen = agent
            .as_ref()
            .and_then(|a| a.last_seen_at.as_deref())
            .and_then(another_core::parse_ts)
            .is_some_and(|seen| store.now() - seen > STALE_AFTER_MS);
        // A capture stored before targets were checked, which every wake now refuses.
        let stale = if another_core::refuse_option_like_target(&t.target).is_err() {
            Some("target starts with '-', which a wake refuses")
        } else if agent.is_none() {
            Some("agent is no longer registered")
        } else if offline {
            Some("agent ended its session")
        } else if unseen {
            Some("agent not seen for over a day")
        } else {
            None
        };
        if stale.is_some() {
            if !dry_run {
                store.clear_auto_wake_target(&t.agent, Some((t.via, &t.target)))?;
            }
            row.pruned = true;
        }
        row.reason = stale.unwrap_or("kept").to_string();
        rows.push(row);
    }
    for s in store.prune_transport_sessions(dry_run)? {
        rows.push(PruneRow {
            agent: s.agent,
            via: s.transport,
            target: s.target,
            session_id: Some(s.session_id),
            pruned: s.pruned,
            reason: s.reason,
        });
    }
    Ok(rows)
}

/// One decision the daemon's waker made.
#[derive(Debug, Clone, Serialize)]
pub struct PushEvent {
    pub agent: String,
    pub target: String,
    /// A wake outcome (`started`, `not_started`, ...), `skipped`, `working`, or `gone` for
    /// a session the sweep confirmed gone.
    pub action: String,
    pub detail: String,
}

/// The daemon side of the transport layer: the sweep and the wake pass. Every time is read
/// from the store's clock. Which messages were announced is claimed in the database
/// ([`Store::claim_announcement`]), shared with the hooks; stops and gaps are in the
/// session rows, shared with `another wake`.
pub struct Waker {
    /// Set on daemon shutdown: transport calls in flight are stopped.
    pub cancel: Arc<AtomicBool>,
    /// A burst of mail settles for this long before one wake covers all of it.
    pub settle: Duration,
    /// After finding every session of an agent busy, look again this soon.
    pub recheck: Duration,
    /// How long a session must stay gone, under watch, before it is confirmed gone.
    /// `None` confirms nothing: what this build ships with for Herdr
    /// ([`HERDR_CONFIRMS_GONE`]).
    pub confirm_after: Option<Duration>,
    transports: Vec<Box<dyn Transport>>,
    /// Newest unannounced message per agent, and when it was first seen.
    seen: HashMap<String, (String, i64)>,
    /// Do not look at an agent again before this.
    next_check: HashMap<String, i64>,
    /// Rows whose pane did not identify them: not asked again before this.
    refused: HashMap<RowKey, i64>,
    /// Rows whose wake outcome could not be written: not typed into again by this process.
    unrecorded: HashSet<RowKey>,
    /// Last decision per agent, so only changes are reported.
    last_action: HashMap<String, String>,
    last_sweep: Option<i64>,
}

impl Default for Waker {
    fn default() -> Waker {
        Waker::new()
    }
}

impl Waker {
    /// A waker over the transports of this build, found through the environment.
    pub fn new() -> Waker {
        Waker::with_transports(vec![Box::new(HerdrTransport::from_env())])
    }

    /// A waker over these transports (tests pass a fake Herdr).
    pub fn with_transports(transports: Vec<Box<dyn Transport>>) -> Waker {
        Waker {
            cancel: Arc::new(AtomicBool::new(false)),
            settle: Duration::from_secs(2),
            recheck: Duration::from_secs(5),
            confirm_after: HERDR_CONFIRMS_GONE
                .then(|| Duration::from_millis(another_core::GONE_CONFIRM_AFTER_MS as u64)),
            transports,
            seen: HashMap::new(),
            next_check: HashMap::new(),
            refused: HashMap::new(),
            unrecorded: HashSet::new(),
            last_action: HashMap::new(),
            last_sweep: None,
        }
    }

    fn transport(&self, name: &str) -> Option<&dyn Transport> {
        self.transports
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Whether the 30-second sweep is due by the store's clock.
    pub fn sweep_due(&self, store: &Store) -> bool {
        self.last_sweep
            .is_none_or(|at| store.now() - at >= SWEEP_EVERY_MS)
    }

    /// The sweep when it is due, then one wake pass.
    pub fn run(&mut self, store: &Store) -> Result<Vec<PushEvent>> {
        let mut events = Vec::new();
        if self.sweep_due(store) {
            events.extend(self.sweep(store)?);
        }
        events.extend(self.tick(store)?);
        Ok(events)
    }

    /// Ask each transport that has a hook-made session row what it can see, once, and
    /// record the answer for every such row. Nothing else observes. Returns the sessions
    /// confirmed gone.
    pub fn sweep(&mut self, store: &Store) -> Result<Vec<PushEvent>> {
        self.last_sweep = Some(store.now());
        let mut names: Vec<String> = store
            .transport_sessions()?
            .into_iter()
            .filter(|r| r.hook_made())
            .map(|r| r.transport)
            .collect();
        names.sort();
        names.dedup();
        let confirm_after = self.confirm_after.map(|d| d.as_millis() as i64);
        let mut events = Vec::new();
        for name in names {
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            let Some(transport) = self.transport(&name) else {
                continue;
            };
            let confirmed = match transport.list(Some(&self.cancel)) {
                ListResult::Sessions(listed) => {
                    store.observe_transport_sessions(&name, Ok(&listed), confirm_after)?
                }
                ListResult::NoAnswer(why) => {
                    store.observe_transport_sessions(&name, Err(&why), confirm_after)?
                }
                ListResult::Unsupported => Vec::new(),
            };
            events.extend(confirmed.into_iter().map(|row| PushEvent {
                agent: row.agent,
                target: row.target,
                action: "gone".into(),
                detail: format!("session {} confirmed gone", row.session_id),
            }));
        }
        Ok(events)
    }

    /// One wake pass. Starts no process unless some agent has mail that nobody has
    /// announced yet and a session that may be woken. Stops between agents when a sweep
    /// has become due, so failing wakes cannot starve the sweep. Returns decisions that
    /// changed since the last pass.
    pub fn tick(&mut self, store: &Store) -> Result<Vec<PushEvent>> {
        let mut events = Vec::new();
        let due_at_start = self.sweep_due(store);
        let mut agents: Vec<String> = store
            .transport_sessions()?
            .into_iter()
            .filter(|r| r.hook_made())
            .map(|r| r.agent)
            .collect();
        agents.sort();
        agents.dedup();
        for agent in agents {
            if self.cancel.load(Ordering::SeqCst) || (!due_at_start && self.sweep_due(store)) {
                break;
            }
            let Ok(Some(waiting)) = waiting(store, &agent) else {
                self.seen.remove(&agent);
                self.last_action.remove(&agent);
                continue;
            };
            let now = store.now();
            let newest = &waiting.message_ids[0];
            let first_seen = match self.seen.get(&agent) {
                Some((id, at)) if id == newest => *at,
                _ => {
                    self.seen.insert(agent.clone(), (newest.clone(), now));
                    now
                }
            };
            if now - first_seen < self.settle.as_millis() as i64
                || self.next_check.get(&agent).is_some_and(|at| now < *at)
            {
                continue;
            }
            let (action, target, detail) = self.pass(store, &agent, &waiting)?;
            // A wake call is always reported; a pass that made none only when it changed.
            let woke = WakeOutcome::parse(&action).is_some();
            if woke || self.last_action.get(&agent) != Some(&action) {
                self.last_action.insert(agent.clone(), action.clone());
                events.push(PushEvent {
                    agent,
                    target,
                    action,
                    detail,
                });
            }
        }
        Ok(events)
    }

    /// Steps 1 to 4 for one agent: (action, target, detail).
    fn pass(
        &mut self,
        store: &Store,
        agent: &str,
        waiting: &Waiting,
    ) -> Result<(String, String, String)> {
        let now = store.now();
        let rows = store.transport_sessions()?;
        let held = |row: &TransportSession| {
            if self.unrecorded.contains(&key(row)) {
                Some("its last wake could not be recorded")
            } else if self.refused.get(&key(row)).is_some_and(|at| now < *at) {
                Some("the pane did not show this session idle a moment ago")
            } else {
                None
            }
        };
        let row = match pick_row(&rows, agent, None, now, held) {
            Ok(row) => row,
            Err((why, working)) => {
                // A working sibling gets the mail from its post-tool hook.
                if working {
                    self.next_check
                        .insert(agent.to_string(), now + self.recheck.as_millis() as i64);
                }
                let action = if working { "working" } else { "skipped" };
                return Ok((action.into(), String::new(), why));
            }
        };
        let target = row.target.clone();
        let Some(transport) = self.transport(&row.transport) else {
            return Ok(("skipped".into(), target, "no such transport".into()));
        };
        let Some(line) = default_message(store, agent)?.0 else {
            return Ok(("skipped".into(), target, "nothing unread".into()));
        };
        let step = wake_row(
            store,
            transport,
            row,
            &rows,
            &line,
            Some(&waiting.message_ids),
            "daemon",
            Some(&self.cancel),
        )?;
        Ok(match step {
            Step::Refused(why) => {
                self.refused.insert(key(row), now + REFUSED_MS);
                ("skipped".into(), target, why)
            }
            Step::AlreadyAnnounced => (
                "skipped".into(),
                target,
                "already announced by a hook".into(),
            ),
            Step::Woke { result, recorded } => {
                if !recorded {
                    self.unrecorded.insert(key(row));
                }
                (result.outcome.as_str().into(), target, result.detail)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_commands() {
        let t = commands(WakeVia::Tmux, "%3", "hi");
        assert_eq!(t[0], ["tmux", "send-keys", "-t", "%3", "-l", "hi"]);
        assert_eq!(t[1], ["tmux", "send-keys", "-t", "%3", "Enter"]);
        let c = commands(WakeVia::Cmux, "surface-1", "hi");
        assert_eq!(c[0], ["cmux", "send", "--surface", "surface-1", "hi\\n"]);
        let n = commands(WakeVia::Cmux, "notify", "hi");
        assert_eq!(n[0][1], "notify");
        let h = commands(WakeVia::Herdr, "reviewer", "hi");
        assert_eq!(&h[0][1..6], ["agent", "prompt", "reviewer", "hi", "--wait"]);
    }
}
