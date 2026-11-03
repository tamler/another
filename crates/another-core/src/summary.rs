//! Daily summary (docs/design/06-summary.md): one scoped digest of lanes, decisions,
//! requests, holds, spend and agents, as JSON plus short phone-readable markdown. Built only
//! from the existing scoped reads, so it never shows more than they would.

use std::collections::BTreeMap;

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::activity::{ActivityItem, ActivityKind, ActivityQuery, RequestCounts, Since};
use crate::decisions::{DecisionFilter, DecisionStatus};
use crate::error::{Error, Result};
use crate::holds::HoldFilter;
use crate::lanes::{LaneFilter, LaneStatus};
use crate::spend::{BudgetStatus, SpendQuery};
use crate::store::Store;
use crate::time::fmt_ts;
use crate::tokens::TokenScope;
use crate::webhooks::DeliveryStatus;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ClosedLane {
    pub name: String,
    pub status: LaneStatus,
    pub outcome: Option<String>,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LaneSummary {
    /// Open lanes per status.
    pub open: BTreeMap<String, u32>,
    /// Lanes that are blocked or need a decision: `name (status, lead)`.
    pub stuck: Vec<String>,
    /// Lane events in the window (status changes, reports, closes), oldest first.
    pub moved: Vec<String>,
    pub closed: Vec<ClosedLane>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct OpenDecision {
    pub decision_id: String,
    pub question: String,
    pub asker: String,
    pub human: String,
    pub lane: Option<String>,
    pub age_hours: f64,
    pub deadline_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DecisionSummary {
    /// Oldest first.
    pub open: Vec<OpenDecision>,
    pub answered: u32,
    pub expired: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RequestSummary {
    #[serde(flatten)]
    pub counts: RequestCounts,
    pub completed: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldSummary {
    pub live: u32,
    /// Refused acquisitions in the window.
    pub conflicts: u32,
    pub cross_lane_conflicts: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendLine {
    pub account: String,
    pub unit: String,
    /// Recorded in the window.
    pub spent: f64,
    /// Period used %, when the caller may see the budget.
    pub period_used_pct: Option<f64>,
    pub status: BudgetStatus,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DailySummary {
    pub since: String,
    pub until: String,
    /// Things that need a person, most important first.
    pub attention: Vec<String>,
    pub lanes: LaneSummary,
    pub decisions: DecisionSummary,
    pub requests: RequestSummary,
    pub holds: HoldSummary,
    pub spend: Vec<SpendLine>,
    pub pending_approvals: u32,
    pub agents_active: Vec<String>,
    pub webhooks_dead: u32,
    /// Events in the window per category.
    pub counts: BTreeMap<String, u32>,
    /// Short, phone-readable digest.
    pub markdown: String,
}

fn ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.timestamp_millis())
        .unwrap_or_default()
}

fn short(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or_default();
    if line.chars().count() <= n {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(n - 1).collect::<String>())
    }
}

impl Store {
    /// Every scoped feed item after `since` in `kinds` (pages through the feed).
    fn feed_since(
        &self,
        since: &Since,
        kinds: Vec<ActivityKind>,
        scope: Option<&TokenScope>,
    ) -> Result<Vec<ActivityItem>> {
        let mut out = Vec::new();
        let mut cursor = since.clone();
        loop {
            let page = self.activity_scoped(
                ActivityQuery {
                    since: Some(cursor.clone()),
                    limit: 500,
                    kinds: kinds.clone(),
                },
                scope,
            )?;
            let n = page.items.len();
            out.extend(page.items);
            match page.next_since {
                Some(next) if n == 500 => cursor = Since::After(next),
                _ => break,
            }
            if out.len() > 20_000 {
                break;
            }
        }
        Ok(out)
    }

    /// The daily summary since `since` (default: the last 24 hours), as `scope` sees it.
    pub fn daily_summary(
        &self,
        since: Option<Since>,
        scope: Option<&TokenScope>,
    ) -> Result<DailySummary> {
        let now = self.now();
        let since = since.unwrap_or(Since::Time(now - 86_400_000));
        let since_ms = match &since {
            Since::Time(t) => *t,
            Since::After(id) => self.read(|conn| {
                conn.query_row("SELECT ts FROM events WHERE event_id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("unknown event id {id:?}")))
            })?,
        };
        let base = self.activity_summary_scoped(Some(since.clone()), scope)?;

        // Lanes.
        let all_lanes = self.lanes(
            LaneFilter {
                include_closed: true,
                ..Default::default()
            },
            scope,
        )?;
        let mut open: BTreeMap<String, u32> = BTreeMap::new();
        let mut stuck = Vec::new();
        let mut closed = Vec::new();
        for l in &all_lanes {
            if l.status.is_closed() {
                if l.closed_at.as_deref().is_some_and(|c| ms(c) >= since_ms) {
                    closed.push(ClosedLane {
                        name: l.name.clone(),
                        status: l.status,
                        outcome: l.outcome.clone(),
                        pr_url: l.pr_url.clone(),
                    });
                }
                continue;
            }
            *open.entry(l.status.as_str().to_string()).or_default() += 1;
            if matches!(l.status, LaneStatus::Blocked | LaneStatus::NeedsDecision) {
                stuck.push(format!(
                    "{} ({}, lead {})",
                    l.name,
                    l.status.as_str(),
                    l.lead
                ));
            }
        }
        let lane_events = self.feed_since(&since, vec![ActivityKind::Lane], scope)?;
        let moved: Vec<String> = lane_events
            .iter()
            .filter(|i| {
                matches!(
                    i.kind.as_str(),
                    "lane.created" | "lane.status_changed" | "lane.reported" | "lane.closed"
                )
            })
            .map(|i| i.summary.clone())
            .collect();
        let cross_lane_conflicts = lane_events
            .iter()
            .filter(|i| i.kind == "lane.hold_conflict")
            .count() as u32;

        // Decisions.
        let mut open_decisions: Vec<OpenDecision> = self
            .decisions(
                DecisionFilter {
                    status: Some(DecisionStatus::Open),
                    limit: 500,
                    ..Default::default()
                },
                scope,
            )?
            .into_iter()
            .map(|d| OpenDecision {
                age_hours: ((now - ms(&d.created_at)) as f64 / 3_600_000.0 * 10.0).round() / 10.0,
                decision_id: d.decision_id,
                question: d.question,
                asker: d.asker,
                human: d.human,
                lane: d.lane,
                deadline_at: d.deadline_at,
            })
            .collect();
        open_decisions.sort_by(|a, b| b.age_hours.total_cmp(&a.age_hours));
        let req_events = self.feed_since(&since, vec![ActivityKind::Request], scope)?;
        let count = |k: &str| req_events.iter().filter(|i| i.kind == k).count() as u32;
        let decisions = DecisionSummary {
            answered: count("decision.answered"),
            expired: count("decision.expired"),
            open: open_decisions,
        };
        let requests = RequestSummary {
            completed: count("request.completed"),
            counts: base.requests.clone(),
        };

        // Holds.
        let hold_events = self.feed_since(&since, vec![ActivityKind::Hold], scope)?;
        let live: u32 = self
            .holds_by_lane(HoldFilter::default(), scope)?
            .iter()
            .map(|g| g.holds.len() as u32)
            .sum();
        let holds = HoldSummary {
            live,
            conflicts: hold_events
                .iter()
                .filter(|i| i.kind == "hold.conflict")
                .count() as u32,
            cross_lane_conflicts,
        };

        // Spend.
        let report = self.spend_report(SpendQuery::default(), scope)?;
        let window =
            self.read(|conn| crate::spend::spent_since(conn, self.node_id(), since_ms, scope))?;
        let mut spend = Vec::new();
        for a in &report.accounts {
            let spent = window
                .iter()
                .find(|(n, _, _)| *n == a.account.name)
                .map_or(0.0, |x| x.2);
            if spent == 0.0 && matches!(a.status, BudgetStatus::Ok | BudgetStatus::NoBudget) {
                continue;
            }
            spend.push(SpendLine {
                account: a.account.name.clone(),
                unit: a.account.unit.clone(),
                spent,
                period_used_pct: a.used_pct,
                status: a.status,
            });
        }
        let pending_approvals = self
            .spend_leases(false, scope)?
            .iter()
            .filter(|l| l.status == crate::spend::SpendLeaseStatus::PendingApproval)
            .count() as u32;

        // Webhooks (remote callers: their own).
        let mut webhooks_dead = 0;
        for h in self.webhooks(scope)? {
            webhooks_dead += self
                .webhook_deliveries(Some(&h.webhook_id), Some(DeliveryStatus::Dead), 500)?
                .iter()
                .filter(|d| d.created_at.as_str() >= fmt_ts(since_ms).as_str())
                .count() as u32;
        }

        // Attention.
        let mut attention = Vec::new();
        for d in &decisions.open {
            attention.push(format!(
                "Decision for {} from {} ({}h old): {} [{}]",
                d.human,
                d.asker,
                d.age_hours,
                short(&d.question, 80),
                d.decision_id
            ));
        }
        for l in &stuck {
            attention.push(format!("Lane {l}"));
        }
        for a in &report.accounts {
            match a.status {
                BudgetStatus::Over => attention.push(format!(
                    "{} is over budget ({}%)",
                    a.account.name,
                    a.used_pct.unwrap_or_default().round()
                )),
                BudgetStatus::Warning if a.projected.zip(a.budget).is_some_and(|(p, b)| p >= b) => {
                    attention.push(format!(
                        "{} is on pace to exceed its budget ({} of {} {} projected)",
                        a.account.name,
                        a.projected.unwrap_or_default(),
                        a.budget.unwrap_or_default(),
                        a.account.unit
                    ))
                }
                _ => {}
            }
        }
        if pending_approvals > 0 {
            attention.push(format!("{pending_approvals} spend approval(s) waiting"));
        }
        if requests.counts.stale > 0 {
            attention.push(format!("{} stale request(s)", requests.counts.stale));
        }
        if webhooks_dead > 0 {
            attention.push(format!(
                "{webhooks_dead} webhook delivery(ies) gave up (`another webhook dead`)"
            ));
        }

        let mut s = DailySummary {
            since: fmt_ts(since_ms),
            until: fmt_ts(now),
            attention,
            lanes: LaneSummary {
                open,
                stuck,
                moved,
                closed,
            },
            decisions,
            requests,
            holds,
            spend,
            pending_approvals,
            agents_active: base
                .agents
                .iter()
                .filter(|a| a.last_seen_at.as_deref().is_some_and(|t| ms(t) >= since_ms))
                .map(|a| a.agent.clone())
                .collect(),
            webhooks_dead,
            counts: base.counts,
            markdown: String::new(),
        };
        s.markdown = summary_markdown(&s);
        Ok(s)
    }
}

/// The phone version: heading, attention list, then one line per section.
pub fn summary_markdown(s: &DailySummary) -> String {
    let hours = ((ms(&s.until) - ms(&s.since)) as f64 / 3_600_000.0).round() as i64;
    let mut out = format!("**Another: last {hours}h**\n");
    if s.attention.is_empty() {
        out.push_str("Nothing needs you.\n");
    } else {
        out.push_str("**Needs you**\n");
        for a in s.attention.iter().take(6) {
            out.push_str(&format!("- {a}\n"));
        }
        if s.attention.len() > 6 {
            out.push_str(&format!("- …and {} more\n", s.attention.len() - 6));
        }
    }
    let open: Vec<String> = s
        .lanes
        .open
        .iter()
        .map(|(k, v)| format!("{v} {}", k.replace('_', " ")))
        .collect();
    let mut lanes = format!(
        "Lanes: {}",
        if open.is_empty() {
            "none open".to_string()
        } else {
            open.join(", ")
        }
    );
    if !s.lanes.closed.is_empty() {
        let names: Vec<String> = s
            .lanes
            .closed
            .iter()
            .map(|c| match &c.pr_url {
                Some(pr) => format!("{} ({pr})", c.name),
                None => c.name.clone(),
            })
            .collect();
        lanes.push_str(&format!("; closed: {}", names.join(", ")));
    }
    out.push_str(&lanes);
    out.push('\n');
    if let Some(last) = s
        .lanes
        .moved
        .iter()
        .rev()
        .find(|m| !m.contains("created lane"))
    {
        out.push_str(&format!("Latest: {}\n", short(last, 120)));
    }
    out.push_str(&format!(
        "Decisions: {} open, {} answered, {} timed out\n",
        s.decisions.open.len(),
        s.decisions.answered,
        s.decisions.expired
    ));
    out.push_str(&format!(
        "Requests: {} open, {} claimed, {} stale, {} done\n",
        s.requests.counts.open,
        s.requests.counts.claimed,
        s.requests.counts.stale,
        s.requests.completed
    ));
    out.push_str(&format!(
        "Holds: {} live, {} conflict(s){}\n",
        s.holds.live,
        s.holds.conflicts,
        if s.holds.cross_lane_conflicts > 0 {
            format!(" ({} between lanes)", s.holds.cross_lane_conflicts)
        } else {
            String::new()
        }
    ));
    if !s.spend.is_empty() {
        let lines: Vec<String> = s
            .spend
            .iter()
            .map(|l| {
                let pct = l
                    .period_used_pct
                    .map(|p| format!(", {}% of budget", p.round()))
                    .unwrap_or_default();
                format!("{} {} {}{pct}", l.account, l.spent, l.unit)
            })
            .collect();
        out.push_str(&format!("Spend: {}\n", lines.join("; ")));
    }
    out.push_str(&format!(
        "Agents active: {}\n",
        if s.agents_active.is_empty() {
            "none".to_string()
        } else {
            s.agents_active.join(", ")
        }
    ));
    out
}
