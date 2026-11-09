//! MCP tools for the spend ledger (docs/design/05-spend.md). Remote callers see their
//! projects' accounts and their projects' share of shared accounts; accounts themselves are
//! configured locally.

use another_core::{
    BorrowSpend, NewSpendAccount, RecordSpend, SpendAccount, SpendAccountUpdate, SpendEntry,
    SpendKind, SpendLease, SpendLeaseOutcome, SpendPeriod, SpendQuery, SpendReport,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::{AnotherMcp, ToolResult};
use crate::remote;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendReportArgs {
    /// current (default), previous, or YYYY-MM.
    pub period: Option<String>,
    pub account: Option<String>,
    pub lane: Option<String>,
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendAccountsArgs {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpendAccountsOut {
    pub accounts: Vec<SpendAccount>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendRecordArgs {
    /// The agent that spent it.
    pub agent: String,
    /// Account name.
    pub account: String,
    /// In the account's unit (USD, credits, resets, ...).
    pub amount: f64,
    pub lane: Option<String>,
    pub project: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendBorrowArgs {
    pub agent: String,
    pub account: String,
    pub lane: Option<String>,
    /// Rate per hour (default: the account's).
    pub cost_per_hour: Option<f64>,
    /// Lease length, renewed by heartbeat (default 3600).
    pub ttl_seconds: Option<i64>,
    /// Expected hours, shown in the approval question.
    pub hours: Option<f64>,
    /// What it is for.
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendReleaseArgs {
    pub agent: String,
    pub lease_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendLeasesArgs {
    /// Include ended and denied leases.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpendLeasesOut {
    pub leases: Vec<SpendLease>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpendAccountUpsertArgs {
    /// Account name (created if new, else updated).
    pub name: String,
    /// gpu_pod, api_credits, plan_reset, server, domain (required to create).
    pub kind: Option<SpendKind>,
    /// USD, credits, resets, hours ... (required to create).
    pub unit: Option<String>,
    pub budget: Option<f64>,
    /// monthly (default) or none.
    pub period: Option<SpendPeriod>,
    /// 1-28 (default 1).
    pub reset_day: Option<u32>,
    pub cost_per_hour: Option<f64>,
    pub requires_approval: Option<bool>,
    pub approver: Option<String>,
    /// Project path or slug; omit for a shared account.
    pub project: Option<String>,
    pub description: Option<String>,
}

#[tool_router(router = spend_router, vis = "pub(crate)")]
impl AnotherMcp {
    #[tool(
        description = "Spend report: per account spent, accrued (open leases), budget, \
        remaining, projected and status for the period, plus totals by lane and agent and a \
        short burn-down in markdown."
    )]
    async fn spend_report(
        &self,
        Parameters(a): Parameters<SpendReportArgs>,
    ) -> ToolResult<SpendReport> {
        let scope = remote::current_scope();
        self.run(move |s| {
            s.spend_report(
                SpendQuery {
                    period: a.period.as_deref().unwrap_or("current").parse()?,
                    account: a.account,
                    lane: a.lane,
                    agent: a.agent,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(description = "List spend accounts (budgets, units, rates, approval rules).")]
    async fn spend_accounts(
        &self,
        Parameters(_): Parameters<SpendAccountsArgs>,
    ) -> ToolResult<SpendAccountsOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(SpendAccountsOut {
                accounts: s.spend_accounts(scope.as_ref())?,
            })
        })
        .await
    }

    #[tool(description = "Record an amount spent against an account (for a lane or project).")]
    async fn spend_record(
        &self,
        Parameters(a): Parameters<SpendRecordArgs>,
    ) -> ToolResult<SpendEntry> {
        let scope = remote::current_scope();
        self.run(move |s| {
            s.record_spend(
                RecordSpend {
                    agent: Some(a.agent),
                    account: a.account,
                    amount: a.amount,
                    lane: a.lane,
                    project: a.project,
                    note: a.note,
                    at: None,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(
        description = "Start using a costed account by the hour (e.g. a GPU pod). If the \
        account requires approval, this asks a person (a decision) and the lease starts only \
        when approved; check decision_show / your inbox. Renewed by heartbeat; release it when \
        done."
    )]
    async fn spend_borrow(
        &self,
        Parameters(a): Parameters<SpendBorrowArgs>,
    ) -> ToolResult<SpendLeaseOutcome> {
        let scope = remote::current_scope();
        self.run(move |s| {
            s.borrow_spend(
                BorrowSpend {
                    agent: a.agent,
                    account: a.account,
                    lane: a.lane,
                    cost_per_hour: a.cost_per_hour,
                    ttl_seconds: a.ttl_seconds,
                    hours: a.hours,
                    note: a.note,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(description = "End your spend lease (its cost is recorded) or withdraw a pending one.")]
    async fn spend_release(
        &self,
        Parameters(a): Parameters<SpendReleaseArgs>,
    ) -> ToolResult<SpendLease> {
        let scope = remote::current_scope();
        self.run(move |s| s.release_spend(&a.agent, &a.lease_id, scope.as_ref()))
            .await
    }

    #[tool(description = "List spend leases (open ones unless all=true).")]
    async fn spend_leases(
        &self,
        Parameters(a): Parameters<SpendLeasesArgs>,
    ) -> ToolResult<SpendLeasesOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(SpendLeasesOut {
                leases: s.spend_leases(a.all, scope.as_ref())?,
            })
        })
        .await
    }

    #[tool(
        description = "Create or update a spend account (local callers only): kind, unit, \
        monthly budget with reset day, hourly rate, approval rule, project."
    )]
    async fn spend_account_upsert(
        &self,
        Parameters(a): Parameters<SpendAccountUpsertArgs>,
    ) -> ToolResult<SpendAccount> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let exists = s
                .spend_accounts(None)?
                .into_iter()
                .any(|x| x.name == a.name.trim().to_ascii_lowercase());
            if exists {
                s.update_spend_account(
                    &a.name,
                    SpendAccountUpdate {
                        budget: a.budget,
                        clear_budget: false,
                        reset_day: a.reset_day,
                        cost_per_hour: a.cost_per_hour,
                        requires_approval: a.requires_approval,
                        approver: a.approver,
                        description: a.description,
                    },
                    scope.as_ref(),
                )
            } else {
                s.create_spend_account(
                    NewSpendAccount {
                        name: a.name,
                        kind: a.kind.ok_or_else(|| {
                            another_core::Error::Invalid(
                                "kind is required to create an account".into(),
                            )
                        })?,
                        unit: a.unit.ok_or_else(|| {
                            another_core::Error::Invalid(
                                "unit is required to create an account".into(),
                            )
                        })?,
                        budget: a.budget,
                        period: a.period.unwrap_or_default(),
                        reset_day: a.reset_day.unwrap_or(1),
                        cost_per_hour: a.cost_per_hour,
                        requires_approval: a.requires_approval.unwrap_or(false),
                        approver: a.approver,
                        project: a.project,
                        description: a.description,
                    },
                    scope.as_ref(),
                )
            }
        })
        .await
    }
}
