//! `another wake`: nudge an agent's terminal when it has mail, through optional adapters
//! (tmux, cmux, Herdr). Hooks remain the primary delivery path; this is for agents that sit
//! idle between prompts. Commands run as argv (no shell); nothing runs without a reason
//! unless `--force` is given.
//!
//! `another serve` also runs a [`Waker`]: it prompts an idle Herdr agent when mail arrives
//! that no hook has announced yet. Herdr reports each agent's state, so a working agent is
//! left to its post-tool hook and a blocked one is not interrupted. Herdr is only ever
//! called with a subcommand (`agent get`, `agent prompt`), never bare, and never to focus.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use another_core::{InboxQuery, Store, WakeVia};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct WakePlan {
    pub agent: String,
    pub via: Option<WakeVia>,
    pub target: Option<String>,
    pub message: Option<String>,
    /// The commands that run (or would run, with --dry-run).
    pub commands: Vec<Vec<String>>,
    pub sent: bool,
    /// Why nothing was sent.
    pub skipped: Option<String>,
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
}

/// The adapter commands for a wake.
pub fn commands(via: WakeVia, target: &str, message: &str) -> Vec<Vec<String>> {
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
            s(message),
        ]],
        // `cmux send` unescapes `\n`, which submits the line.
        WakeVia::Cmux => vec![vec![
            s("cmux"),
            s("send"),
            s("--surface"),
            s(target),
            format!("{message}\\n"),
        ]],
        WakeVia::Herdr => {
            let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| s("herdr"));
            vec![vec![herdr, s("agent"), s("prompt"), s(target), s(message)]]
        }
    }
}

fn default_message(store: &Store, agent: &str) -> Result<(Option<String>, String)> {
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
        .map(|m| format!("\"{}\"", m.message.subject))
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

/// Plan (and unless `dry_run`, send) a wake for one agent.
pub fn wake(store: &Store, agent: &str, opts: &WakeOptions) -> Result<WakePlan> {
    let stored = store.wake_target(agent)?;
    let (auto_message, address) = default_message(store, agent)?;
    let mut plan = WakePlan {
        agent: address.clone(),
        via: opts.via.or(stored.as_ref().map(|t| t.via)),
        target: opts
            .target
            .clone()
            .or_else(|| stored.as_ref().map(|t| t.target.clone())),
        message: opts.message.clone().or(auto_message),
        commands: Vec::new(),
        sent: false,
        skipped: None,
    };
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
    if plan.target.is_none() && via == WakeVia::Herdr {
        // Herdr addresses agents by name; default to the Another agent's name.
        plan.target = Some(address.split('@').next().unwrap_or(&address).to_string());
    }
    let Some(target) = plan.target.clone() else {
        plan.skipped = Some(format!("no {} target for {address}", via.as_str()));
        return Ok(plan);
    };
    plan.commands = commands(via, &target, plan.message.as_deref().unwrap_or_default());
    if opts.dry_run {
        plan.skipped = Some("dry run".into());
        return Ok(plan);
    }
    for argv in &plan.commands {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .with_context(|| format!("running {} (is it installed?)", argv[0]))?;
        if !status.success() {
            bail!("{} exited with {status}", argv.join(" "));
        }
    }
    plan.sent = true;
    Ok(plan)
}

/// Wake every agent with a stored target that has something waiting.
pub fn wake_all(store: &Store, opts: &WakeOptions) -> Result<Vec<WakePlan>> {
    store
        .wake_targets()?
        .into_iter()
        .map(|t| wake(store, &t.agent, opts))
        .collect()
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

/// Record the terminal a hook runs in, so `another wake` can find the agent later.
pub fn capture_target(store: &Store, agent: &str) {
    if let Some((via, target)) = current_target() {
        let _ = store.set_wake_target(agent, via, &target, true);
    }
}

/// At session end, forget the captured target if it is still this terminal, so nothing
/// is typed into the pane after the agent has left it.
pub fn release_target(store: &Store, agent: &str) {
    if let Some((via, target)) = current_target() {
        let _ = store.clear_auto_wake_target(agent, Some((via, &target)));
    }
}

/// The herdr binary: `$HERDR_BIN_PATH`, else `herdr` on PATH.
fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "herdr".to_string())
}

/// What a terminal adapter says about a stored target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Herdr: an agent runs there, with its program (`claude`, `codex`, ...) and status
    /// (`idle`, `working`, `blocked`, `done`, `unknown`). tmux: the pane exists.
    Live {
        program: Option<String>,
        status: String,
    },
    /// The adapter answered that the pane is gone (or, for Herdr, has no agent now).
    Gone,
    /// The adapter could not be asked (not installed, server not running).
    Unknown(String),
}

/// Ask Herdr about the agent in a pane: `herdr agent get <target>` (read only).
pub fn probe_herdr(target: &str) -> Probe {
    let out = match Command::new(herdr_bin())
        .args(["agent", "get", target])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => return Probe::Unknown(format!("herdr: {e}")),
    };
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if let Some(agent) = reply.pointer("/result/agent") {
        let text = |k: &str| agent.get(k).and_then(Value::as_str).map(str::to_string);
        return Probe::Live {
            program: text("agent"),
            status: text("agent_status").unwrap_or_else(|| "unknown".into()),
        };
    }
    match reply.pointer("/error/code").and_then(Value::as_str) {
        Some("agent_not_found") => Probe::Gone,
        Some(code) => Probe::Unknown(format!("herdr: {code}")),
        None => Probe::Unknown(format!("herdr exited with {}", out.status)),
    }
}

/// Does a tmux pane exist? `tmux display-message -p -t <pane>` (read only).
pub fn probe_tmux(target: &str) -> Probe {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_id}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Probe::Live {
            program: None,
            status: "unknown".into(),
        },
        // tmux answers but cannot find the pane (or its server is gone with it).
        Ok(_) => Probe::Gone,
        Err(e) => Probe::Unknown(format!("tmux: {e}")),
    }
}

fn probe(via: WakeVia, target: &str) -> Probe {
    match via {
        WakeVia::Herdr => probe_herdr(target),
        WakeVia::Tmux => probe_tmux(target),
        // cmux has no read-only pane query we rely on; staleness decides for it.
        WakeVia::Cmux => Probe::Unknown("cmux targets are not probed".into()),
    }
}

/// An automatic target whose agent has not been seen for this long is stale.
const STALE_AFTER_MS: i64 = 24 * 3_600_000;

#[derive(Debug, Clone, Serialize)]
pub struct PruneRow {
    pub agent: String,
    pub via: WakeVia,
    pub target: String,
    pub pruned: bool,
    pub reason: String,
}

/// Drop automatically captured targets that can no longer be reached: the adapter says
/// the pane is gone, or the agent ended its session or has not been seen for a day.
/// Targets set by hand are listed but never removed.
pub fn prune(store: &Store, dry_run: bool) -> Result<Vec<PruneRow>> {
    let mut rows = Vec::new();
    for t in store.wake_targets()? {
        let mut row = PruneRow {
            agent: t.agent.clone(),
            via: t.via,
            target: t.target.clone(),
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
        let stale = match probe(t.via, &t.target) {
            Probe::Gone => Some(format!("{} reports the pane is gone", t.via.as_str())),
            _ if agent.is_none() => Some("agent is no longer registered".into()),
            _ if offline => Some("agent ended its session".into()),
            _ if unseen => Some("agent not seen for over a day".into()),
            Probe::Live { .. } => None,
            Probe::Unknown(why) => {
                row.reason = format!("kept ({why})");
                None
            }
        };
        if let Some(reason) = stale {
            if !dry_run {
                store.clear_auto_wake_target(&t.agent, Some((t.via, &t.target)))?;
            }
            row.pruned = true;
            row.reason = reason;
        } else if row.reason.is_empty() {
            row.reason = "live".into();
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One decision the daemon's waker made.
#[derive(Debug, Clone, Serialize)]
pub struct PushEvent {
    pub agent: String,
    pub target: String,
    /// `woke`, `working`, `blocked`, `pruned`, `skipped` or `failed`.
    pub action: String,
    pub detail: String,
}

/// The daemon side of push delivery: wake idle Herdr agents that have mail no hook or wake
/// has announced yet. Holds per-agent timing in memory; the announced cursor is shared
/// with the hooks through `$ANOTHER_HOME/hooks/state`.
pub struct Waker {
    another_home: PathBuf,
    /// A burst of mail settles for this long before one wake covers all of it.
    pub settle: Duration,
    /// After a wake (or a failed one), leave the agent alone this long.
    pub min_gap: Duration,
    /// After finding the agent busy, look again this soon.
    pub recheck: Duration,
    /// Newest unannounced message per agent, and when it was first seen.
    seen: HashMap<String, (String, Instant)>,
    /// Do not probe an agent again before this.
    next_check: HashMap<String, Instant>,
    /// Last decision per agent, so only changes are reported.
    last_action: HashMap<String, String>,
}

impl Waker {
    pub fn new(another_home: PathBuf) -> Waker {
        Waker {
            another_home,
            settle: Duration::from_secs(2),
            min_gap: Duration::from_secs(60),
            recheck: Duration::from_secs(5),
            seen: HashMap::new(),
            next_check: HashMap::new(),
            last_action: HashMap::new(),
        }
    }

    /// One pass over the Herdr targets. Spawns nothing unless some agent has mail that
    /// nobody has announced yet. Returns decisions that changed since the last pass.
    pub fn tick(&mut self, store: &Store) -> Result<Vec<PushEvent>> {
        let mut events = Vec::new();
        for t in store.wake_targets()? {
            if t.via != WakeVia::Herdr {
                continue;
            }
            let cursor = crate::hooks::announced_cursor(&self.another_home, &t.agent);
            let Ok(unread) = store.unread_after(&t.agent, &cursor, 50) else {
                continue;
            };
            let Some(newest) = unread.items.first() else {
                self.seen.remove(&t.agent);
                self.last_action.remove(&t.agent);
                continue;
            };
            let first_seen = match self.seen.get(&t.agent) {
                Some((id, at)) if *id == newest.message_id => *at,
                _ => {
                    let now = Instant::now();
                    self.seen
                        .insert(t.agent.clone(), (newest.message_id.clone(), now));
                    now
                }
            };
            if first_seen.elapsed() < self.settle
                || self
                    .next_check
                    .get(&t.agent)
                    .is_some_and(|at| Instant::now() < *at)
            {
                continue;
            }
            let ids: Vec<String> = unread.items.iter().map(|i| i.message_id.clone()).collect();
            let (action, detail, wait) = self.decide(store, &t, &ids)?;
            self.next_check
                .insert(t.agent.clone(), Instant::now() + wait);
            if self.last_action.get(&t.agent) != Some(&action) || action == "woke" {
                self.last_action.insert(t.agent.clone(), action.clone());
                events.push(PushEvent {
                    agent: t.agent.clone(),
                    target: t.target.clone(),
                    action,
                    detail,
                });
            }
        }
        Ok(events)
    }

    /// Probe one target and act: (action, detail, how long until the next look).
    fn decide(
        &self,
        store: &Store,
        t: &another_core::WakeTarget,
        message_ids: &[String],
    ) -> Result<(String, String, Duration)> {
        let newest = message_ids.first().map(String::as_str).unwrap_or_default();
        // Evidence for `another stats`; a wake goes ahead even if it cannot be recorded.
        let record = |outcome: &str, detail: &str| {
            if let Err(e) =
                store.record_wake(&t.agent, t.via, &t.target, outcome, detail, message_ids)
            {
                tracing::warn!(agent = %t.agent, error = %e, "wake not recorded for stats");
            }
        };
        let program = store.agent(&t.agent).map(|a| a.program).ok();
        let out = |a: &str, d: String, w: Duration| Ok((a.to_string(), d, w));
        match probe_herdr(&t.target) {
            Probe::Gone if t.auto => {
                store.clear_auto_wake_target(&t.agent, Some((t.via, &t.target)))?;
                out(
                    "pruned",
                    "herdr reports the pane is gone".into(),
                    self.min_gap,
                )
            }
            Probe::Gone => out(
                "skipped",
                "herdr reports the pane is gone (target set by hand)".into(),
                self.min_gap,
            ),
            Probe::Unknown(why) => out("skipped", why, self.recheck),
            Probe::Live {
                program: pane_program,
                status,
            } => {
                let mismatch = matches!(
                    (program.as_deref(), pane_program.as_deref()),
                    (Some(ours @ ("claude" | "codex" | "grok" | "cursor")), Some(theirs))
                        if ours != theirs
                );
                if mismatch {
                    if t.auto {
                        store.clear_auto_wake_target(&t.agent, Some((t.via, &t.target)))?;
                    }
                    return out(
                        "pruned",
                        format!("the pane now runs {}", pane_program.unwrap_or_default()),
                        self.min_gap,
                    );
                }
                match status.as_str() {
                    "idle" | "done" => match wake(store, &t.agent, &WakeOptions::default()) {
                        Ok(plan) if plan.sent => {
                            crate::hooks::mark_announced(&self.another_home, &t.agent, newest)?;
                            let detail = format!("agent was {status}");
                            record("sent", &detail);
                            out("woke", detail, self.min_gap)
                        }
                        Ok(plan) => out("skipped", plan.skipped.unwrap_or_default(), self.min_gap),
                        Err(e) => {
                            let detail = format!("{e:#}");
                            record("failed", &detail);
                            out("failed", detail, self.min_gap)
                        }
                    },
                    // A working agent gets the mail from its post-tool hook; a blocked one
                    // is waiting on a person and must not be interrupted.
                    "working" => out("working", "left to the post-tool hook".into(), self.recheck),
                    "blocked" => out("blocked", "waiting on a person".into(), self.recheck),
                    other => out("skipped", format!("agent state {other}"), self.recheck),
                }
            }
        }
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
        assert_eq!(&h[0][1..], ["agent", "prompt", "reviewer", "hi"]);
    }
}
