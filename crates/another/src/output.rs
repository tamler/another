//! Human-readable rendering for CLI output (`--json` bypasses all of this).

use std::fmt::Write;

use another_core::{
    ActivityFeed, Agent, AuditEntry, Decision, Event, HeartbeatReport, Hold, HoldConflict,
    HoldReport, Inbox, Lane, LaneDetail, LaneHolds, LaneReport, LeaseOutcome, Message, NodeInfo,
    Peer, Project, Request, Resource, SpendAccount, SpendLease, Thread, ThreadSummary, Token,
    WakeTarget, Webhook, WebhookCreated, WebhookDelivery,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Status {
    pub node: NodeInfo,
    pub db_path: String,
    pub projects: Vec<Project>,
    pub agents: Vec<Agent>,
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

pub fn holds(hs: &[Hold]) -> String {
    hs.iter()
        .map(|h| {
            format!(
                "{:<9} {:<16} {}:{}  until {}{}{}  [{}]\n",
                if h.exclusive { "exclusive" } else { "shared" },
                h.agent,
                h.project,
                h.pattern,
                h.expires_at,
                h.lane
                    .as_deref()
                    .map(|l| format!("  lane {l}"))
                    .unwrap_or_default(),
                h.reason
                    .as_deref()
                    .map(|r| format!("  ({r})"))
                    .unwrap_or_default(),
                h.hold_id
            )
        })
        .collect()
}

pub fn conflicts(cs: &[HoldConflict]) -> String {
    cs.iter()
        .map(|c| {
            format!(
                "conflict: {} is held by {}{} ({}, {} until {})\n",
                c.path,
                c.held.agent,
                c.held
                    .lane
                    .as_deref()
                    .map(|l| format!(" in lane {l}"))
                    .unwrap_or_default(),
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
    out.push_str(&hold_warnings(&r.warnings));
    out
}

pub fn hold_warnings(warnings: &[HoldConflict]) -> String {
    let mut out = String::new();
    for w in warnings {
        let where_held = if w.same_lane {
            "in the same lane"
        } else {
            "on another node"
        };
        let advice = if w.same_lane {
            "allowed; coordinate"
        } else {
            "holds are node-local; coordinate"
        };
        let _ = writeln!(
            out,
            "warning: {} overlaps {} held {where_held} by {} ({advice})",
            w.path, w.held.pattern, w.held.agent
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
}

pub fn hook_status(rows: &[HookStatus]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "{:<7} {:<13} {}\n",
                r.client,
                if r.installed {
                    "installed"
                } else {
                    "not installed"
                },
                r.config
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
                    .unwrap_or_default()
                    + &r.spend_account
                        .as_deref()
                        .map(|a| format!("  spend:{a}"))
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
    let decision = o
        .decision_id
        .as_ref()
        .map(|d| format!(" (waiting for decision {d})"))
        .unwrap_or_default();
    format!(
        "{} {} {}{lease}{decision}\n{}",
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
        let _ = writeln!(
            out,
            "eligible; nothing was done. Re-run with --confirm to send SIGTERM."
        );
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
                e.detail.as_deref().unwrap_or(""),
                e.remote
                    .as_deref()
                    .map(|r| format!("  [{r}]"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

pub fn peers(ps: &[Peer]) -> String {
    if ps.is_empty() {
        return "not paired with any node".to_string();
    }
    ps.iter()
        .map(|p| {
            let perms: Vec<&str> = p.perms.iter().map(|x| x.as_str()).collect();
            format!(
                "{:<6} {:<16} {}  key {}  perms [{}]  last sync {}{}\n",
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
    out
}

pub fn wake_targets(ts: &[WakeTarget]) -> String {
    if ts.is_empty() {
        return "no wake targets (hooks record tmux/cmux panes automatically)".to_string();
    }
    ts.iter()
        .map(|t| {
            format!(
                "{:<24} {:<6} {}{}\n",
                t.agent,
                t.via.as_str(),
                t.target,
                if t.auto { "  (auto)" } else { "" }
            )
        })
        .collect()
}

pub fn wake_plans(ps: &[crate::wake::WakePlan]) -> String {
    ps.iter()
        .map(|p| match (&p.skipped, p.sent) {
            (_, true) => format!(
                "woke {} via {} ({})\n",
                p.agent,
                p.via.map_or("-", |v| v.as_str()),
                p.target.as_deref().unwrap_or("-")
            ),
            (Some(why), _) if why == "dry run" => p
                .commands
                .iter()
                .map(|c| format!("would run: {}\n", c.join(" ")))
                .collect(),
            (Some(why), _) => format!("skipped {}: {why}\n", p.agent),
            (None, false) => format!("skipped {}\n", p.agent),
        })
        .collect()
}

pub fn lanes(ls: &[Lane]) -> String {
    if ls.is_empty() {
        return "no lanes".to_string();
    }
    ls.iter()
        .map(|l| {
            format!(
                "{:<15} {:<24} {:<10} lead {:<18} {} report(s), updated {}\n",
                l.status.as_str(),
                l.name,
                l.project,
                l.lead,
                l.reports,
                l.updated_at
            )
        })
        .collect()
}

pub fn lane_detail(d: &LaneDetail) -> String {
    let l = &d.lane;
    let mut out = format!(
        "lane {} [{}] in {} (lead {}, created by {})\n",
        l.name,
        l.status.as_str(),
        l.project,
        l.lead,
        l.created_by
    );
    if !l.charter.trim().is_empty() {
        let _ = writeln!(
            out,
            "charter: {}",
            l.charter.lines().next().unwrap_or_default()
        );
    }
    if let Some(pr) = &l.pr_url {
        let _ = writeln!(out, "pr: {pr}");
    }
    for r in d.reports.iter().take(5) {
        let _ = writeln!(
            out,
            "  {} {:<6} {}{}: {}",
            r.created_at,
            r.kind.as_str(),
            r.agent,
            r.progress.map(|p| format!(" {p}%")).unwrap_or_default(),
            r.summary.lines().next().unwrap_or_default()
        );
    }
    for k in &d.links {
        let _ = writeln!(out, "  linked {} {}", k.kind.as_str(), k.ref_id);
    }
    for h in &d.holds {
        let _ = writeln!(
            out,
            "  hold {} by {} ({}, until {})",
            h.pattern,
            h.agent,
            if h.exclusive { "exclusive" } else { "shared" },
            h.expires_at
        );
    }
    out
}

pub fn lane_report(r: &LaneReport) -> String {
    format!(
        "reported on lane ({}){}: {}",
        r.kind.as_str(),
        r.status
            .map(|s| format!(" -> {}", s.as_str()))
            .unwrap_or_default(),
        r.summary.lines().next().unwrap_or_default()
    )
}

pub fn webhook_created(c: &WebhookCreated) -> String {
    let mut out = webhooks(std::slice::from_ref(&c.webhook));
    let _ = writeln!(out, "signing secret (shown once): {}", c.secret);
    out.push_str(
        "verify: X-Another-Signature = sha256=hex(HMAC-SHA256(secret, X-Another-Timestamp + \".\" + body))\n",
    );
    out
}

pub fn webhooks(hs: &[Webhook]) -> String {
    if hs.is_empty() {
        return "no webhooks".to_string();
    }
    let mut out = String::new();
    for h in hs {
        let filters: Vec<&str> = h.filters.iter().map(|f| f.as_str()).collect();
        let projects = h
            .project_globs
            .as_ref()
            .map_or_else(|| "all projects".to_string(), |g| g.join(", "));
        let _ = writeln!(
            out,
            "{} {}\n  events: {}; projects: {}; {} pending, {} delivered, {} dead{}{}",
            h.webhook_id,
            h.url,
            filters.join(","),
            projects,
            h.pending,
            h.delivered,
            h.dead,
            h.disabled_reason
                .as_ref()
                .map(|r| format!("; disabled: {r}"))
                .unwrap_or_default(),
            h.description
                .as_ref()
                .map(|d| format!("; {d}"))
                .unwrap_or_default()
        );
    }
    out
}

pub fn deliveries(ds: &[WebhookDelivery]) -> String {
    if ds.is_empty() {
        return "no deliveries".to_string();
    }
    let mut out = String::new();
    for d in ds {
        let result = match (&d.last_status, &d.last_error) {
            (_, Some(e)) => e.clone(),
            (Some(s), None) => format!("HTTP {s}"),
            _ => "not sent yet".to_string(),
        };
        let _ = writeln!(
            out,
            "{} {:<9} {} via {} ({} attempt(s)): {}{}",
            d.delivery_id,
            d.status.as_str(),
            d.kind,
            d.filter,
            d.attempts,
            result,
            d.next_attempt_at
                .as_ref()
                .map(|t| format!("; next {t}"))
                .unwrap_or_default()
        );
    }
    out
}

pub fn decisions(ds: &[Decision]) -> String {
    if ds.is_empty() {
        return "no decisions".to_string();
    }
    let mut out = String::new();
    for d in ds {
        let values: Vec<&str> = d.options.iter().map(|o| o.value.as_str()).collect();
        let answer = d
            .answer
            .as_ref()
            .map(|a| {
                format!(
                    " -> {} by {}",
                    a.value.as_deref().unwrap_or("text"),
                    a.answered_by
                )
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{} {:<9} for {} from {} [{}]{}\n  {}  ({}){}",
            d.decision_id,
            d.status.as_str(),
            d.human,
            d.asker,
            d.project,
            d.lane
                .as_ref()
                .map(|l| format!(" lane {l}"))
                .unwrap_or_default(),
            d.question.lines().next().unwrap_or_default(),
            values.join(" | "),
            answer
        );
    }
    out
}

pub fn lane_holds(gs: &[LaneHolds]) -> String {
    if gs.is_empty() {
        return "no live holds".to_string();
    }
    let mut out = String::new();
    for g in gs {
        match &g.lane {
            Some(l) => {
                let _ = writeln!(
                    out,
                    "lane {l} [{}] in {} (lead {}): {} hold(s)",
                    g.status.map(|s| s.as_str()).unwrap_or("?"),
                    g.project,
                    g.lead.as_deref().unwrap_or("?"),
                    g.holds.len()
                );
            }
            None => {
                let _ = writeln!(out, "no lane, in {}: {} hold(s)", g.project, g.holds.len());
            }
        }
        for line in holds(&g.holds).lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    out
}

pub fn spend_accounts(accts: &[SpendAccount]) -> String {
    if accts.is_empty() {
        return "no spend accounts".to_string();
    }
    let mut out = String::new();
    for a in accts {
        let budget = a
            .budget
            .map(|b| {
                format!(
                    "{b} {}/{}",
                    a.unit,
                    if a.period.as_str() == "monthly" {
                        format!("month from day {}", a.reset_day)
                    } else {
                        "lifetime".into()
                    }
                )
            })
            .unwrap_or_else(|| "no budget".into());
        let _ = writeln!(
            out,
            "{:<20} {:<12} {}{}{}{}",
            a.name,
            a.kind.as_str(),
            budget,
            a.cost_per_hour
                .map(|c| format!(", {c} {}/h", a.unit))
                .unwrap_or_default(),
            if a.requires_approval {
                format!(
                    ", approval by {}",
                    a.approver.as_deref().unwrap_or("the decider")
                )
            } else {
                String::new()
            },
            a.project
                .as_ref()
                .map(|p| format!(" [{p}]"))
                .unwrap_or_else(|| " [shared]".into())
        );
    }
    out
}

pub fn spend_leases(ls: &[SpendLease]) -> String {
    if ls.is_empty() {
        return "no spend leases".to_string();
    }
    let mut out = String::new();
    for l in ls {
        let _ = writeln!(
            out,
            "{} {:<16} {} by {}{} at {} {}/h: {} {}{}",
            l.lease_id,
            l.status.as_str(),
            l.account,
            l.owner,
            l.lane
                .as_ref()
                .map(|x| format!(" (lane {x})"))
                .unwrap_or_default(),
            l.cost_per_hour,
            l.unit,
            l.cost,
            l.unit,
            l.expires_at
                .as_ref()
                .filter(|_| l.status.as_str() == "active")
                .map(|e| format!(", until {e}"))
                .unwrap_or_default()
        );
    }
    out
}
