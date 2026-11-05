//! Daily summary (docs/design/06-summary.md): one scoped digest of lanes, decisions,
//! requests, holds, spend and agents, as JSON plus short phone-readable markdown. Built only
//! from the existing scoped reads, so it never shows more than they would.

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::activity::{ActivityItem, ActivityKind, ActivityQuery, RequestCounts, Since};
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
    /// Oldest first, at most [`OPEN_DECISIONS_CAP`] (see `open_total`).
    pub open: Vec<OpenDecision>,
    /// All open decisions the caller may see.
    pub open_total: u32,
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
    /// Agents (not people) seen in the window.
    pub agents_active: Vec<String>,
    /// People (program `human`) seen in the window, listed apart from agents.
    pub people_active: Vec<String>,
    pub webhooks_dead: u32,
    /// True when an event list hit the pager's cap, so counts may be low.
    pub truncated: bool,
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

/// The most open decisions listed (oldest first); past this the summary is `truncated`.
pub const OPEN_DECISIONS_CAP: usize = 500;

/// The most feed items read per section; past this the summary is marked `truncated`.
pub const FEED_CAP: usize = 20_000;

impl Store {
    /// Every scoped feed item after `since` in `kinds` (pages through the feed, up to
    /// [`FEED_CAP`]). Returns whether the cap was hit.
    fn feed_since(
        &self,
        since: &Since,
        kinds: Vec<ActivityKind>,
        scope: Option<&TokenScope>,
    ) -> Result<(Vec<ActivityItem>, bool)> {
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
            if out.len() >= FEED_CAP {
                out.truncate(FEED_CAP);
                return Ok((out, true));
            }
        }
        Ok((out, false))
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
            // Resolved through the shared scoped read: an event the caller can't see is
            // as unknown as one that doesn't exist.
            Since::After(id) => {
                self.read(|conn| Ok(crate::activity::resolve_cursor(conn, id, scope)?.1))?
            }
        };
        let mut truncated = false;
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
            if l.leaderless_since.is_some() {
                stuck.push(format!("{} (no lead: its lead left)", l.name));
            }
            if matches!(l.status, LaneStatus::Blocked | LaneStatus::NeedsDecision) {
                stuck.push(format!(
                    "{} ({}, lead {})",
                    l.name,
                    l.status.as_str(),
                    l.lead
                ));
            }
        }
        let (lane_events, cut) = self.feed_since(&since, vec![ActivityKind::Lane], scope)?;
        truncated |= cut;
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
        // Every visible open decision, oldest first, so the longest-waiting are never the
        // ones a cap drops; the list keeps the oldest OPEN_DECISIONS_CAP.
        let all_open = self.open_decisions_oldest_first(scope)?;
        let open_total = all_open.len() as u32;
        if all_open.len() > OPEN_DECISIONS_CAP {
            truncated = true;
        }
        let mut open_decisions: Vec<OpenDecision> = all_open
            .into_iter()
            .take(OPEN_DECISIONS_CAP)
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
        let (req_events, cut) = self.feed_since(&since, vec![ActivityKind::Request], scope)?;
        truncated |= cut;
        let count = |k: &str| req_events.iter().filter(|i| i.kind == k).count() as u32;
        let decisions = DecisionSummary {
            answered: count("decision.answered"),
            expired: count("decision.expired"),
            open: open_decisions,
            open_total,
        };
        let requests = RequestSummary {
            completed: count("request.completed"),
            counts: base.requests.clone(),
        };

        // Holds.
        let (hold_events, cut) = self.feed_since(&since, vec![ActivityKind::Hold], scope)?;
        truncated |= cut;
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
                .find(|(id, _, _)| *id == a.account.account_id)
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
        // Counted in SQL: the lease listing is capped and would drop older approvals.
        let pending_approvals =
            self.read(|conn| crate::spend::pending_approvals(conn, self.node_id(), scope))?;

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
                // Without a visible percentage (budget hidden or zero), just say so.
                BudgetStatus::Over => attention.push(match a.used_pct {
                    Some(p) => format!("{} is over budget ({}%)", a.account.name, p.round()),
                    None => format!("{} is over budget", a.account.name),
                }),
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

        let seen: Vec<_> = base
            .agents
            .iter()
            .filter(|a| a.last_seen_at.as_deref().is_some_and(|t| ms(t) >= since_ms))
            .filter(|a| a.agent != crate::identity::SYSTEM_AGENT)
            .collect();
        let (people, agents): (Vec<_>, Vec<_>) =
            seen.into_iter().partition(|a| a.program == "human");
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
            agents_active: agents.iter().map(|a| a.agent.clone()).collect(),
            people_active: people.iter().map(|a| a.agent.clone()).collect(),
            webhooks_dead,
            truncated,
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
        s.decisions.open_total, s.decisions.answered, s.decisions.expired
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
                let pct = match (l.period_used_pct, l.status) {
                    (Some(p), _) => format!(", {}% of budget", p.round()),
                    (None, BudgetStatus::Over) => ", over budget".to_string(),
                    _ => String::new(),
                };
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
    if !s.people_active.is_empty() {
        out.push_str(&format!("People active: {}\n", s.people_active.join(", ")));
    }
    if s.truncated {
        out.push_str(&format!(
            "(Truncated: over {FEED_CAP} events in a section, or over {OPEN_DECISIONS_CAP} open decisions; lists may be incomplete.)\n"
        ));
    }
    out
}

/// Parse `HH:MM` (24-hour) into minutes after midnight.
pub fn parse_hhmm(v: &str) -> Result<u32> {
    let bad = || {
        Error::Invalid(format!(
            "expected a time like 09:00 (HH:MM, 24-hour), not {v:?}"
        ))
    };
    let (h, m) = v.trim().split_once(':').ok_or_else(bad)?;
    let two_digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(h) || !two_digits(m) {
        return Err(bad());
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

// Older writes accepted five-byte numeric forms such as 001:5 and +9:00. Keep those
// stored settings readable without relaxing the format required for new writes.
fn stored_summary_time(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let bad = || {
        Error::Invalid(format!(
            "expected a time like 09:00 (HH:MM, 24-hour), not {value:?}"
        ))
    };
    let (h, m) = value.split_once(':').ok_or_else(bad)?;
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 || value.len() != 5 {
        return Err(bad());
    }
    Ok(Some(format!("{h:02}:{m:02}")))
}

/// The opt-in daily rollup (`another summary schedule`).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SummarySchedule {
    /// Local time `HH:MM`, or `None` when off.
    pub daily_at: Option<String>,
    /// `webhook` or `mail:NAME`.
    pub to: String,
    pub last_sent_at: Option<String>,
}

const LAST_SENT_MS: &str = "summary_last_sent_ms";
const LAST_SENT_DAY: &str = "summary_last_sent_day";
pub(crate) const LAST_FAILED_DAY: &str = "summary_last_failed_day";

impl Store {
    pub fn summary_schedule(&self) -> Result<SummarySchedule> {
        self.read(|conn| {
            let at = crate::settings::get(conn, "summary_daily")?;
            let last: Option<String> = conn
                .query_row(
                    "SELECT value FROM node_settings WHERE key = ?1",
                    [LAST_SENT_MS],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(SummarySchedule {
                daily_at: stored_summary_time(&at)?,
                to: crate::settings::get(conn, "summary_to")?,
                last_sent_at: last.and_then(|v| v.parse().ok()).map(fmt_ts),
            })
        })
    }

    /// Turn the rollup on (`Some("09:00")`) or off (`None`), and choose where it goes.
    pub fn set_summary_schedule(
        &self,
        daily_at: Option<&str>,
        to: Option<&str>,
    ) -> Result<SummarySchedule> {
        // Checked first: a refused time must not leave the new recipient behind.
        let daily_at = daily_at.unwrap_or("").trim();
        crate::settings::validate("summary_daily", daily_at)?;
        if let Some(to) = to {
            self.set_setting("summary_to", to)?;
        }
        self.set_setting("summary_daily", daily_at)?;
        self.summary_schedule()
    }

    /// Called by `another serve` on every sweep: once a day, after the scheduled local time,
    /// send the summary (local view) covering the time since the last one (at most 48 h;
    /// 24 h the first time). Returns where it went, if it was sent.
    pub fn send_scheduled_summary(&self) -> Result<Option<String>> {
        use chrono::TimeZone;
        let offset = chrono::Local
            .timestamp_millis_opt(self.now())
            .single()
            .map(|t| *t.offset())
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("utc"));
        self.send_scheduled_summary_at(offset)
    }

    /// [`Store::send_scheduled_summary`] with an explicit UTC offset (tests).
    pub fn send_scheduled_summary_at(&self, offset: chrono::FixedOffset) -> Result<Option<String>> {
        use chrono::{TimeZone, Timelike};
        let sched = self.summary_schedule()?;
        let Some(at) = sched.daily_at.as_deref() else {
            return Ok(None);
        };
        let due_min = parse_hhmm(at)?;
        let now = self.now();
        let local = offset
            .timestamp_millis_opt(now)
            .single()
            .ok_or_else(|| Error::Invalid("clock out of range".into()))?;
        let day = local.date_naive().to_string();
        if local.hour() * 60 + local.minute() < due_min {
            return Ok(None);
        }
        let (last_day, last_failed, last_ms): (Option<String>, Option<String>, Option<i64>) = self
            .read(|conn| {
                let get = |k: &str| -> Result<Option<String>> {
                    Ok(conn
                        .query_row("SELECT value FROM node_settings WHERE key = ?1", [k], |r| {
                            r.get(0)
                        })
                        .optional()?)
                };
                Ok((
                    get(LAST_SENT_DAY)?,
                    get(LAST_FAILED_DAY)?,
                    get(LAST_SENT_MS)?.and_then(|v| v.parse().ok()),
                ))
            })?;
        if last_day.as_deref() == Some(day.as_str()) || last_failed.as_deref() == Some(day.as_str())
        {
            return Ok(None);
        }
        let since = last_ms
            .filter(|t| now - t <= 2 * 86_400_000)
            .unwrap_or(now - 86_400_000);
        let summary = self.daily_summary(Some(Since::Time(since)), None)?;
        self.claim_scheduled_summary(sched, day, now, summary)
    }

    fn claim_scheduled_summary(
        &self,
        sched: SummarySchedule,
        day: String,
        now: i64,
        summary: DailySummary,
    ) -> Result<Option<String>> {
        let to = sched.to.clone();
        let sent = self.write(|tx| {
            // Configuration can change after the outer reads. A stale attempt must neither
            // send to its previous recipient nor mark the replacement schedule as failed.
            let at = stored_summary_time(&crate::settings::get(tx, "summary_daily")?)?;
            if at != sched.daily_at || crate::settings::get(tx, "summary_to")? != to {
                return Ok(Ok(false));
            }
            // The claim: writes are serialized (BEGIN IMMEDIATE), so of several sweepers
            // sharing this home (an HTTP server plus stdio MCP servers), only the first to
            // get here sends today's rollup; the others see the marker and stop.
            let marked: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM node_settings WHERE key IN (?1, ?2) AND value = ?3)",
                params![LAST_SENT_DAY, LAST_FAILED_DAY, day],
                |r| r.get(0),
            )?;
            if marked {
                return Ok(Ok(false));
            }
            tx.execute_batch("SAVEPOINT scheduled_summary_send")?;
            let result = (|| {
                let me = crate::identity::system_agent_tx(tx)?;
                match to.split_once(':') {
                    Some(("mail", name)) => {
                        let who = crate::identity::resolve_agent(tx, tx.node_id, name)?;
                        crate::mail::send_tx(
                            tx,
                            &crate::mail::SendMessage {
                                from: me.address.clone(),
                                to: vec![who.address],
                                subject: format!("[summary] {day}"),
                                body: summary.markdown.clone(),
                                ..Default::default()
                            },
                        )?;
                    }
                    _ => {
                        // Node-local and never visible to tokens; only local webhooks without
                        // project globs and the daily_summary filter receive it.
                        tx.emit(
                            &me.address,
                            "summary.daily",
                            serde_json::json!({
                                "since": summary.since,
                                "until": summary.until,
                                "attention_count": summary.attention.len(),
                                "summary": summary,
                            }),
                        )?;
                    }
                }
                for (k, v) in [(LAST_SENT_DAY, day.clone()), (LAST_SENT_MS, now.to_string())] {
                    tx.execute(
                        "INSERT INTO node_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                        params![k, v, tx.now],
                    )?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    tx.execute_batch("RELEASE scheduled_summary_send")?;
                    Ok(Ok(true))
                }
                // Transient storage errors roll the whole claim back so the next sweep retries.
                Err(e @ (Error::Db(_) | Error::Io(_))) => Err(e),
                Err(e) => {
                    tx.execute_batch(
                        "ROLLBACK TO scheduled_summary_send; RELEASE scheduled_summary_send",
                    )?;
                    // This marker and the refusal commit together, so another stale precheck
                    // cannot report the same deterministic failure again today.
                    tx.execute(
                        "INSERT INTO node_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                        params![LAST_FAILED_DAY, day, tx.now],
                    )?;
                    Ok(Err(e))
                }
            }
        })??;
        Ok(sent.then_some(to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, Program, RegisterAgent};

    const DAY: i64 = 1_795_219_200_000;
    const HOUR: i64 = 3_600_000;

    fn store() -> Store {
        let (clock, _) = Clock::manual(DAY + 23 * HOUR + 59 * 60_000);
        Store::open_in_memory(clock).unwrap()
    }

    fn person(s: &Store, name: &str) {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program: Program::Human,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }

    fn ambiguous_mail_schedule(s: &Store) {
        person(s, "owner");
        s.set_summary_schedule(Some("00:00"), Some("mail:owner"))
            .unwrap();
        s.ensure_project("test", "/work/shop").unwrap();
        s.register_agent_trusted(RegisterAgent {
            name: "owner".into(),
            program: Program::Claude,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    }

    fn marker(s: &Store, key: &str) -> Option<String> {
        s.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM node_settings WHERE key = ?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .unwrap()
    }

    fn raw_daily(s: &Store, value: &str) {
        // An older binary persisted these values before writes required canonical HH:MM.
        s.write(|tx| {
            tx.execute(
                "INSERT INTO node_settings (key, value, updated_at) VALUES ('summary_daily', ?1, ?2)",
                params![value, tx.now],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn stored_legacy_summary_times_still_send_once() {
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        for value in [
            "001:5", "1:005", "9:000", "023:5", "+9:00", "09:+5", "+1:+2", " 001:5 ",
        ] {
            let s = store();
            raw_daily(&s, value);
            assert_eq!(
                s.send_scheduled_summary_at(utc).unwrap().as_deref(),
                Some("webhook"),
                "legacy {value} must still deliver"
            );
            assert_eq!(s.send_scheduled_summary_at(utc).unwrap(), None, "{value}");
            assert_eq!(marker(&s, "summary_daily").as_deref(), Some(value));
        }
    }

    #[test]
    fn stored_legacy_summary_time_reads_are_canonical_without_writes() {
        for (value, canonical) in [
            ("001:5", "01:05"),
            ("1:005", "01:05"),
            ("9:000", "09:00"),
            ("023:5", "23:05"),
            ("+9:00", "09:00"),
            ("09:+5", "09:05"),
            ("+1:+2", "01:02"),
            (" 001:5 ", "01:05"),
        ] {
            let s = store();
            raw_daily(&s, value);
            let before = s.events(100).unwrap().len();
            assert_eq!(
                s.summary_schedule().unwrap().daily_at.as_deref(),
                Some(canonical)
            );
            assert_eq!(marker(&s, "summary_daily").as_deref(), Some(value));
            assert_eq!(s.events(100).unwrap().len(), before);
        }
    }

    #[test]
    fn stale_prechecks_report_a_delivery_failure_only_once() {
        let s = store();
        ambiguous_mail_schedule(&s);
        // Both sweepers captured this state before either serialized claim ran.
        let sched = s.summary_schedule().unwrap();
        let summary = s.daily_summary(None, None).unwrap();
        let day = "2026-11-21".to_string();
        let before = s.events(100).unwrap().len();
        let failed =
            s.claim_scheduled_summary(sched.clone(), day.clone(), s.now(), summary.clone());
        assert_eq!(failed.unwrap_err().code(), "ambiguous");
        assert_eq!(marker(&s, LAST_FAILED_DAY).as_deref(), Some(day.as_str()));
        assert_eq!(marker(&s, LAST_SENT_DAY), None);
        assert_eq!(
            s.events(100).unwrap().len(),
            before,
            "failed send mutations roll back"
        );
        assert_eq!(
            s.claim_scheduled_summary(sched, day, s.now(), summary)
                .unwrap(),
            None,
            "a second stale precheck must see the committed failure marker"
        );
    }

    #[test]
    fn stale_schedule_claim_cannot_send_after_settings_change() {
        for (at, to) in [
            (None, None),
            (Some("23:58"), None),
            (Some("00:00"), Some("mail:sam")),
        ] {
            let s = store();
            person(&s, "owner");
            person(&s, "sam");
            s.set_summary_schedule(Some("00:00"), Some("mail:owner"))
                .unwrap();
            let sched = s.summary_schedule().unwrap();
            let summary = s.daily_summary(None, None).unwrap();
            s.set_summary_schedule(at, to).unwrap();
            let before = s.events(100).unwrap().len();
            assert_eq!(
                s.claim_scheduled_summary(sched, "2026-11-21".into(), s.now(), summary)
                    .unwrap(),
                None
            );
            assert_eq!(s.events(100).unwrap().len(), before, "no stale send");
            assert_eq!(marker(&s, LAST_SENT_DAY), None);
            assert_eq!(marker(&s, LAST_FAILED_DAY), None);
        }
    }

    #[test]
    fn stale_schedule_failure_cannot_block_the_new_recipient() {
        let s = store();
        ambiguous_mail_schedule(&s);
        let sched = s.summary_schedule().unwrap();
        let summary = s.daily_summary(None, None).unwrap();
        person(&s, "sam");
        s.set_summary_schedule(Some("00:00"), Some("mail:sam"))
            .unwrap();
        assert_eq!(
            s.claim_scheduled_summary(sched, "2026-11-21".into(), s.now(), summary)
                .unwrap(),
            None
        );
        assert_eq!(marker(&s, LAST_FAILED_DAY), None);
        assert_eq!(
            s.send_scheduled_summary_at(chrono::FixedOffset::east_opt(0).unwrap())
                .unwrap()
                .as_deref(),
            Some("mail:sam")
        );
    }

    #[test]
    fn a_database_delivery_error_does_not_block_a_same_day_retry() {
        let s = store();
        person(&s, "owner");
        s.set_summary_schedule(Some("00:00"), Some("mail:owner"))
            .unwrap();
        s.write(|tx| {
            tx.execute_batch(
                "CREATE TEMP TRIGGER fail_summary_delivery BEFORE INSERT ON messages
                 WHEN NEW.subject LIKE '[summary]%'
                 BEGIN SELECT RAISE(ABORT, 'temporary delivery failure'); END",
            )?;
            Ok(())
        })
        .unwrap();
        let before = s.events(100).unwrap().len();
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        assert!(matches!(
            s.send_scheduled_summary_at(utc),
            Err(Error::Db(_))
        ));
        assert_eq!(marker(&s, LAST_FAILED_DAY), None);
        assert_eq!(marker(&s, LAST_SENT_DAY), None);
        assert_eq!(s.events(100).unwrap().len(), before);

        s.write(|tx| {
            tx.execute_batch("DROP TRIGGER fail_summary_delivery")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            s.send_scheduled_summary_at(utc).unwrap().as_deref(),
            Some("mail:owner")
        );
    }
}
