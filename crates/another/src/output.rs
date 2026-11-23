//! Human-readable rendering for CLI output (`--json` bypasses all of this).

use std::fmt::Write;

use another_core::{
    ActivityFeed, Agent, AuditEntry, Event, HeartbeatReport, Hold, HoldConflict, HoldReport, Inbox,
    LeaseOutcome, ListedHold, Message, NodeInfo, Peer, Project, QueueAudit, QueueDelegate,
    QueueGrace, QueueItem, QueueLimits, QueueNeedsOwner, QueueNext, QuietClaim, Request, Resource,
    Thread, ThreadSummary, Token, WakeTarget,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Status {
    pub node: NodeInfo,
    pub db_path: String,
    pub projects: Vec<Project>,
    pub agents: Vec<Agent>,
    pub actors: Vec<another_core::ActorView>,
    pub leases: Vec<another_core::RightLease>,
    pub operations: Vec<another_core::OperationView>,
    pub authority_now: i64,
}

impl Status {
    /// Repository attribution comes from file evidence, never a display name.
    /// Include parents for lifecycle context and other rights of attributed actors.
    pub fn retain_repo(&mut self, repo: &std::path::Path) -> anyhow::Result<()> {
        let root = std::fs::canonicalize(repo)?;
        anyhow::ensure!(root.is_dir(), "--repo must name a directory");
        let folded_root = root.to_string_lossy().to_lowercase();
        let folds = std::fs::canonicalize(&folded_root).is_ok_and(|other| other == root);
        let under = |key: &str| {
            let path = std::path::Path::new(key);
            path.starts_with(&root) || (folds && path.starts_with(&folded_root))
        };
        let files: std::collections::BTreeSet<_> = self
            .leases
            .iter()
            .filter(|l| l.subject_kind == another_core::SubjectKind::File && under(&l.subject_key))
            .map(|l| l.lease_id.clone())
            .collect();
        let mut actors: std::collections::BTreeSet<_> = self
            .leases
            .iter()
            .filter(|l| files.contains(&l.lease_id))
            .map(|l| l.owner_actor_id.clone())
            .collect();
        loop {
            let before = actors.len();
            for actor in &self.actors {
                if actors.contains(&actor.actor_id) {
                    if let Some(parent) = &actor.parent_actor_id {
                        actors.insert(parent.clone());
                    }
                }
            }
            if before == actors.len() {
                break;
            }
        }
        self.actors.retain(|a| actors.contains(&a.actor_id));
        self.agents.retain(|a| actors.contains(&a.agent_id));
        self.leases.retain(|l| {
            files.contains(&l.lease_id)
                || (l.subject_kind != another_core::SubjectKind::File
                    && actors.contains(&l.owner_actor_id))
        });
        self.operations
            .retain(|op| op.targets.iter().any(|r| files.contains(&r.lease_id)));
        self.projects
            .retain(|p| std::path::Path::new(&p.abs_path).starts_with(&root));
        Ok(())
    }
}

pub fn status(s: &Status) -> String {
    let mut out = format!(
        "node      {} ({}, {})\ndatabase  {}\nprojects  {}\nagents    {}\n",
        s.node.node_id,
        s.node.display_name,
        s.node.role,
        s.db_path,
        s.projects.len(),
        s.agents.len()
    );
    if !s.agents.is_empty() {
        out.push('\n');
        out.push_str(&agents(&s.agents));
    }
    for actor in &s.actors {
        let state = if actor.closed_at.is_some() {
            "closed"
        } else if actor.unused_stale {
            "unused/stale"
        } else {
            "live"
        };
        let _ = writeln!(
            out,
            "actor {} {} scope {} epoch {} {state}",
            actor.actor_id, actor.name, actor.session_scope_id, actor.session_epoch
        );
        if let Some(reminder) = &actor.reminder {
            let _ = writeln!(out, "{reminder}");
        }
    }
    for lease in &s.leases {
        let state = if let Some(reason) = &lease.release_reason {
            format!("released:{reason}")
        } else if lease.expires_at <= s.authority_now {
            "expired/nonblocking".into()
        } else {
            "live".into()
        };
        let _ = writeln!(
            out,
            "lease {} generation {} ttl {}s updated {} expires {} {state} subject {}",
            lease.lease_id,
            lease.generation,
            lease.ttl_seconds,
            lease.last_update_at,
            lease.expires_at,
            another_core::display_line(&lease.subject_key, 200)
        );
    }
    let _ = writeln!(out, "Cursor: {}", another_core::CURSOR_COVERAGE);
    let _ = writeln!(out, "Admission-time structured-edit coverage only; native permission delay may outlast a lease. Shell caller carriage is unavailable.");
    for operation in &s.operations {
        let _ = writeln!(
            out,
            "operation {} actor {} epoch {} targets {} completed {}",
            another_core::display_line(&operation.call_id, 100),
            operation.actor_id,
            operation.session_epoch,
            operation.targets.len(),
            operation.completed_at.map_or_else(
                || "unknown/nonblocking after lease expiry".into(),
                |at| at.to_string()
            )
        );
    }
    out
}

pub fn agents(agents: &[Agent]) -> String {
    agents
        .iter()
        .map(|a| {
            format!(
                "{:<24} {:<9} {:<8} last seen {}\n",
                a.address,
                a.program,
                a.status,
                a.last_seen_at.as_deref().unwrap_or("never")
            )
        })
        .collect()
}

pub fn sent(m: &Message) -> String {
    let mut out = format!("sent {} to {}", m.message_id, m.to.join(", "));
    if !m.cc.is_empty() {
        let _ = write!(out, " (cc {})", m.cc.join(", "));
    }
    if let Some(r) = &m.request_id {
        let _ = write!(out, "\nopened request {r}");
    }
    out
}

fn flag(m: &Message) -> &'static str {
    match m.importance {
        another_core::Importance::Urgent => "!!",
        another_core::Importance::High => "! ",
        _ => "  ",
    }
}

pub fn inbox(i: &Inbox) -> String {
    let mut out = format!(
        "{}: {} unread ({} urgent), {} awaiting ack, {} request(s)\n",
        i.agent,
        i.unread,
        i.urgent_unread,
        i.awaiting_ack,
        i.requests.len()
    );
    for item in &i.messages {
        let m = &item.message;
        let state = if item.read_at.is_none() {
            "new"
        } else if m.ack_required && item.acked_at.is_none() {
            "ack?"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{} {:<4} {}  {:<16} {}  [{}]",
            flag(m),
            state,
            m.created_at,
            m.from,
            m.subject,
            m.message_id
        );
    }
    if !i.requests.is_empty() {
        out.push_str("\nrequests:\n");
        out.push_str(&requests(&i.requests));
    }
    out
}

pub fn threads(ts: &[ThreadSummary]) -> String {
    ts.iter()
        .map(|t| {
            format!(
                "{}  {:>3} msg {:>3} unread  {}  ({})  [{}]\n",
                t.last_at,
                t.messages,
                t.unread,
                t.subject,
                t.participants.join(", "),
                t.thread_id
            )
        })
        .collect()
}

pub fn thread(t: &Thread) -> String {
    let mut out = String::new();
    for m in &t.messages {
        let _ = writeln!(
            out,
            "{}{} {} -> {}  [{}]\n  {}\n",
            flag(m).trim(),
            m.created_at,
            m.from,
            m.to.join(", "),
            m.message_id,
            m.subject
        );
        for line in m.body.lines() {
            let _ = writeln!(out, "  {line}");
        }
        out.push('\n');
    }
    out
}

pub fn requests(rs: &[Request]) -> String {
    rs.iter()
        .map(|r| {
            let owner = r.owner.as_deref().unwrap_or("-");
            let due = r
                .due_at
                .as_deref()
                .map(|d| format!(" due {d}"))
                .unwrap_or_default();
            format!(
                "{:<9} {:<16} {}  (from {}{due})  [{}]\n",
                r.status.as_str(),
                owner,
                r.title,
                r.created_by,
                r.request_id
            )
        })
        .collect()
}

fn hold_row(h: &Hold) -> String {
    format!(
        "{:<9} {:<16} {}:{}  until {}{}  [{}]",
        if h.exclusive { "exclusive" } else { "shared" },
        h.agent,
        h.project,
        h.pattern,
        h.expires_at,
        h.reason
            .as_deref()
            .map(|r| format!("  ({r})"))
            .unwrap_or_default(),
        h.hold_id
    )
}

pub fn holds(hs: &[Hold]) -> String {
    hs.iter().map(|h| hold_row(h) + "\n").collect()
}

/// `another holds`: the rows of [`holds`], with the holds of other checkouts marked and one
/// line saying what the mark means when any row carries it.
pub fn listed_holds(hs: &[ListedHold]) -> String {
    let mut out: String = hs
        .iter()
        .map(|h| {
            let mark = if h.other_checkout {
                "  other checkout"
            } else {
                ""
            };
            format!("{}{mark}\n", hold_row(&h.hold))
        })
        .collect();
    if hs.iter().any(|h| h.other_checkout) {
        out.push_str(
            "other checkout: taken in a different working directory from this one; it does \
             not block edits here\n",
        );
    }
    out
}

pub fn conflicts(cs: &[HoldConflict]) -> String {
    cs.iter()
        .map(|c| {
            // A hold whose pattern could not be read: the note says whose it is, on which
            // node, and how to clear it.
            if let Some(note) = &c.unreadable {
                return format!("conflict: {}: {note}\n", c.path);
            }
            // The project matters: the holder may be in another project of the checkout,
            // one registered at a subdirectory of it.
            format!(
                "conflict: {} is held by {} ({}:{}, {} until {})\n",
                c.path,
                c.held.agent,
                c.held.project,
                c.held.pattern,
                if c.held.exclusive {
                    "exclusive"
                } else {
                    "shared"
                },
                c.held.expires_at
            )
        })
        .collect()
}

pub fn hold_report(r: &HoldReport) -> String {
    let mut out = if r.conflicts.is_empty() {
        holds(&r.granted)
    } else {
        format!("nothing acquired\n{}", conflicts(&r.conflicts))
    };
    for w in &r.warnings {
        if let Some(note) = &w.unreadable {
            let _ = writeln!(
                out,
                "warning: {}: {note} (holds are node-local; this one does not block you)",
                w.path
            );
            continue;
        }
        let _ = writeln!(
            out,
            "warning: {} overlaps {} held on another node by {} (holds are node-local; coordinate)",
            w.path, w.held.pattern, w.held.agent
        );
    }
    if r.warnings_more > 0 {
        let _ = writeln!(
            out,
            "warning: {} more hold(s) on other nodes overlap and are not listed",
            r.warnings_more
        );
    }
    if !r.not_compared.is_empty() {
        // One line however many holds and nodes there are: a peer chooses both.
        let total: u64 = r.not_compared.iter().map(|n| u64::from(n.holds)).sum();
        let mut from: Vec<String> = r
            .not_compared
            .iter()
            .take(5)
            .map(|n| {
                format!(
                    "{} from {}",
                    n.holds,
                    another_core::display_line(&n.node, 64)
                )
            })
            .collect();
        if r.not_compared.len() > 5 {
            from.push(format!(
                "the rest from {} more nodes",
                r.not_compared.len() - 5
            ));
        }
        let _ = writeln!(
            out,
            "warning: {total} hold(s) on other nodes were not compared with yours ({}): \
             comparing with other nodes' holds stops at a fixed amount of work, so these may \
             overlap (holds are node-local; they do not block you)",
            from.join(", ")
        );
    }
    out
}

pub fn heartbeat(h: &HeartbeatReport) -> String {
    let holds = if h.status == "offline" {
        format!("released {} hold(s)", h.released_holds.len())
    } else {
        format!("renewed {} hold(s)", h.renewed_holds.len())
    };
    format!(
        "{} {}: {holds}; {} unread ({} urgent), {} open request(s)",
        h.agent, h.status, h.unread, h.urgent_unread, h.open_requests
    )
}

pub fn events(es: &[Event]) -> String {
    es.iter()
        .map(|e| {
            format!(
                "{:>6} {}  {:<18} {:<16} {}\n",
                e.seq, e.ts, e.kind, e.actor, e.event_id
            )
        })
        .collect()
}

pub fn activity(f: &ActivityFeed) -> String {
    f.items
        .iter()
        .map(|i| format!("{}  {:<8} {}\n", i.ts, i.category.as_str(), i.summary))
        .collect()
}

#[derive(Debug, Serialize)]
pub struct HookStatus {
    pub client: String,
    pub installed: bool,
    pub config: String,
    /// Configuration presence does not verify whether Codex trusted or ran the hooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_note: Option<&'static str>,
}

pub fn hook_status(rows: &[HookStatus]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "{:<7} {:<13} {}\n{}",
                r.client,
                if r.installed {
                    "installed"
                } else {
                    "not installed"
                },
                r.config,
                r.trust_note
                    .map(|note| format!("  {note}\n"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

pub fn hook_report(r: &crate::hooks::HookReport) -> String {
    let mut out = String::new();
    if r.dry_run {
        out.push_str("dry run: nothing written\n");
    }
    for c in &r.changes {
        let verb = match c.change {
            crate::hooks::ChangeKind::Create => "create",
            crate::hooks::ChangeKind::Update => "update",
            crate::hooks::ChangeKind::Delete => "delete",
            crate::hooks::ChangeKind::Unchanged => "unchanged",
        };
        let _ = write!(out, "{verb:<9} {}", c.path);
        if let Some(b) = &c.backup {
            let _ = write!(out, "  (backup: {b})");
        }
        out.push('\n');
    }
    for n in &r.notes {
        let _ = writeln!(out, "note: {n}");
    }
    out
}

pub fn resources(rs: &[Resource]) -> String {
    if rs.is_empty() {
        return "no resources registered".to_string();
    }
    rs.iter()
        .map(|r| {
            let who = match (&r.owner, r.refcount) {
                (Some(o), n) if n > 1 => format!("{o} +{} borrower(s)", n - 1),
                (Some(o), _) => o.clone(),
                (None, _) => "-".to_string(),
            };
            format!(
                "{:<8} {:<12} {:<26} {:<9} {:<28} {}{}  [{}]\n",
                r.status,
                r.kind.as_str(),
                r.name,
                r.mode.as_str(),
                who,
                r.endpoint.as_deref().unwrap_or(""),
                r.purpose
                    .as_deref()
                    .map(|p| format!("  ({p})"))
                    .unwrap_or_default(),
                r.resource_id
            )
        })
        .collect()
}

pub fn lease_outcome(o: &LeaseOutcome) -> String {
    let lease = o
        .lease
        .as_ref()
        .map(|l| format!(" as {} until {}", l.role, l.expires_at))
        .unwrap_or_default();
    format!(
        "{} {} {}{lease}\n{}",
        o.outcome,
        o.resource.kind.as_str(),
        o.resource.name,
        resources(std::slice::from_ref(&o.resource))
    )
}

pub fn orphans(r: &crate::reap::OrphanReport) -> String {
    let mut out = String::new();
    if r.orphans.is_empty() {
        out.push_str("no orphans found\n");
    }
    for o in &r.orphans {
        let _ = writeln!(
            out,
            "{:<22} {:<28} pid {:<7} port {:<6} age {}s  {}",
            o.source,
            o.target,
            o.pid.map_or("-".into(), |p| p.to_string()),
            o.port.map_or("-".into(), |p| p.to_string()),
            o.age_seconds.map_or("?".into(), |a| a.to_string()),
            o.command.as_deref().unwrap_or("")
        );
    }
    if let Some(scan) = &r.scan {
        if let Some(e) = &scan.error {
            let _ = writeln!(out, "scan unavailable: {e}");
        }
    }
    out
}

pub fn reap_plan(p: &crate::reap::ReapPlan) -> String {
    let mut out = format!(
        "target {} (pid {}): {}\n",
        p.target,
        p.pid.map_or("-".into(), |x| x.to_string()),
        p.command.as_deref().unwrap_or("unknown command")
    );
    if let Some(o) = &p.outcome {
        let _ = writeln!(out, "{o}");
    } else if p.eligible {
        let message = if cfg!(windows) {
            "eligible; nothing was done. Re-run with --confirm to terminate the process. Windows drops hard."
        } else {
            "eligible; nothing was done. Re-run with --confirm to send SIGTERM."
        };
        let _ = writeln!(out, "{message}");
    } else {
        for b in &p.blockers {
            let _ = writeln!(out, "blocked: {b}");
        }
    }
    out
}

pub fn tokens(ts: &[Token]) -> String {
    if ts.is_empty() {
        return "no tokens".to_string();
    }
    ts.iter()
        .map(|t| {
            let state = if t.revoked_at.is_some() {
                "revoked"
            } else {
                "active"
            };
            format!(
                "{:<8} {}  {:<20} projects [{}]  last used {}{}\n",
                state,
                t.token_id,
                t.agent,
                t.project_globs.join(", "),
                t.last_used_at.as_deref().unwrap_or("never"),
                t.label
                    .as_deref()
                    .map(|l| format!("  ({l})"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

pub fn audit(es: &[AuditEntry]) -> String {
    es.iter()
        .map(|e| {
            format!(
                "{}  {:<7} {:<16} {:<32} {}{}\n",
                e.ts,
                e.outcome,
                e.agent.as_deref().unwrap_or("-"),
                e.operation,
                // A detail can quote stored text, such as an OAuth client's name: one row
                // is one line. Nothing is cut: a detail can end with a redirect host.
                another_core::display_line(e.detail.as_deref().unwrap_or(""), usize::MAX),
                e.remote
                    .as_deref()
                    .map(|r| format!("  [{r}]"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// `own_id` is this node's id: a pairing row for the node itself, which an older build
/// could write, is shown marked, because nothing acts on it. `unchecked_hub` is the id of
/// this node's hub when it was joined before invites were signed: its row is marked too.
pub fn peers(ps: &[Peer], own_id: &str, unchecked_hub: Option<&str>) -> String {
    if ps.is_empty() {
        return "not paired with any node".to_string();
    }
    ps.iter()
        .map(|p| {
            let perms: Vec<&str> = p.perms.iter().map(|x| x.as_str()).collect();
            format!(
                "{:<6} {:<16} {}  key {}  perms [{}]  last sync {}{}{}{}\n",
                p.role,
                p.display_name,
                p.url.as_deref().unwrap_or("-"),
                p.fingerprint,
                perms.join(", "),
                p.last_sync_at.as_deref().unwrap_or("never"),
                if p.revoked_at.is_some() {
                    "  (revoked)"
                } else {
                    ""
                },
                if unchecked_hub == Some(p.node_id.as_str()) {
                    "  (joined before invites were signed; address not checked)"
                } else {
                    ""
                },
                if p.node_id == own_id {
                    "  (this node itself; ignored)"
                } else {
                    ""
                }
            )
        })
        .collect()
}

pub fn sync_report(r: &crate::fed::SyncReport) -> String {
    let mut out = format!(
        "synced with {}: pushed {} ({} already there), pulled {} (applied {}, {} duplicate)",
        r.hub, r.pushed, r.push_duplicates, r.pulled, r.applied, r.pull_duplicates
    );
    for x in r.push_rejected.iter().chain(&r.pull_rejected) {
        let _ = write!(out, "\nrejected {}: {}", x.event_id, x.reason);
    }
    for x in &r.not_applied {
        let _ = write!(out, "\nnot applied {}: {}", x.event_id, x.reason);
    }
    // Every unacknowledged skip, after every sync, until `another sync ack`.
    if !r.skipped.is_empty() {
        let lines = another_core::sync_skip_lines(&r.skipped, r.skipped_total);
        let _ = write!(out, "\n{lines}");
    }
    out
}

/// One stored wake target as `another wake list` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct WakeTargetRow {
    #[serde(flatten)]
    pub target: WakeTarget,
    /// Whether a transport of that name exists now: `herdr`, or one in `transports.json`.
    pub configured: bool,
    /// What is wrong with the target, if anything, and what to set.
    pub note: Option<String>,
}

impl WakeTargetRow {
    pub fn new(target: WakeTarget, config: &crate::transport::Config) -> WakeTargetRow {
        let via = target.via.as_str();
        let configured = config.provides(via);
        let note = if !configured {
            Some(config.why_missing(via))
        } else if crate::wake::is_old_cmux_notify(via, &target.target) {
            Some(
                "the target `notify` no longer means a notification: set it again with a \
                 transport of its own (docs/WAKE.md has `cmux-notify`)"
                    .to_string(),
            )
        } else {
            None
        };
        WakeTargetRow {
            target,
            configured,
            note,
        }
    }
}

pub fn wake_targets(ts: &[WakeTargetRow]) -> String {
    if ts.is_empty() {
        return "no wake targets set by hand (sessions that hooks recorded are listed by `another transport status`)"
            .to_string();
    }
    ts.iter()
        .map(|row| {
            let t = &row.target;
            let mut line = format!(
                "{:<24} {:<6} {}{}\n",
                t.agent,
                t.via.as_str(),
                t.target,
                if t.auto { "  (auto)" } else { "" }
            );
            if let Some(note) = &row.note {
                let _ = writeln!(line, "  not usable: {note}");
            }
            line
        })
        .collect()
}

pub fn wake_plans(ps: &[crate::wake::WakePlan]) -> String {
    ps.iter()
        .map(|p| {
            let at = format!(
                "via {} ({})",
                p.via.as_ref().map_or("-", |v| v.as_str()),
                p.target.as_deref().unwrap_or("-")
            );
            let hint = p
                .hint
                .as_deref()
                .map(|h| format!("  {h}\n"))
                .unwrap_or_default();
            let line = match (&p.skipped, p.outcome) {
                _ if p.error.is_some() => format!(
                    "failed {}: {}\n",
                    p.agent,
                    p.error.as_deref().unwrap_or_default()
                ),
                (_, Some(outcome)) => {
                    // What happened, in the words of the outcome table, then the
                    // transport's own answer.
                    let verb = match outcome {
                        another_core::WakeOutcome::Started => "woke",
                        _ if p.failure => "failed",
                        _ if p.sent => "sent a wake to",
                        _ => "did not wake",
                    };
                    format!(
                        "{verb} {} {at}: {} ({})\n",
                        p.agent,
                        outcome.meaning(),
                        p.detail.as_deref().unwrap_or_default()
                    )
                }
                (Some(why), _) if why == "dry run" => p
                    .commands
                    .iter()
                    .map(|c| format!("would run: {}\n", c.join(" ")))
                    .collect(),
                (Some(why), _) => format!("skipped {}: {why}\n", p.agent),
                (None, None) => format!("skipped {}\n", p.agent),
            };
            line + &hint
        })
        .collect()
}

pub fn prune_rows(rows: &[crate::wake::PruneRow], dry_run: bool) -> String {
    if rows.is_empty() {
        return "no wake targets or transport sessions".to_string();
    }
    let verb = if dry_run { "would drop" } else { "dropped" };
    rows.iter()
        .map(|r| {
            let what = if r.pruned { verb } else { "kept" };
            let session = match r.session_id.as_deref() {
                Some("") => "  session without an id".to_string(),
                Some(id) => format!("  session {id}"),
                None => String::new(),
            };
            format!(
                "{what:<10} {:<24} {:<6} {}{session}  ({})\n",
                r.agent, r.via, r.target, r.reason
            )
        })
        .collect()
}

pub fn daemon_report(r: &crate::daemon::DaemonReport) -> String {
    let mut out = String::new();
    let prefix = if r.dry_run { "would " } else { "" };
    let _ = writeln!(out, "{prefix}{} {}", r.change, r.file);
    for c in &r.commands {
        let _ = writeln!(out, "{prefix}run: {}", c.join(" "));
    }
    let _ = write!(
        out,
        "label {}, serving {} (loopback), log {}",
        r.label, r.bind, r.log
    );
    out
}

pub fn trial_stats(s: &another_core::TrialStats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Another stats from {} to {}", s.since, s.until);
    let _ = writeln!(out, "Collisions caught");
    let _ = writeln!(
        out,
        "  edits stopped by a hold        {:>5}  ({} distinct collision(s))",
        s.edits_blocked, s.collisions
    );
    let _ = writeln!(
        out,
        "  holds taken by the edit hook   {:>5}",
        s.holds_auto_acquired
    );
    let _ = writeln!(out, "Mail pushed into running sessions");
    let _ = writeln!(
        out,
        "  mid-task notices               {:>5}",
        s.notices_mid_task
    );
    let _ = writeln!(
        out,
        "  Herdr wakes sent               {:>5}  ({} failed)",
        s.wakes_sent, s.wakes_failed
    );
    let _ = writeln!(
        out,
        "  messages pushed                {:>5}  ({} then read, acknowledged or answered)",
        s.messages_pushed, s.pushes_acted_on
    );
    let _ = writeln!(
        out,
        "  notices at a prompt            {:>5}",
        s.notices_prompt
    );
    let _ = writeln!(out, "Requests");
    let _ = writeln!(
        out,
        "  created {}, claimed {}, completed {}",
        s.requests_created, s.requests_claimed, s.requests_completed
    );
    if !s.blocked.is_empty() {
        let _ = writeln!(out, "Recent stopped edits");
        for b in s.blocked.iter().take(10) {
            let _ = writeln!(
                out,
                "  {} {} on {}:{}, held by {} ({}:{})",
                b.at, b.agent, b.project, b.path, b.holder, b.holder_project, b.pattern
            );
        }
    }
    if !s.wakes.is_empty() {
        let _ = writeln!(out, "Recent wakes");
        for w in s.wakes.iter().take(10) {
            let _ = writeln!(
                out,
                "  {} {} {} {} ({} message(s)) {}",
                w.at, w.agent, w.via, w.target, w.messages, w.outcome
            );
        }
    }
    out
}

/// `12m ago` / `3h ago` relative to `now`; the raw text when either time does not parse.
fn ago(at: &str, now: i64) -> String {
    match another_core::parse_ts(at) {
        Some(t) => format!("{} ago", span((now - t).max(0) / 1000)),
        None => at.to_string(),
    }
}

fn span(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

pub fn board(b: &another_core::Board) -> String {
    let now = another_core::parse_ts(&b.now).unwrap_or(0);
    let mut out = String::new();
    let scope = b
        .project
        .as_deref()
        .map(|p| format!(", {p}"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "Board for {}{scope} at {} (edits since {})",
        b.node, b.now, b.since
    );
    if b.projects.is_empty() {
        let _ = writeln!(out, "\nNo live holds and no edits in this window.");
    }
    for p in &b.projects {
        let _ = writeln!(out, "\n{}  {}", p.project, p.path);
        if p.holds.is_empty() {
            let _ = writeln!(out, "  holds: none");
        } else {
            let _ = writeln!(out, "  holds");
        }
        for h in &p.holds {
            let kind = if h.exclusive { "exclusive" } else { "shared" };
            let why = if h.edit_hook {
                "edit hook".to_string()
            } else {
                h.reason.clone().unwrap_or_default()
            };
            let _ = writeln!(
                out,
                "    {}  {}  {kind}, held {}, lapses in {}{}",
                h.agent,
                h.pattern,
                ago(&h.since, now).trim_end_matches(" ago"),
                span(h.expires_in_seconds.max(0)),
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({why})")
                }
            );
        }
        if !p.edits.is_empty() {
            let _ = writeln!(out, "  recent edits (paths the edit hook claimed)");
            for e in &p.edits {
                let _ = writeln!(out, "    {}  {}  {}", ago(&e.at, now), e.agent, e.path);
            }
        }
        if !p.blocked.is_empty() {
            let _ = writeln!(out, "  stopped edits");
            for s in &p.blocked {
                let held_by = match (&s.holder, &s.holder_project, &s.pattern) {
                    (Some(h), Some(hp), Some(pat)) => format!("held by {h} ({hp}:{pat})"),
                    _ => "held in a project outside this view".to_string(),
                };
                let _ = writeln!(
                    out,
                    "    {}  {} on {}, {held_by}",
                    ago(&s.at, now),
                    s.agent,
                    s.path
                );
            }
        }
        if p.truncated {
            let _ = writeln!(out, "  (more edits than --limit shows)");
        }
    }
    if b.history_truncated {
        let _ = writeln!(
            out,
            "\n(edit history stopped after {} events; older edits in this window may be missing. Narrow --since or --project.)",
            another_core::BOARD_EVENT_SCAN_CAP
        );
    }
    let _ = writeln!(out, "\nAgents");
    if b.agents.is_empty() {
        let _ = writeln!(
            out,
            "  none with holds, unread mail, acknowledgements or requests"
        );
    }
    for a in &b.agents {
        let mut bits = Vec::new();
        if a.unread > 0 {
            bits.push(if a.urgent_unread > 0 {
                format!("{} unread ({} urgent)", a.unread, a.urgent_unread)
            } else {
                format!("{} unread", a.unread)
            });
        }
        if a.awaiting_ack > 0 {
            bits.push(format!("{} to acknowledge", a.awaiting_ack));
        }
        if a.requests > 0 {
            bits.push(format!("{} request(s)", a.requests));
        }
        if a.holds > 0 {
            bits.push(format!("{} hold(s)", a.holds));
        }
        let seen = a
            .last_seen_at
            .as_deref()
            .map(|t| format!(", seen {}", ago(t, now)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {}  {}{seen}: {}",
            a.address,
            a.presence,
            bits.join(", ")
        );
    }
    if !b.requests.is_empty() {
        let _ = writeln!(out, "\nPending requests");
    }
    for r in &b.requests {
        let status = if r.overdue {
            "overdue"
        } else {
            r.status.as_str()
        };
        let mut to = r.to.clone();
        if r.to_hidden > 0 {
            to.push(format!("{} outside this view", r.to_hidden));
        }
        let to = if to.is_empty() {
            "nobody".to_string()
        } else {
            to.join(", ")
        };
        let owner = match (&r.owner, r.owner_hidden) {
            (Some(o), _) => format!(", owner {o}"),
            (None, true) => ", owner outside this view".to_string(),
            (None, false) => String::new(),
        };
        let from = r.from.as_deref().unwrap_or("an agent outside this view");
        let _ = writeln!(
            out,
            "  {status}  {}  from {from} to {to}{owner}, opened {}  [{}]",
            r.title,
            ago(&r.created_at, now),
            r.request_id
        );
    }
    if b.requests_truncated {
        let _ = writeln!(out, "  (more requests than --limit shows)");
    }
    out
}

pub fn daemon_status(s: &crate::daemon::DaemonStatus) -> String {
    let yes = |b: bool| if b { "yes" } else { "no" };
    format!(
        "manager   {}\nlabel     {}\nfile      {} (installed: {})\nloaded    {}\npid       {}\nbind      {}\nhealthy   {}\nlog       {}",
        match s.manager {
            crate::daemon::Manager::Launchd => "launchd",
            crate::daemon::Manager::Systemd => "systemd (user)",
            crate::daemon::Manager::TaskScheduler => "Task Scheduler",
        },
        s.label,
        s.file,
        yes(s.installed),
        yes(s.loaded),
        s.pid.map_or("-".to_string(), |p| p.to_string()),
        s.bind.as_deref().unwrap_or("-"),
        yes(s.healthy),
        s.log
    )
}

/// One line per queue item: number, state, priority, kind, scope, title, then who holds it
/// or who it waits for, and its result when it is done.
pub fn queue_items(items: &[QueueItem]) -> String {
    if items.is_empty() {
        return "no items".to_string();
    }
    items.iter().map(queue_line).collect()
}

fn queue_line(i: &QueueItem) -> String {
    let mut out = format!(
        "{:<5} {:<8} {:<6} {:<9} {:<6} {}",
        i.label,
        i.state.as_str(),
        i.priority.as_str(),
        i.kind.flag(),
        i.scope.as_deref().unwrap_or("-"),
        i.title
    );
    if let Some(c) = &i.claim {
        let _ = write!(out, "  (claimed by {})", c.agent);
    }
    if let Some(a) = &i.for_agent {
        let _ = write!(out, "  (for {a})");
    }
    if i.admitted.as_ref().is_some_and(|a| a.self_admitted) {
        out.push_str("  (self-admitted)");
    }
    if let Some(r) = &i.result {
        let _ = write!(out, "  -> {}", r.outcome);
        if let Some(summary) = &r.summary {
            let _ = write!(out, ": {}", summary.replace('\n', " "));
        }
        if let (Some(kind), Some(pointer)) = (&r.pointer_kind, &r.pointer) {
            let _ = write!(out, " ({kind} {pointer})");
        }
    }
    out.push('\n');
    out
}

fn indented(out: &mut String, text: &str) {
    for line in text.lines() {
        let _ = writeln!(out, "  {line}");
    }
}

/// Every field of one item, its approval line and its history.
pub fn queue_item(i: &QueueItem) -> String {
    let mut out = format!("{}  {}\n", i.label, i.title);
    let _ = writeln!(out, "state          {}", i.state.as_str());
    let _ = writeln!(out, "kind           {}", i.kind.flag());
    let _ = writeln!(out, "priority       {}", i.priority.as_str());
    let _ = writeln!(out, "scope          {}", i.scope.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "proposed by    {} at {}", i.proposed_by, i.proposed_at);
    if let Some(a) = &i.for_agent {
        let _ = writeln!(out, "reserved for   {a}");
    }
    if let Some(a) = &i.skip_agent {
        let _ = writeln!(out, "not offered to {a}");
    }
    if let Some(c) = &i.claim {
        let bound = if c.session_bound {
            "bound to its session"
        } else {
            "not bound to a session"
        };
        let _ = writeln!(out, "claimed by     {} at {} ({bound})", c.agent, c.at);
    }
    let _ = writeln!(out, "id             {}", i.item_id);
    if let Some(line) = i.approval_line() {
        let _ = writeln!(out, "{line}");
    }
    out.push_str("done when\n");
    indented(&mut out, i.done_condition.as_deref().unwrap_or("(not set)"));
    if !i.body.trim().is_empty() {
        out.push_str("body\n");
        indented(&mut out, &i.body);
    }
    if let Some(r) = &i.result {
        let _ = writeln!(out, "result         {} by {} at {}", r.outcome, r.by, r.at);
        if let Some(summary) = &r.summary {
            indented(&mut out, summary);
        }
        if let (Some(kind), Some(pointer)) = (&r.pointer_kind, &r.pointer) {
            let _ = writeln!(out, "  {kind}: {pointer}");
        }
    }
    if let Some(history) = &i.history {
        out.push_str("history\n");
        for h in history {
            let _ = writeln!(
                out,
                "  {}  {:<16} {}",
                h.at,
                h.kind.trim_start_matches("queue."),
                h.actor
            );
            if let Some(note) = &h.note {
                for line in note.lines() {
                    let _ = writeln!(out, "      {line}");
                }
            }
        }
    }
    out
}

/// What `queue next` says: the item and how to finish or give it back, that nothing is
/// ready, or why the agent was refused.
pub fn queue_next(n: &QueueNext) -> String {
    if let Some(r) = &n.refused {
        return r.message.clone();
    }
    match &n.claimed {
        Some(i) => format!(
            "{}\nYou hold {label}. When its done condition is met:\n  another queue done {label} --summary TEXT (--pr N_OR_URL | --commit SHA | --file PATH)\nTo give it back:\n  another queue release {label} [--note TEXT] [--to AGENT]",
            queue_item(i).trim_end(),
            label = i.label
        ),
        None => "Nothing is ready. Wait; do not start work that is not in the queue.".to_string(),
    }
}

/// The owner list: what waits for the owner, section by section.
/// Why a quiet claim is listed, in the words `another needs` and
/// `another queue list --needs-owner` both print.
pub fn quiet_claim_words(q: &QuietClaim) -> String {
    let session = match &q.session_last_seen_at {
        Some(seen) if !q.unbound => format!("its session last seen {seen}"),
        _ => "not bound to a session".to_string(),
    };
    match q.reason.as_str() {
        "session_quiet" => format!(
            "name active, its session quiet for {} min, {session}, grace ends {}",
            q.session_quiet_for_seconds.unwrap_or(0) / 60,
            q.grace_ends_at
        ),
        "old_unbound" => format!(
            "name active, claimed {} ago, {session}",
            crate::transport::span(q.claimed_for_seconds.saturating_mul(1000))
        ),
        _ => format!(
            "quiet for {} min, {session}, grace ends {}",
            q.quiet_for_seconds / 60,
            q.grace_ends_at
        ),
    }
}

pub fn queue_needs_owner(n: &QueueNeedsOwner) -> String {
    let mut out = String::new();
    let mut section = |title: &str, lines: Vec<String>| {
        let _ = writeln!(out, "{title} ({})", lines.len());
        for line in lines {
            let _ = writeln!(out, "  {}", line.trim_end());
        }
    };
    section(
        "Decide now: security and data-loss proposals",
        n.decide_now.iter().map(queue_line).collect(),
    );
    section(
        "Awaiting a decision",
        n.awaiting.iter().map(queue_line).collect(),
    );
    section(
        "Quiet claims",
        n.quiet_claims
            .iter()
            .map(|q| {
                format!(
                    "{}  {}",
                    queue_line(&q.item).trim_end(),
                    quiet_claim_words(q)
                )
            })
            .collect(),
    );
    section(
        "Ready with nobody to take them",
        n.nobody_can_take.iter().map(queue_line).collect(),
    );
    section(
        "Failed notices, last 24 hours",
        n.failed_notices
            .iter()
            .map(|f| format!("{}  to {} at {}", f.item, f.to, f.at))
            .collect(),
    );
    section(
        "Self-admitted, last 7 days",
        n.self_admitted.recent.iter().map(queue_line).collect(),
    );
    let _ = writeln!(
        out,
        "{} self-admitted item(s) in all: another queue list --self-admitted",
        n.self_admitted.total
    );
    out
}

pub fn queue_audit(a: &QueueAudit) -> String {
    let releases = if a.releases.is_empty() {
        "0".to_string()
    } else {
        a.releases
            .iter()
            .map(|(reason, n)| format!("{n} {reason}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!(
        "{} item(s), {} claim(s) by {} agent(s), releases: {releases}, {} completion(s)\n",
        a.items, a.claims, a.claimers, a.completions
    );
    if a.ok {
        out.push_str("no findings\n");
    }
    for f in &a.findings {
        let _ = writeln!(out, "{:<9} {:<6} {}  {}", f.kind, f.item, f.at, f.detail);
    }
    out
}

pub fn queue_limits(l: &QueueLimits) -> String {
    let mut out = format!("default  {}\n", l.default_limit);
    for a in &l.agents {
        let _ = writeln!(out, "{:<24} {}", a.agent, a.limit);
    }
    out
}

pub fn queue_delegates(ds: &[QueueDelegate]) -> String {
    if ds.is_empty() {
        return "no delegates".to_string();
    }
    ds.iter()
        .map(|d| format!("{:<24} since {}\n", d.agent, d.added_at))
        .collect()
}

pub fn queue_grace(g: &QueueGrace) -> String {
    format!(
        "grace period: {} seconds ({} hours)",
        g.grace_seconds,
        g.grace_seconds / 3600
    )
}
