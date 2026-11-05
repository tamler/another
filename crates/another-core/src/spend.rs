//! Spend ledger (docs/design/05-spend.md): accounts with budgets and monthly periods, spend
//! entries, hourly leases with an approval gate (a human decision), and burn-down reports.
//! Nothing here talks to a provider; amounts are recorded by agents or people.

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::decisions::{
    ask_tx, decider_tx, validate, DecisionOption, DecisionTimeout, NewDecision,
};
use crate::error::{Error, Result};
use crate::identity::{find_project, resolve_agent, AgentRef};
use crate::lanes::resolve_lane_in;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::{scope_allows_project, TokenScope};

const HOUR_MS: f64 = 3_600_000.0;
const DAY_MS: f64 = 86_400_000.0;
pub const DEFAULT_SPEND_LEASE_TTL_SECONDS: i64 = 3600;
const MIN_LEASE_TTL: i64 = 60;
const MAX_LEASE_TTL: i64 = 86_400;
/// Budget fractions that emit `spend.budget_threshold` when crossed.
const THRESHOLDS: [f64; 2] = [0.8, 1.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpendKind {
    GpuPod,
    ApiCredits,
    PlanReset,
    Server,
    Domain,
}

impl SpendKind {
    pub const ALL: [SpendKind; 5] = [
        SpendKind::GpuPod,
        SpendKind::ApiCredits,
        SpendKind::PlanReset,
        SpendKind::Server,
        SpendKind::Domain,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            SpendKind::GpuPod => "gpu_pod",
            SpendKind::ApiCredits => "api_credits",
            SpendKind::PlanReset => "plan_reset",
            SpendKind::Server => "server",
            SpendKind::Domain => "domain",
        }
    }
}

impl std::str::FromStr for SpendKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        SpendKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s.trim())
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown spend kind {s:?}; expected gpu_pod, api_credits, plan_reset, server or domain"
                ))
            })
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SpendPeriod {
    /// From `reset_day` of one month to the same day of the next (UTC).
    #[default]
    Monthly,
    /// No reset: the budget is for the account's lifetime.
    None,
}

impl SpendPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            SpendPeriod::Monthly => "monthly",
            SpendPeriod::None => "none",
        }
    }
}

impl std::str::FromStr for SpendPeriod {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "monthly" => Ok(SpendPeriod::Monthly),
            "none" | "lifetime" => Ok(SpendPeriod::None),
            other => Err(Error::Invalid(format!(
                "unknown period {other:?}; expected monthly or none"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendAccount {
    pub account_id: String,
    pub name: String,
    pub kind: SpendKind,
    pub unit: String,
    /// Per period, in `unit`.
    pub budget: Option<f64>,
    pub period: SpendPeriod,
    pub reset_day: u32,
    /// Default rate for leases, in `unit` per hour.
    pub cost_per_hour: Option<f64>,
    pub requires_approval: bool,
    /// Who approves borrowing (default: the node's decider).
    pub approver: Option<String>,
    /// Owning project (slug); `None` = shared.
    pub project: Option<String>,
    pub description: Option<String>,
    pub created_at: String,
}

/// Input for [`Store::create_spend_account`].
#[derive(Debug, Clone)]
pub struct NewSpendAccount {
    pub name: String,
    pub kind: SpendKind,
    pub unit: String,
    pub budget: Option<f64>,
    pub period: SpendPeriod,
    pub reset_day: u32,
    pub cost_per_hour: Option<f64>,
    pub requires_approval: bool,
    pub approver: Option<String>,
    /// Project path or slug.
    pub project: Option<String>,
    pub description: Option<String>,
}

/// Changes for [`Store::update_spend_account`]; `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct SpendAccountUpdate {
    pub budget: Option<f64>,
    pub clear_budget: bool,
    pub reset_day: Option<u32>,
    pub cost_per_hour: Option<f64>,
    pub requires_approval: Option<bool>,
    pub approver: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendEntry {
    pub entry_id: String,
    pub account: String,
    pub amount: f64,
    pub unit: String,
    pub at: String,
    pub agent: Option<String>,
    pub lane: Option<String>,
    pub project: Option<String>,
    pub note: Option<String>,
    /// `manual` or `lease`.
    pub source: String,
    pub lease_id: Option<String>,
}

/// Input for [`Store::record_spend`].
#[derive(Debug, Clone, Default)]
pub struct RecordSpend {
    pub agent: Option<String>,
    pub account: String,
    pub amount: f64,
    pub lane: Option<String>,
    pub project: Option<String>,
    pub note: Option<String>,
    /// When it was spent (default now).
    pub at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpendLeaseStatus {
    PendingApproval,
    Active,
    Ended,
    Denied,
}

impl SpendLeaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SpendLeaseStatus::PendingApproval => "pending_approval",
            SpendLeaseStatus::Active => "active",
            SpendLeaseStatus::Ended => "ended",
            SpendLeaseStatus::Denied => "denied",
        }
    }
    fn parse(s: &str) -> SpendLeaseStatus {
        match s {
            "pending_approval" => SpendLeaseStatus::PendingApproval,
            "active" => SpendLeaseStatus::Active,
            "denied" => SpendLeaseStatus::Denied,
            _ => SpendLeaseStatus::Ended,
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendLease {
    pub lease_id: String,
    pub account: String,
    pub unit: String,
    pub owner: String,
    pub lane: Option<String>,
    pub project: Option<String>,
    pub cost_per_hour: f64,
    pub ttl_seconds: i64,
    pub status: SpendLeaseStatus,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub expires_at: Option<String>,
    pub ended_at: Option<String>,
    /// Cost so far (active) or final cost (ended).
    pub cost: f64,
    pub decision_id: Option<String>,
    pub note: Option<String>,
    /// The registered resource this lease pays for (borrowing a linked resource).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}

/// Input for [`Store::borrow_spend`].
#[derive(Debug, Clone, Default)]
pub struct BorrowSpend {
    pub agent: String,
    pub account: String,
    pub lane: Option<String>,
    /// Rate (default: the account's `cost_per_hour`).
    pub cost_per_hour: Option<f64>,
    pub ttl_seconds: Option<i64>,
    /// Expected duration, shown in the approval question.
    pub hours: Option<f64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendLeaseOutcome {
    pub lease: SpendLease,
    /// Set when the account requires approval: the lease starts once this is approved.
    pub decision_id: Option<String>,
}

/// Which period a report covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PeriodSel {
    #[default]
    Current,
    Previous,
    /// The period starting on the account's reset day in this month.
    Month(i32, u32),
}

impl std::str::FromStr for PeriodSel {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        match s {
            "" | "current" => return Ok(PeriodSel::Current),
            "previous" | "last" => return Ok(PeriodSel::Previous),
            _ => {}
        }
        let bad = || {
            Error::Invalid(format!(
                "invalid period {s:?}; use current, previous or YYYY-MM"
            ))
        };
        let (y, m) = s.split_once('-').ok_or_else(bad)?;
        let (y, m): (i32, u32) = (y.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
        if !(1..=12).contains(&m) || !(1970..=9999).contains(&y) {
            return Err(bad());
        }
        Ok(PeriodSel::Month(y, m))
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpendQuery {
    pub period: PeriodSel,
    pub account: Option<String>,
    pub lane: Option<String>,
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    Ok,
    Warning,
    Over,
    /// No budget set.
    NoBudget,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AccountSpend {
    pub account: SpendAccount,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    /// Recorded entries in the period.
    pub spent: f64,
    /// Open leases' cost so far.
    pub accrued: f64,
    pub budget: Option<f64>,
    pub remaining: Option<f64>,
    pub used_pct: Option<f64>,
    pub days_elapsed: Option<f64>,
    pub days_total: Option<f64>,
    /// Straight-line projection of spent + accrued to the period end.
    pub projected: Option<f64>,
    /// Remaining budget per remaining day.
    pub daily_left: Option<f64>,
    pub status: BudgetStatus,
    pub active_leases: u32,
    /// Shared account seen by a scoped token: `spent`, `accrued` and the groupings count only
    /// the caller's projects, while `budget`, `remaining`, `used_pct`, `projected` and
    /// `status` are for the whole account (read-only).
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendGroup {
    /// Lane name or agent address (`(none)` when unattributed).
    pub key: String,
    pub account: String,
    pub unit: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpendReport {
    pub generated_at: String,
    pub accounts: Vec<AccountSpend>,
    pub by_lane: Vec<SpendGroup>,
    pub by_agent: Vec<SpendGroup>,
    /// Short burn-down for a phone.
    pub markdown: String,
}

fn round(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn check_name(name: &str) -> Result<String> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty()
        || n.len() > 64
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Invalid(format!(
            "account name {name:?}: 1-64 letters, digits, '-', '_' or '.'"
        )));
    }
    Ok(n)
}

fn check_amount(what: &str, x: f64) -> Result<()> {
    if !x.is_finite() || x < 0.0 {
        return Err(Error::Invalid(format!(
            "{what} must be a non-negative number"
        )));
    }
    Ok(())
}

fn check_reset_day(d: u32) -> Result<()> {
    if !(1..=28).contains(&d) {
        return Err(Error::Invalid("reset_day must be 1..28".into()));
    }
    Ok(())
}

fn local_only(scope: Option<&TokenScope>) -> Result<()> {
    if scope.is_some() {
        return Err(Error::Forbidden(
            "spend accounts are configured locally (CLI or local MCP), not by remote peers".into(),
        ));
    }
    Ok(())
}

fn ms_of(d: NaiveDate) -> i64 {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
        .timestamp_millis()
}

fn add_month(y: i32, m: u32, by: i32) -> (i32, u32) {
    let idx = y * 12 + (m as i32 - 1) + by;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

/// `[start, end)` of the selected monthly period for an account resetting on `reset_day`.
pub fn period_bounds(sel: PeriodSel, now_ms: i64, reset_day: u32) -> (i64, i64) {
    let today = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap_or_default()
        .date_naive();
    let (y, m) = match sel {
        PeriodSel::Month(y, m) => (y, m),
        _ => {
            let (y, m) = (today.year(), today.month());
            let anchor = if today.day() >= reset_day {
                (y, m)
            } else {
                add_month(y, m, -1)
            };
            if sel == PeriodSel::Previous {
                add_month(anchor.0, anchor.1, -1)
            } else {
                anchor
            }
        }
    };
    let start = NaiveDate::from_ymd_opt(y, m, reset_day).expect("reset_day <= 28");
    let (ny, nm) = add_month(y, m, 1);
    let end = NaiveDate::from_ymd_opt(ny, nm, reset_day).expect("reset_day <= 28");
    (ms_of(start), ms_of(end))
}

const ACCOUNT_SELECT: &str = "SELECT a.account_id, a.name, a.kind, a.unit, a.budget, a.period,
       a.reset_day, a.cost_per_hour, a.requires_approval, a.approver, p.slug, a.description,
       a.created_at, a.project_id
     FROM spend_accounts a LEFT JOIN projects p ON p.project_id = a.project_id";

fn account_from_row(r: &Row<'_>) -> rusqlite::Result<(SpendAccount, Option<String>)> {
    Ok((
        SpendAccount {
            account_id: r.get(0)?,
            name: r.get(1)?,
            kind: r
                .get::<_, String>(2)?
                .parse()
                .unwrap_or(SpendKind::ApiCredits),
            unit: r.get(3)?,
            budget: r.get(4)?,
            period: r.get::<_, String>(5)?.parse().unwrap_or_default(),
            reset_day: r.get(6)?,
            cost_per_hour: r.get(7)?,
            requires_approval: r.get(8)?,
            approver: r.get(9)?,
            project: r.get(10)?,
            description: r.get(11)?,
            created_at: fmt_ts(r.get(12)?),
        },
        r.get(13)?,
    ))
}

/// An account by id or name (live only), with its project id.
fn account(conn: &Connection, node_id: &str, key: &str) -> Result<(SpendAccount, Option<String>)> {
    conn.query_row(
        &format!(
            "{ACCOUNT_SELECT} WHERE a.node_id = ?1 AND a.archived_at IS NULL
               AND (a.account_id = ?2 OR a.name = ?3)"
        ),
        params![node_id, key.trim(), key.trim().to_ascii_lowercase()],
        account_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("no spend account {key:?}")))
}

/// [`account`] as `scope` sees it: another project's account is the same `not_found` (and
/// message) as one that doesn't exist.
fn account_in(
    conn: &Connection,
    node_id: &str,
    key: &str,
    scope: Option<&TokenScope>,
) -> Result<(SpendAccount, Option<String>)> {
    let found = account(conn, node_id, key)?;
    if !account_visible(conn, scope, found.1.as_deref())? {
        return Err(Error::NotFound(format!("no spend account {key:?}")));
    }
    Ok(found)
}

/// Accounts a scope may see: its projects' accounts, plus shared ones.
fn account_visible(
    conn: &Connection,
    scope: Option<&TokenScope>,
    project_id: Option<&str>,
) -> Result<bool> {
    match (scope, project_id) {
        (None, _) | (Some(_), None) => Ok(true),
        (Some(s), Some(p)) => scope_allows_project(conn, s, p),
    }
}

/// Whether a scope may see an entry or lease attributed to `project_id` by `agent_id`.
fn attributed_visible(
    conn: &Connection,
    scope: Option<&TokenScope>,
    project_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<bool> {
    match (scope, project_id) {
        (None, _) => Ok(true),
        (Some(s), Some(p)) => scope_allows_project(conn, s, p),
        (Some(s), None) => Ok(agent_id == Some(s.agent_id.as_str())),
    }
}

struct Attribution {
    agent: Option<AgentRef>,
    lane_id: Option<String>,
    project_id: Option<String>,
}

/// Who and what a spend is for: lane's project, else the account's, else the agent's.
fn attribute(
    tx: &Connection,
    node_id: &str,
    account_project: Option<&str>,
    agent: Option<&str>,
    lane: Option<&str>,
    project: Option<&str>,
    scope: Option<&TokenScope>,
) -> Result<Attribution> {
    // Everything named is looked up as the caller sees it: a hidden project or agent is
    // the same not_found as a missing one.
    let agent = match agent {
        Some(a) => Some(crate::identity::resolve_agent_in(tx, node_id, a, scope)?),
        None => None,
    };
    let lane = match lane {
        Some(l) => Some(resolve_lane_in(tx, node_id, l, scope)?),
        None => None,
    };
    let explicit = match project {
        Some(p) => Some(crate::identity::find_project_in(tx, node_id, p, scope)?.project_id),
        None => None,
    };
    let agent_project: Option<String> = match &agent {
        Some(a) => tx
            .query_row(
                "SELECT project_id FROM agents WHERE agent_id = ?1",
                [&a.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten(),
        None => None,
    };
    let project_id = lane
        .as_ref()
        .map(|(_, p)| p.clone())
        .or(explicit.clone())
        .or(account_project.map(str::to_string))
        .or(agent_project);
    for (what, p) in [
        ("lane", lane.as_ref().map(|(_, p)| p.as_str())),
        ("project", explicit.as_deref()),
    ] {
        if let (Some(p), Some(ap)) = (p, account_project) {
            if p != ap {
                return Err(Error::Invalid(format!(
                    "the {what} belongs to a different project than the account"
                )));
            }
        }
    }
    if let (Some((_, lp)), Some(e)) = (&lane, &explicit) {
        if lp != e {
            return Err(Error::Invalid(
                "the lane belongs to a different project".into(),
            ));
        }
    }
    Ok(Attribution {
        agent,
        lane_id: lane.map(|(l, _)| l.lane_id),
        project_id,
    })
}

/// Entries' total for an account in `[from, to)`.
fn spent_between(conn: &Connection, account_id: &str, from: i64, to: i64) -> Result<f64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(amount), 0) FROM spend_entries WHERE account_id = ?1 AND at >= ?2 AND at < ?3",
        params![account_id, from, to],
        |r| r.get(0),
    )?)
}

fn current_window(a: &SpendAccount, now: i64) -> (i64, i64) {
    match a.period {
        SpendPeriod::Monthly => period_bounds(PeriodSel::Current, now, a.reset_day),
        SpendPeriod::None => (0, i64::MAX),
    }
}

/// Insert an entry and emit `spend.recorded` (+ thresholds crossed). Returns the entry id.
#[allow(clippy::too_many_arguments)]
fn insert_entry(
    tx: &Tx<'_>,
    a: &SpendAccount,
    account_project: Option<&str>,
    amount: f64,
    at: i64,
    who: &Attribution,
    note: Option<&str>,
    source: &str,
    lease_id: Option<&str>,
    actor: &str,
) -> Result<String> {
    let (from, to) = current_window(a, tx.now);
    let before = spent_between(tx, &a.account_id, from, to)?;
    let entry_id = new_id();
    let event_id = tx.emit(
        actor,
        "spend.recorded",
        json!({
            "entry_id": entry_id,
            "account_id": a.account_id,
            "account": a.name,
            "amount": amount,
            "unit": a.unit,
            "project_id": who.project_id,
            "lane_id": who.lane_id,
            "agent_id": who.agent.as_ref().map(|x| x.id.clone()),
            "source": source,
            "lease_id": lease_id,
        }),
    )?;
    tx.execute(
        "INSERT INTO spend_entries (entry_id, node_id, account_id, amount, at, agent_id, lane_id,
           project_id, note, source, lease_id, created_at, event_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            entry_id,
            tx.node_id,
            a.account_id,
            amount,
            at,
            who.agent.as_ref().map(|x| x.id.clone()),
            who.lane_id,
            who.project_id,
            note,
            source,
            lease_id,
            tx.now,
            event_id
        ],
    )?;
    if let Some(budget) = a.budget.filter(|b| *b > 0.0) {
        let after = spent_between(tx, &a.account_id, from, to)?;
        for t in THRESHOLDS {
            if before < t * budget && after >= t * budget {
                tx.emit(
                    "system",
                    "spend.budget_threshold",
                    json!({
                        "account_id": a.account_id,
                        "account": a.name,
                        "project_id": account_project,
                        "pct": (t * 100.0).round(),
                        "spent": round(after),
                        "budget": budget,
                        "unit": a.unit,
                    }),
                )?;
            }
        }
    }
    Ok(entry_id)
}

const LEASE_SELECT: &str = "SELECT l.lease_id, a.name, a.unit, ag.address, ln.name, p.slug,
       l.cost_per_hour, l.ttl_seconds, l.status, l.requested_at, l.started_at, l.expires_at,
       l.ended_at, l.decision_id, l.note, l.account_id, l.project_id, l.agent_id, l.lane_id,
       l.resource_id
     FROM spend_leases l
     JOIN spend_accounts a ON a.account_id = l.account_id
     JOIN agents ag ON ag.agent_id = l.agent_id
     LEFT JOIN lanes ln ON ln.lane_id = l.lane_id
     LEFT JOIN projects p ON p.project_id = l.project_id";

struct LeaseRow {
    lease: SpendLease,
    account_id: String,
    project_id: Option<String>,
    agent_id: String,
    lane_id: Option<String>,
    resource_id: Option<String>,
    started: Option<i64>,
    expires: Option<i64>,
}

fn lease_cost(cph: f64, started: Option<i64>, until: i64) -> f64 {
    match started {
        Some(s) if until > s => round((until - s) as f64 / HOUR_MS * cph),
        _ => 0.0,
    }
}

fn lease_from_row(now: i64) -> impl Fn(&Row<'_>) -> rusqlite::Result<LeaseRow> {
    move |r| {
        let status = SpendLeaseStatus::parse(&r.get::<_, String>(8)?);
        let started: Option<i64> = r.get(10)?;
        let expires: Option<i64> = r.get(11)?;
        let ended: Option<i64> = r.get(12)?;
        let cph: f64 = r.get(6)?;
        let cost = match status {
            SpendLeaseStatus::Active => lease_cost(cph, started, now),
            SpendLeaseStatus::Ended => lease_cost(cph, started, ended.unwrap_or(now)),
            _ => 0.0,
        };
        Ok(LeaseRow {
            lease: SpendLease {
                lease_id: r.get(0)?,
                account: r.get(1)?,
                unit: r.get(2)?,
                owner: r.get(3)?,
                lane: r.get(4)?,
                project: r.get(5)?,
                cost_per_hour: cph,
                ttl_seconds: r.get(7)?,
                status,
                requested_at: fmt_ts(r.get(9)?),
                started_at: started.map(fmt_ts),
                expires_at: expires.map(fmt_ts),
                ended_at: ended.map(fmt_ts),
                cost,
                decision_id: r.get(13)?,
                note: r.get(14)?,
                resource_id: r.get(19)?,
            },
            account_id: r.get(15)?,
            project_id: r.get(16)?,
            agent_id: r.get(17)?,
            lane_id: r.get(18)?,
            resource_id: r.get(19)?,
            started,
            expires,
        })
    }
}

fn load_lease(conn: &Connection, now: i64, id: &str) -> Result<LeaseRow> {
    conn.query_row(
        &format!("{LEASE_SELECT} WHERE l.lease_id = ?1"),
        [id.trim().to_ascii_uppercase()],
        lease_from_row(now),
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("no spend lease {id:?}")))
}

fn account_by_id(conn: &Connection, id: &str) -> Result<(SpendAccount, Option<String>)> {
    Ok(conn.query_row(
        &format!("{ACCOUNT_SELECT} WHERE a.account_id = ?1"),
        [id],
        account_from_row,
    )?)
}

fn agent_by_id(conn: &Connection, id: &str) -> Result<AgentRef> {
    Ok(conn.query_row(
        "SELECT agent_id, address, node_id FROM agents WHERE agent_id = ?1",
        [id],
        |r| {
            Ok(AgentRef {
                id: r.get(0)?,
                address: r.get(1)?,
                node_id: r.get(2)?,
            })
        },
    )?)
}

/// End an active lease at `at`: write its cost as a `lease` entry.
fn end_lease_tx(tx: &Tx<'_>, row: &LeaseRow, at: i64, actor: &str, reason: &str) -> Result<()> {
    let (a, account_project) = account_by_id(tx, &row.account_id)?;
    let cost = lease_cost(row.lease.cost_per_hour, row.started, at);
    let who = Attribution {
        agent: Some(agent_by_id(tx, &row.agent_id)?),
        lane_id: row.lane_id.clone(),
        project_id: row.project_id.clone(),
    };
    // A lease that ran across a monthly reset is charged to each period for its overlap
    // with it: one entry per period, dated inside that period.
    let mut segments: Vec<(i64, i64)> = Vec::new();
    let start = row.started.unwrap_or(at).min(at);
    if a.period == SpendPeriod::Monthly && at > start {
        let mut cur = start;
        while cur < at {
            let (_, to) = period_bounds(PeriodSel::Current, cur, a.reset_day);
            let end = to.min(at);
            segments.push((cur, end));
            cur = end;
        }
    }
    if segments.is_empty() {
        segments.push((start, at));
    }
    let parts = segments.len();
    let mut entry_id = String::new();
    for (i, (from, to)) in segments.iter().enumerate() {
        let part = if parts == 1 {
            lease_cost(row.lease.cost_per_hour, row.started, at)
        } else {
            lease_cost(row.lease.cost_per_hour, Some(*from), *to)
        };
        let last = i + 1 == parts;
        if part == 0.0 && !last {
            continue;
        }
        // Periods are half-open, so a lease ending exactly on the reset is dated just before it.
        let on_reset = a.period == SpendPeriod::Monthly && at == *to && at > *from;
        let note = if parts == 1 {
            format!("lease {} ({reason})", row.lease.lease_id)
        } else {
            format!(
                "lease {} ({reason}; period {} of {parts})",
                row.lease.lease_id,
                i + 1
            )
        };
        entry_id = insert_entry(
            tx,
            &a,
            account_project.as_deref(),
            part,
            if last && !on_reset { at } else { *to - 1 },
            &who,
            Some(&note),
            "lease",
            Some(&row.lease.lease_id),
            actor,
        )?;
    }
    tx.emit(
        actor,
        "spend.lease_ended",
        json!({
            "lease_id": row.lease.lease_id,
            "account_id": row.account_id,
            "account": a.name,
            "project_id": row.project_id,
            "agent_id": row.agent_id,
            "lane_id": row.lane_id,
            "cost": cost,
            "unit": a.unit,
            "reason": reason,
        }),
    )?;
    tx.execute(
        "UPDATE spend_leases SET status = 'ended', ended_at = ?2, entry_id = ?3 WHERE lease_id = ?1",
        params![row.lease.lease_id, at, entry_id],
    )?;
    Ok(())
}

/// Withdraw a pending lease: deny it, withdraw its approval decision (which mails the
/// approver and puts the lane back to `working`), and emit `spend.lease_withdrawn`.
fn withdraw_lease_tx(tx: &Tx<'_>, me: &AgentRef, row: &LeaseRow, reason: &str) -> Result<()> {
    tx.execute(
        "UPDATE spend_leases SET status = 'denied', ended_at = ?2 WHERE lease_id = ?1",
        params![row.lease.lease_id, tx.now],
    )?;
    if let Some(d) = &row.lease.decision_id {
        crate::decisions::withdraw_tx(tx, me, d, &format!("spend lease {reason}"))?;
    }
    tx.emit(
        &me.address,
        "spend.lease_withdrawn",
        json!({
            "lease_id": row.lease.lease_id,
            "account_id": row.account_id,
            "account": row.lease.account,
            "project_id": row.project_id,
            "agent_id": row.agent_id,
            "lane_id": row.lane_id,
            "decision_id": row.lease.decision_id,
            "reason": reason,
        }),
    )?;
    Ok(())
}

fn start_lease_tx(
    tx: &Tx<'_>,
    lease_id: &str,
    ttl: i64,
    actor: &str,
    row: &LeaseRow,
) -> Result<()> {
    tx.execute(
        "UPDATE spend_leases SET status = 'active', started_at = ?2, renewed_at = ?2,
           expires_at = ?2 + ?3 * 1000 WHERE lease_id = ?1",
        params![lease_id, tx.now, ttl],
    )?;
    tx.emit(
        actor,
        "spend.lease_started",
        json!({
            "lease_id": lease_id,
            "account_id": row.account_id,
            "account": row.lease.account,
            "project_id": row.project_id,
            "agent_id": row.agent_id,
            "lane_id": row.lane_id,
            "cost_per_hour": row.lease.cost_per_hour,
            "unit": row.lease.unit,
        }),
    )?;
    Ok(())
}

/// Lapsed active leases end at their expiry (called by the sweeper).
pub(crate) fn expire_spend_leases_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.node_id = ?1 AND l.status = 'active' AND l.expires_at <= ?2"
    ))?;
    let due = stmt
        .query_map(params![tx.node_id, tx.now], lease_from_row(tx.now))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &due {
        end_lease_tx(tx, row, row.expires.unwrap_or(tx.now), "system", "expired")?;
    }
    Ok(due.len())
}

/// Ends an agent's spend leases at session end: active ones are charged up to now, pending
/// ones are withdrawn along with their approval decision. Returns how many were closed.
pub(crate) fn end_agent_leases_tx(tx: &Tx<'_>, agent_id: &str, reason: &str) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.agent_id = ?1 AND l.status IN ('active', 'pending_approval')"
    ))?;
    let rows = stmt
        .query_map([agent_id], lease_from_row(tx.now))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(0);
    }
    let me = agent_by_id(tx, agent_id)?;
    for row in &rows {
        if row.lease.status == SpendLeaseStatus::Active {
            end_lease_tx(tx, row, tx.now, &me.address, reason)?;
        } else {
            withdraw_lease_tx(tx, &me, row, reason)?;
        }
    }
    Ok(rows.len())
}

/// What borrowing a resource needs from its linked spend account.
pub(crate) enum ResourceGate {
    /// Not linked (or the account is archived): borrow as usual.
    Free,
    /// A spend lease is running for this agent and resource: borrow as usual.
    Covered,
    /// Waiting for approval: don't take the resource lease yet.
    Pending(String),
}

/// Before an agent takes a new lease on a local resource: if the resource is linked to a
/// spend account, open (or reuse) the agent's spend lease for it. Accounts that require
/// approval leave it pending on a decision.
pub(crate) fn resource_gate_tx(
    tx: &Tx<'_>,
    resource_id: &str,
    me: &AgentRef,
    ttl: i64,
) -> Result<ResourceGate> {
    let linked: Option<(String, String, String)> = tx
        .query_row(
            "SELECT a.account_id, r.kind, r.name FROM resources r
             JOIN spend_accounts a ON a.account_id = r.spend_account_id
             WHERE r.resource_id = ?1 AND a.archived_at IS NULL",
            [resource_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((account_id, kind, name)) = linked else {
        return Ok(ResourceGate::Free);
    };
    let open: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT status, decision_id FROM spend_leases
             WHERE resource_id = ?1 AND agent_id = ?2 AND status IN ('active', 'pending_approval')
             ORDER BY requested_at DESC LIMIT 1",
            params![resource_id, me.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match open {
        Some((status, _)) if status == "active" => return Ok(ResourceGate::Covered),
        Some((_, Some(d))) => return Ok(ResourceGate::Pending(d)),
        _ => {}
    }
    let input = BorrowSpend {
        agent: me.address.clone(),
        account: account_id,
        note: Some(format!("resource {kind}:{name}")),
        ..Default::default()
    };
    let ttl = ttl.clamp(MIN_LEASE_TTL, MAX_LEASE_TTL);
    let out = borrow_spend_tx(tx, &input, ttl, Some(resource_id), None)?;
    Ok(match out.decision_id {
        Some(d) => ResourceGate::Pending(d),
        None => ResourceGate::Covered,
    })
}

/// Renew the spend lease paying for an agent's lease on a resource.
pub(crate) fn renew_resource_spend_tx(
    tx: &Tx<'_>,
    resource_id: &str,
    agent_id: &str,
    ttl: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE spend_leases SET renewed_at = ?3, expires_at = ?3 + ?4 * 1000, ttl_seconds = ?4
         WHERE resource_id = ?1 AND agent_id = ?2 AND status = 'active'",
        params![
            resource_id,
            agent_id,
            tx.now,
            ttl.clamp(MIN_LEASE_TTL, MAX_LEASE_TTL)
        ],
    )?;
    Ok(())
}

/// End spend leases whose resource lease is gone (released, expired, stopped, session end),
/// charging up to when the resource lease ended.
pub(crate) fn reconcile_resource_spend_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.node_id = ?1 AND l.status = 'active' AND l.resource_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM resource_leases rl
                           WHERE rl.resource_id = l.resource_id AND rl.agent_id = l.agent_id
                             AND rl.released_at IS NULL)"
    ))?;
    let rows = stmt
        .query_map([tx.node_id], lease_from_row(tx.now))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &rows {
        let released: Option<i64> = tx.query_row(
            "SELECT MAX(released_at) FROM resource_leases WHERE resource_id = ?1 AND agent_id = ?2",
            params![row.resource_id, row.agent_id],
            |r| r.get(0),
        )?;
        let at = released
            .unwrap_or(tx.now)
            .max(row.started.unwrap_or(tx.now))
            .min(tx.now);
        end_lease_tx(tx, row, at, "system", "resource_released")?;
    }
    Ok(rows.len())
}

/// Renew an agent's active spend leases by their TTL (session heartbeat).
pub(crate) fn renew_spend_leases_tx(tx: &Tx<'_>, agent_id: &str) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE spend_leases SET renewed_at = ?2, expires_at = ?2 + ttl_seconds * 1000
         WHERE agent_id = ?1 AND status = 'active'",
        params![agent_id, tx.now],
    )?)
}

/// A decision closed: start (approved) or deny the lease waiting on it.
pub(crate) fn decision_closed_tx(tx: &Tx<'_>, decision_id: &str, approved: bool) -> Result<()> {
    let lease_id: Option<String> = tx
        .query_row(
            "SELECT lease_id FROM spend_leases WHERE decision_id = ?1 AND status = 'pending_approval'",
            [decision_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(lease_id) = lease_id else {
        return Ok(());
    };
    let row = load_lease(tx, tx.now, &lease_id)?;
    if approved {
        start_lease_tx(tx, &lease_id, row.lease.ttl_seconds, "system", &row)?;
        if let Some(rid) = &row.resource_id {
            // Borrowing a linked resource: take the resource lease the approval was for. If
            // the resource can't be leased any more, the spend lease ends at once (no cost).
            let me = agent_by_id(tx, &row.agent_id)?;
            match crate::resources::grant_after_approval_tx(tx, rid, &me, row.lease.ttl_seconds) {
                Ok(()) => {}
                Err(Error::Conflict(_) | Error::NotFound(_)) => {
                    let row = load_lease(tx, tx.now, &lease_id)?;
                    end_lease_tx(tx, &row, tx.now, "system", "resource_unavailable")?;
                }
                Err(e) => return Err(e),
            }
        }
    } else {
        tx.execute(
            "UPDATE spend_leases SET status = 'denied', ended_at = ?2 WHERE lease_id = ?1",
            params![lease_id, tx.now],
        )?;
        tx.emit(
            "system",
            "spend.lease_denied",
            json!({ "lease_id": lease_id, "account_id": row.account_id, "project_id": row.project_id, "agent_id": row.agent_id, "decision_id": decision_id }),
        )?;
    }
    Ok(())
}

/// Open a spend lease (shared by `spend_borrow` and borrowing a linked resource). With
/// `requires_approval` it stays pending on a decision; otherwise it starts now.
fn borrow_spend_tx(
    tx: &Tx<'_>,
    input: &BorrowSpend,
    ttl: i64,
    resource_id: Option<&str>,
    scope: Option<&TokenScope>,
) -> Result<SpendLeaseOutcome> {
    let (a, account_project) = account_in(tx, tx.node_id, &input.account, scope)?;
    let cph = input
        .cost_per_hour
        .or(a.cost_per_hour)
        .ok_or_else(|| Error::Invalid(format!("{} has no cost_per_hour; give one", a.name)))?;
    check_amount("cost_per_hour", cph)?;
    let who = attribute(
        tx,
        tx.node_id,
        account_project.as_deref(),
        Some(&input.agent),
        input.lane.as_deref(),
        None,
        scope,
    )?;
    let me = who.agent.clone().expect("agent given");
    if scope.is_some()
        && !matches!(&who.project_id, Some(p) if attributed_visible(tx, scope, Some(p), None)?)
    {
        return Err(Error::Forbidden(
            "remote borrowing must be for a project in this token's globs (name a lane)".into(),
        ));
    }
    let lease_id = new_id();
    tx.execute(
        "INSERT INTO spend_leases (lease_id, node_id, account_id, agent_id, lane_id,
           project_id, cost_per_hour, ttl_seconds, status, requested_at, note, resource_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending_approval', ?9, ?10, ?11)",
        params![
            lease_id,
            tx.node_id,
            a.account_id,
            me.id,
            who.lane_id,
            who.project_id,
            cph,
            ttl,
            tx.now,
            input.note,
            resource_id
        ],
    )?;
    let row = load_lease(tx, tx.now, &lease_id)?;
    let mut decision_id = None;
    if a.requires_approval {
        let estimate = input
            .hours
            .map(|h| format!(" for {h} h (about {} {})", round(h * cph), a.unit))
            .unwrap_or_default();
        let decision = NewDecision {
            agent: me.address.clone(),
            question: format!(
                "Approve {} borrowing {} at {} {}/h{estimate}?{}",
                me.address,
                a.name,
                cph,
                a.unit,
                input
                    .note
                    .as_deref()
                    .map(|n| format!(" Purpose: {n}"))
                    .unwrap_or_default()
            ),
            options: vec![
                DecisionOption {
                    value: "approve".into(),
                    label: "Approve".into(),
                    description: None,
                },
                DecisionOption {
                    value: "deny".into(),
                    label: "Deny".into(),
                    description: None,
                },
            ],
            on_timeout: DecisionTimeout::KeepOpen,
            human: a.approver.clone(),
            project: match &who.project_id {
                Some(id) => Some(tx.query_row(
                    "SELECT slug FROM projects WHERE project_id = ?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )?),
                None => None,
            },
            lane: who.lane_id.clone(),
            ..Default::default()
        };
        validate(&decision)?;
        // The account's approver, else the node's `decider` setting.
        let approver = match a.approver.clone() {
            Some(x) => x,
            None => decider_tx(tx)?,
        };
        if who.project_id.is_none() {
            return Err(Error::Invalid(
                "approval needs a project: name a lane, or give the account or agent a project"
                    .into(),
            ));
        }
        let id = ask_tx(tx, &decision, &approver, scope)?;
        tx.execute(
            "UPDATE spend_leases SET decision_id = ?2 WHERE lease_id = ?1",
            params![lease_id, id],
        )?;
        tx.emit(
            &me.address,
            "spend.approval_requested",
            json!({ "lease_id": lease_id, "account_id": a.account_id, "account": a.name, "project_id": who.project_id, "agent_id": me.id, "decision_id": id, "cost_per_hour": cph, "unit": a.unit }),
        )?;
        decision_id = Some(id);
    } else {
        start_lease_tx(tx, &lease_id, ttl, &me.address, &row)?;
    }
    Ok(SpendLeaseOutcome {
        lease: load_lease(tx, tx.now, &lease_id)?.lease,
        decision_id,
    })
}

impl Store {
    /// Create a spend account (local callers only).
    pub fn create_spend_account(
        &self,
        input: NewSpendAccount,
        scope: Option<&TokenScope>,
    ) -> Result<SpendAccount> {
        local_only(scope)?;
        let name = check_name(&input.name)?;
        if input.unit.trim().is_empty() || input.unit.len() > 32 {
            return Err(Error::Invalid(
                "unit must be 1-32 characters (USD, credits, resets)".into(),
            ));
        }
        if let Some(b) = input.budget {
            check_amount("budget", b)?;
        }
        if let Some(c) = input.cost_per_hour {
            check_amount("cost_per_hour", c)?;
        }
        check_reset_day(input.reset_day)?;
        self.write(|tx| {
            let taken = tx
                .query_row(
                    "SELECT 1 FROM spend_accounts WHERE node_id = ?1 AND name = ?2 AND archived_at IS NULL",
                    params![tx.node_id, name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if taken {
                return Err(Error::Conflict(format!("a spend account is already named {name:?}")));
            }
            let project_id = match &input.project {
                Some(p) => Some(find_project(tx, tx.node_id, p)?.project_id),
                None => None,
            };
            let account_id = new_id();
            let event_id = tx.emit(
                "local",
                "spend.account_created",
                json!({
                    "account_id": account_id,
                    "name": name,
                    "kind": input.kind.as_str(),
                    "unit": input.unit.trim(),
                    "budget": input.budget,
                    "period": input.period.as_str(),
                    "reset_day": input.reset_day,
                    "requires_approval": input.requires_approval,
                    "project_id": project_id,
                }),
            )?;
            tx.execute(
                "INSERT INTO spend_accounts (account_id, node_id, name, kind, unit, budget, period,
                   reset_day, cost_per_hour, requires_approval, approver, project_id, description,
                   created_at, updated_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)",
                params![
                    account_id,
                    tx.node_id,
                    name,
                    input.kind.as_str(),
                    input.unit.trim(),
                    input.budget,
                    input.period.as_str(),
                    input.reset_day,
                    input.cost_per_hour,
                    input.requires_approval,
                    input.approver,
                    project_id,
                    input.description,
                    tx.now,
                    event_id
                ],
            )?;
            Ok(account(tx, tx.node_id, &account_id)?.0)
        })
    }

    /// Change a spend account (local callers only).
    pub fn update_spend_account(
        &self,
        key: &str,
        u: SpendAccountUpdate,
        scope: Option<&TokenScope>,
    ) -> Result<SpendAccount> {
        local_only(scope)?;
        if let Some(b) = u.budget {
            check_amount("budget", b)?;
        }
        if let Some(c) = u.cost_per_hour {
            check_amount("cost_per_hour", c)?;
        }
        if let Some(d) = u.reset_day {
            check_reset_day(d)?;
        }
        self.write(|tx| {
            let (a, project_id) = account(tx, tx.node_id, key)?;
            let budget = if u.clear_budget { None } else { u.budget.or(a.budget) };
            tx.execute(
                "UPDATE spend_accounts SET budget = ?2, reset_day = ?3, cost_per_hour = ?4,
                   requires_approval = ?5, approver = ?6, description = ?7, updated_at = ?8
                 WHERE account_id = ?1",
                params![
                    a.account_id,
                    budget,
                    u.reset_day.unwrap_or(a.reset_day),
                    u.cost_per_hour.or(a.cost_per_hour),
                    u.requires_approval.unwrap_or(a.requires_approval),
                    u.approver.clone().or(a.approver.clone()),
                    u.description.clone().or(a.description.clone()),
                    tx.now
                ],
            )?;
            tx.emit(
                "local",
                "spend.account_updated",
                json!({ "account_id": a.account_id, "name": a.name, "project_id": project_id, "budget": budget }),
            )?;
            Ok(account(tx, tx.node_id, &a.account_id)?.0)
        })
    }

    /// Archive a spend account: hidden from reports, history kept (local callers only).
    pub fn archive_spend_account(
        &self,
        key: &str,
        scope: Option<&TokenScope>,
    ) -> Result<SpendAccount> {
        local_only(scope)?;
        self.write(|tx| {
            let (a, project_id) = account(tx, tx.node_id, key)?;
            let live: i64 = tx.query_row(
                "SELECT COUNT(*) FROM spend_leases WHERE account_id = ?1 AND status IN ('active', 'pending_approval')",
                [&a.account_id],
                |r| r.get(0),
            )?;
            if live > 0 {
                return Err(Error::Conflict(format!(
                    "{} has {live} open lease(s); release them first",
                    a.name
                )));
            }
            tx.execute(
                "UPDATE spend_accounts SET archived_at = ?2, updated_at = ?2 WHERE account_id = ?1",
                params![a.account_id, tx.now],
            )?;
            tx.emit(
                "local",
                "spend.account_updated",
                json!({ "account_id": a.account_id, "name": a.name, "project_id": project_id, "archived": true }),
            )?;
            Ok(a)
        })
    }

    /// Live accounts the scope may see.
    pub fn spend_accounts(&self, scope: Option<&TokenScope>) -> Result<Vec<SpendAccount>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ACCOUNT_SELECT} WHERE a.node_id = ?1 AND a.archived_at IS NULL ORDER BY a.name"
            ))?;
            let rows = stmt
                .query_map([self.node_id()], account_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for (a, p) in rows {
                if account_visible(conn, scope, p.as_deref())? {
                    out.push(a);
                }
            }
            Ok(out)
        })
    }

    /// Record an amount spent.
    pub fn record_spend(
        &self,
        input: RecordSpend,
        scope: Option<&TokenScope>,
    ) -> Result<SpendEntry> {
        check_amount("amount", input.amount)?;
        if input.amount == 0.0 {
            return Err(Error::Invalid("amount must be greater than zero".into()));
        }
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let (a, account_project) = account_in(tx, tx.node_id, &input.account, scope)?;
            let who = attribute(
                tx,
                tx.node_id,
                account_project.as_deref(),
                input.agent.as_deref(),
                input.lane.as_deref(),
                input.project.as_deref(),
                scope,
            )?;
            if scope.is_some() {
                match &who.project_id {
                    Some(p) if attributed_visible(tx, scope, Some(p), None)? => {}
                    _ => {
                        return Err(Error::Forbidden(
                            "remote spend must be attributed to a project in this token's globs (name a lane or project)".into(),
                        ))
                    }
                }
            }
            let at = input.at.unwrap_or(tx.now);
            let actor = who.agent.as_ref().map_or("local".to_string(), |a| a.address.clone());
            let id = insert_entry(
                tx,
                &a,
                account_project.as_deref(),
                round(input.amount),
                at,
                &who,
                input.note.as_deref(),
                "manual",
                None,
                &actor,
            )?;
            entry(tx, &id)
        })
    }

    /// Start using a costed account by the hour. With `requires_approval`, asks the approver
    /// first and returns a pending lease plus the decision id.
    pub fn borrow_spend(
        &self,
        input: BorrowSpend,
        scope: Option<&TokenScope>,
    ) -> Result<SpendLeaseOutcome> {
        let ttl = input
            .ttl_seconds
            .unwrap_or(DEFAULT_SPEND_LEASE_TTL_SECONDS)
            .clamp(MIN_LEASE_TTL, MAX_LEASE_TTL);
        if let Some(h) = input.hours {
            check_amount("hours", h)?;
        }
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let out = borrow_spend_tx(tx, &input, ttl, None, scope)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            crate::mail::touch(tx, &me.id)?;
            Ok(out)
        })
    }

    /// Link a registered resource to a spend account, or unlink it with `None` (local callers
    /// only). Borrowing a linked resource opens a spend lease for the borrower, behind the
    /// account's approval gate, and its cost accrues while the resource lease lives.
    pub fn link_resource_spend(
        &self,
        target: &crate::resources::ResourceRef,
        account_key: Option<&str>,
        scope: Option<&TokenScope>,
    ) -> Result<crate::resources::Resource> {
        local_only(scope)?;
        self.write(|tx| {
            let rid = crate::resources::resolve(tx, tx.node_id, target)?;
            let (res_node, project_id): (String, Option<String>) = tx.query_row(
                "SELECT node_id, project_id FROM resources WHERE resource_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if res_node != tx.node_id {
                return Err(Error::Forbidden(
                    "only resources registered on this node can be linked to a spend account"
                        .into(),
                ));
            }
            let account_id = match account_key {
                Some(k) => {
                    let (a, _) = account(tx, tx.node_id, k)?;
                    if a.cost_per_hour.is_none() {
                        return Err(Error::Invalid(format!(
                            "{} has no cost_per_hour; set one before linking a resource",
                            a.name
                        )));
                    }
                    Some(a.account_id)
                }
                None => None,
            };
            tx.execute(
                "UPDATE resources SET spend_account_id = ?2 WHERE resource_id = ?1",
                params![rid, account_id],
            )?;
            tx.emit(
                "local",
                "spend.resource_linked",
                json!({ "resource_id": rid, "account_id": account_id, "project_id": project_id }),
            )?;
            crate::resources::get_resource(tx, &rid, tx.now)
        })
    }

    /// End your lease (its cost is recorded), or withdraw a pending one.
    pub fn release_spend(
        &self,
        agent: &str,
        lease_id: &str,
        scope: Option<&TokenScope>,
    ) -> Result<SpendLease> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let row = load_lease(tx, tx.now, lease_id)?;
            if !attributed_visible(tx, scope, row.project_id.as_deref(), Some(&row.agent_id))? {
                // A hidden lease is a missing one (same error and message).
                return Err(Error::NotFound(format!("no spend lease {lease_id:?}")));
            }
            if row.agent_id != me.id {
                return Err(Error::Forbidden(format!(
                    "lease {} belongs to {}",
                    row.lease.lease_id, row.lease.owner
                )));
            }
            if row.lease.status == SpendLeaseStatus::Active {
                if let Some(rid) = &row.resource_id {
                    let live: bool = tx
                        .query_row(
                            "SELECT 1 FROM resource_leases
                             WHERE resource_id = ?1 AND agent_id = ?2 AND released_at IS NULL",
                            params![rid, row.agent_id],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if live {
                        // It pays for a resource lease that is still held: ending it alone
                        // would stop charging while the resource stays in use.
                        return Err(Error::Conflict(format!(
                            "lease {} pays for a resource you still hold: release the resource (`another release {rid}`) and it ends with it",
                            row.lease.lease_id
                        )));
                    }
                }
            }
            match row.lease.status {
                SpendLeaseStatus::Active => {
                    end_lease_tx(tx, &row, tx.now, &me.address, "released")?
                }
                SpendLeaseStatus::PendingApproval => withdraw_lease_tx(tx, &me, &row, "withdrawn")?,
                s => {
                    return Err(Error::Conflict(format!(
                        "lease {} is already {}",
                        row.lease.lease_id,
                        s.as_str()
                    )))
                }
            }
            Ok(load_lease(tx, tx.now, &row.lease.lease_id)?.lease)
        })
    }

    /// Spend leases (open ones unless `all`), newest first, within `scope`.
    pub fn spend_leases(&self, all: bool, scope: Option<&TokenScope>) -> Result<Vec<SpendLease>> {
        self.sweep()?;
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{LEASE_SELECT} WHERE l.node_id = ?1
                   AND (?2 OR l.status IN ('active', 'pending_approval'))
                 ORDER BY l.requested_at DESC, l.lease_id DESC LIMIT 200"
            ))?;
            let rows = stmt
                .query_map(params![self.node_id(), all], lease_from_row(self.now()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for r in rows {
                if attributed_visible(conn, scope, r.project_id.as_deref(), Some(&r.agent_id))? {
                    out.push(r.lease);
                }
            }
            Ok(out)
        })
    }

    /// Spend by period, lane and agent, with a burn-down per account.
    pub fn spend_report(&self, q: SpendQuery, scope: Option<&TokenScope>) -> Result<SpendReport> {
        self.sweep()?;
        let now = self.now();
        self.read(|conn| {
            let lane_id = match &q.lane {
                Some(l) => Some(resolve_lane_in(conn, self.node_id(), l, scope)?.0.lane_id),
                None => None,
            };
            let agent_id = match &q.agent {
                // As the caller sees agents: a hidden agent is the same not_found as a
                // missing one, and ambiguity names only visible agents.
                Some(a) => {
                    Some(crate::identity::resolve_agent_in(conn, self.node_id(), a, scope)?.id)
                }
                None => None,
            };
            let mut stmt = conn.prepare(&format!(
                "{ACCOUNT_SELECT} WHERE a.node_id = ?1 AND a.archived_at IS NULL ORDER BY a.name"
            ))?;
            let accounts = stmt
                .query_map([self.node_id()], account_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            let mut by_lane: Vec<SpendGroup> = Vec::new();
            let mut by_agent: Vec<SpendGroup> = Vec::new();
            let add = |groups: &mut Vec<SpendGroup>, key: String, a: &SpendAccount, amount: f64| {
                if amount == 0.0 {
                    return;
                }
                match groups
                    .iter_mut()
                    .find(|g| g.key == key && g.account == a.name)
                {
                    Some(g) => g.amount = round(g.amount + amount),
                    None => groups.push(SpendGroup {
                        key,
                        account: a.name.clone(),
                        unit: a.unit.clone(),
                        amount: round(amount),
                    }),
                }
            };
            for (a, account_project) in accounts {
                if let Some(k) = &q.account {
                    if a.name != k.to_ascii_lowercase() && a.account_id != *k {
                        continue;
                    }
                }
                if !account_visible(conn, scope, account_project.as_deref())? {
                    continue;
                }
                let partial = scope.is_some() && account_project.is_none();
                let (from, to) = match a.period {
                    SpendPeriod::Monthly => period_bounds(q.period, now, a.reset_day),
                    SpendPeriod::None => (0, i64::MAX),
                };
                let filtered = lane_id.is_some() || agent_id.is_some();
                let mut stmt = conn.prepare(
                    "SELECT e.amount, e.project_id, e.agent_id, ag.address, ln.name
                     FROM spend_entries e
                     LEFT JOIN agents ag ON ag.agent_id = e.agent_id
                     LEFT JOIN lanes ln ON ln.lane_id = e.lane_id
                     WHERE e.account_id = ?1 AND e.at >= ?2 AND e.at < ?3
                       AND (?4 IS NULL OR e.lane_id = ?4) AND (?5 IS NULL OR e.agent_id = ?5)",
                )?;
                let entries = stmt
                    .query_map(params![a.account_id, from, to, lane_id, agent_id], |r| {
                        Ok((
                            r.get::<_, f64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut spent = 0.0;
                // Everyone's use, for a shared account's budget figures (scoped callers see
                // the remaining budget, not who used it).
                let mut total = 0.0;
                for (amount, project, agent, address, lane) in entries {
                    total += amount;
                    if !attributed_visible(conn, scope, project.as_deref(), agent.as_deref())? {
                        continue;
                    }
                    spent += amount;
                    add(
                        &mut by_lane,
                        lane.unwrap_or_else(|| "(none)".into()),
                        &a,
                        amount,
                    );
                    add(
                        &mut by_agent,
                        address.unwrap_or_else(|| "(none)".into()),
                        &a,
                        amount,
                    );
                }
                let mut stmt = conn.prepare(&format!(
                    "{LEASE_SELECT} WHERE l.account_id = ?1 AND l.status = 'active'
                       AND (?2 IS NULL OR l.lane_id = ?2) AND (?3 IS NULL OR l.agent_id = ?3)"
                ))?;
                let leases = stmt
                    .query_map(
                        params![a.account_id, lane_id, agent_id],
                        lease_from_row(now),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut accrued = 0.0;
                let mut active = 0;
                // An open lease counts for its overlap with the selected period: the running
                // period gets it up to now, an earlier one (`previous`, a past month) the part
                // before its end, so a lease kept across a reset isn't missing from either.
                let overlap = |l: &LeaseRow| -> Option<f64> {
                    let started = l.started?;
                    let (start, end) = if a.period == SpendPeriod::None {
                        (started, now)
                    } else {
                        (started.max(from), now.min(to))
                    };
                    let active_now = end == start
                        && (a.period == SpendPeriod::None || (from <= now && now < to));
                    (end > start || active_now)
                        .then(|| lease_cost(l.lease.cost_per_hour, Some(start), end))
                };
                for l in leases {
                    let Some(cost) = overlap(&l) else {
                        continue;
                    };
                    total += cost;
                    if !attributed_visible(conn, scope, l.project_id.as_deref(), Some(&l.agent_id))?
                    {
                        continue;
                    }
                    active += 1;
                    accrued += cost;
                    add(
                        &mut by_lane,
                        l.lease.lane.clone().unwrap_or_else(|| "(none)".into()),
                        &a,
                        cost,
                    );
                    add(&mut by_agent, l.lease.owner.clone(), &a, cost);
                }
                let mine = spent + accrued;
                // Budget figures are for the whole account; a lane or agent filter hides them.
                let used = if partial { total } else { mine };
                let budget = if filtered { None } else { a.budget };
                let monthly = a.period == SpendPeriod::Monthly;
                let days_total = monthly.then(|| (to - from) as f64 / DAY_MS);
                let days_elapsed = monthly.then(|| ((now.min(to) - from) as f64 / DAY_MS).max(0.0));
                let projected = match (days_elapsed, days_total) {
                    (Some(e), Some(t)) if q.period == PeriodSel::Current && e > 0.01 => {
                        Some(round(used / e * t))
                    }
                    (Some(_), Some(_)) => Some(round(used)),
                    _ => None,
                };
                let remaining = budget.map(|b| round(b - used));
                let daily_left = match (remaining, days_elapsed, days_total) {
                    (Some(r), Some(e), Some(t))
                        if t - e > 0.01 && q.period == PeriodSel::Current =>
                    {
                        Some(round(r.max(0.0) / (t - e)))
                    }
                    _ => None,
                };
                let status = match budget {
                    None => BudgetStatus::NoBudget,
                    Some(b) if used >= b => BudgetStatus::Over,
                    Some(b) if used >= 0.8 * b || projected.is_some_and(|p| p >= b) => {
                        BudgetStatus::Warning
                    }
                    Some(_) => BudgetStatus::Ok,
                };
                out.push(AccountSpend {
                    period_start: monthly.then(|| fmt_ts(from)),
                    period_end: monthly.then(|| fmt_ts(to)),
                    spent: round(spent),
                    accrued: round(accrued),
                    used_pct: budget.filter(|b| *b > 0.0).map(|b| round(used / b * 100.0)),
                    budget,
                    remaining,
                    days_elapsed: days_elapsed.map(round),
                    days_total: days_total.map(round),
                    projected,
                    daily_left,
                    status,
                    active_leases: active,
                    partial,
                    account: a,
                });
            }
            by_lane.sort_by(|x, y| y.amount.total_cmp(&x.amount));
            by_agent.sort_by(|x, y| y.amount.total_cmp(&x.amount));
            let mut report = SpendReport {
                generated_at: fmt_ts(now),
                accounts: out,
                by_lane,
                by_agent,
                markdown: String::new(),
            };
            report.markdown = spend_markdown(&report);
            Ok(report)
        })
    }
}

fn entry(conn: &Connection, id: &str) -> Result<SpendEntry> {
    Ok(conn.query_row(
        "SELECT e.entry_id, a.name, e.amount, a.unit, e.at, ag.address, ln.name, p.slug, e.note,
           e.source, e.lease_id
         FROM spend_entries e JOIN spend_accounts a ON a.account_id = e.account_id
         LEFT JOIN agents ag ON ag.agent_id = e.agent_id
         LEFT JOIN lanes ln ON ln.lane_id = e.lane_id
         LEFT JOIN projects p ON p.project_id = e.project_id
         WHERE e.entry_id = ?1",
        [id],
        |r| {
            Ok(SpendEntry {
                entry_id: r.get(0)?,
                account: r.get(1)?,
                amount: r.get(2)?,
                unit: r.get(3)?,
                at: fmt_ts(r.get(4)?),
                agent: r.get(5)?,
                lane: r.get(6)?,
                project: r.get(7)?,
                note: r.get(8)?,
                source: r.get(9)?,
                lease_id: r.get(10)?,
            })
        },
    )?)
}

fn num(x: f64) -> String {
    if (x - x.round()).abs() < 0.005 {
        format!("{}", x.round() as i64)
    } else {
        format!("{x:.2}")
    }
}

fn bar(pct: f64) -> String {
    let filled = ((pct / 10.0).round() as usize).min(10);
    format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled))
}

/// The burn-down as short markdown: one line per account, then top lanes and agents.
pub fn spend_markdown(r: &SpendReport) -> String {
    if r.accounts.is_empty() {
        return "No spend accounts. Add one with `another spend account add`.".into();
    }
    let mut out = String::from("**Spend**\n");
    for s in &r.accounts {
        let a = &s.account;
        let used = s.spent + s.accrued;
        let line = match (s.budget, s.used_pct) {
            (Some(b), Some(p)) => {
                let mut l = if s.partial {
                    // Shared account, scoped caller: whole-account budget, own use.
                    format!(
                        "`{}` {} of {} {} left  {}% used  {}  (your projects: {})",
                        a.name,
                        num(s.remaining.unwrap_or_default().max(0.0)),
                        num(b),
                        a.unit,
                        p.round(),
                        bar(p),
                        num(used)
                    )
                } else {
                    format!(
                        "`{}` {} / {} {}  {}%  {}",
                        a.name,
                        num(used),
                        num(b),
                        a.unit,
                        p.round(),
                        bar(p)
                    )
                };
                if let (Some(e), Some(t)) = (s.days_elapsed, s.days_total) {
                    l.push_str(&format!("  day {}/{}", e.ceil() as i64, t.round() as i64));
                }
                if let Some(pr) = s.projected {
                    let flag = match s.status {
                        BudgetStatus::Over => " (over)",
                        BudgetStatus::Warning if pr >= b => " (over pace)",
                        BudgetStatus::Warning => " (80%+)",
                        _ => "",
                    };
                    l.push_str(&format!(" → {}{flag}", num(pr)));
                }
                l
            }
            _ => format!(
                "`{}` {} {}{}",
                a.name,
                num(used),
                a.unit,
                if s.partial { " (your projects)" } else { "" }
            ),
        };
        out.push_str("- ");
        out.push_str(&line);
        if s.active_leases > 0 {
            out.push_str(&format!(" · {} lease(s) open", s.active_leases));
        }
        out.push('\n');
    }
    for (title, groups) in [("lanes", &r.by_lane), ("agents", &r.by_agent)] {
        if groups.is_empty() {
            continue;
        }
        let top: Vec<String> = groups
            .iter()
            .take(5)
            .map(|g| format!("{} {} {} ({})", g.key, num(g.amount), g.unit, g.account))
            .collect();
        out.push_str(&format!("Top {title}: {}\n", top.join(", ")));
    }
    out
}

/// Spend recorded since `since_ms`, per account (name, unit, amount), as `scope` sees it.
pub(crate) fn spent_since(
    conn: &Connection,
    node_id: &str,
    since_ms: i64,
    scope: Option<&TokenScope>,
) -> Result<Vec<(String, String, f64)>> {
    // Keyed by account id: an archived account and a new one with the same name are
    // different accounts (and may have different units).
    let mut stmt = conn.prepare(
        "SELECT a.account_id, a.unit, e.amount, e.project_id, e.agent_id, a.project_id
         FROM spend_entries e JOIN spend_accounts a ON a.account_id = e.account_id
         WHERE e.node_id = ?1 AND e.at >= ?2 ORDER BY a.name",
    )?;
    let rows = stmt
        .query_map(params![node_id, since_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: Vec<(String, String, f64)> = Vec::new();
    for (account_id, unit, amount, project, agent, account_project) in rows {
        if !account_visible(conn, scope, account_project.as_deref())?
            || !attributed_visible(conn, scope, project.as_deref(), agent.as_deref())?
        {
            continue;
        }
        match out.iter_mut().find(|(id, _, _)| *id == account_id) {
            Some(x) => x.2 = round(x.2 + amount),
            None => out.push((account_id, unit, round(amount))),
        }
    }
    Ok(out)
}

/// Pending spend approvals `scope` may see, counted in SQL (no listing limit).
pub(crate) fn pending_approvals(
    conn: &Connection,
    node_id: &str,
    scope: Option<&TokenScope>,
) -> Result<u32> {
    let mut stmt = conn.prepare(
        "SELECT project_id, agent_id, COUNT(*) FROM spend_leases
         WHERE node_id = ?1 AND status = 'pending_approval' GROUP BY project_id, agent_id",
    )?;
    let groups = stmt
        .query_map([node_id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut n = 0;
    for (project, agent, count) in groups {
        if attributed_visible(conn, scope, project.as_deref(), Some(&agent))? {
            n += count as u32;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i32, m: u32, d: u32) -> i64 {
        ms_of(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    #[test]
    fn periods() {
        let now = ms(2026, 11, 3) + 5_000;
        assert_eq!(
            period_bounds(PeriodSel::Current, now, 1),
            (ms(2026, 11, 1), ms(2026, 12, 1))
        );
        assert_eq!(
            period_bounds(PeriodSel::Current, now, 15),
            (ms(2026, 10, 15), ms(2026, 11, 15))
        );
        assert_eq!(
            period_bounds(PeriodSel::Previous, now, 1),
            (ms(2026, 10, 1), ms(2026, 11, 1))
        );
        assert_eq!(
            period_bounds(PeriodSel::Month(2026, 12), now, 3),
            (ms(2026, 12, 3), ms(2027, 1, 3))
        );
        assert_eq!(
            period_bounds(PeriodSel::Current, ms(2027, 1, 2), 5),
            (ms(2026, 12, 5), ms(2027, 1, 5))
        );
        assert_eq!(
            "2026-02".parse::<PeriodSel>().unwrap(),
            PeriodSel::Month(2026, 2)
        );
        assert!("2026-13".parse::<PeriodSel>().is_err());
    }
}
