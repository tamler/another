//! MCP tools for lanes (docs/design/01-lanes.md). Remote callers are limited to lanes in
//! their token's project globs; the scope comes from [`remote::current_scope`].

use another_core::{
    Lane, LaneDetail, LaneFilter, LaneLinkKind, LaneReport, LaneReportInput, LaneStatus,
    LaneUpdate, NewLane, ReportKind,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::{AnotherMcp, ToolResult};
use crate::remote;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneCreateArgs {
    /// Acting agent (the creator, usually the manager).
    pub agent: String,
    /// 1-64 chars: letters, digits, '-', '_', '.'; unique among open lanes.
    pub name: String,
    /// Project absolute path or slug (required).
    pub project: String,
    /// Agent that leads the work.
    pub lead: String,
    /// The brief: what to do, constraints, definition of done.
    #[serde(default)]
    pub charter: String,
    /// Initial status (default planned).
    pub status: Option<LaneStatus>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneListArgs {
    pub status: Option<LaneStatus>,
    /// Project absolute path or slug.
    pub project: Option<String>,
    pub lead: Option<String>,
    /// Include done and abandoned lanes.
    #[serde(default)]
    pub include_closed: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HoldsByLaneArgs {
    /// Project absolute path or slug.
    pub project: Option<String>,
    /// Only this agent's holds.
    pub agent: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HoldsByLaneOut {
    /// Lanes first (by name), then holds outside any lane.
    pub lanes: Vec<another_core::LaneHolds>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LanesOut {
    pub lanes: Vec<Lane>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneShowArgs {
    /// Lane id or name.
    pub lane: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneUpdateArgs {
    pub agent: String,
    /// Lane id or name.
    pub lane: String,
    /// New open status (planned, working, blocked, needs_decision); finish with lane_report
    /// kind=done or lane_close.
    pub status: Option<LaneStatus>,
    pub charter: Option<String>,
    pub lead: Option<String>,
    #[serde(default)]
    pub link_threads: Vec<String>,
    #[serde(default)]
    pub link_requests: Vec<String>,
    #[serde(default)]
    pub link_holds: Vec<String>,
    #[serde(default)]
    pub link_messages: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportKindArg {
    #[default]
    Status,
    Done,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneReportArgs {
    pub agent: String,
    /// Lane id or name.
    pub lane: String,
    /// status (default) or done. A done report closes the lane.
    #[serde(default)]
    pub kind: ReportKindArg,
    /// What happened (first line is used as the mail subject).
    pub summary: String,
    /// Percent complete, 0-100.
    pub progress: Option<u8>,
    /// Optional new open status, e.g. blocked or needs_decision.
    pub status: Option<LaneStatus>,
    /// URLs or ids (CI runs, threads, issues).
    #[serde(default)]
    pub links: Vec<String>,
    /// Files or URLs produced.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Done reports: what was delivered (required for done).
    pub outcome: Option<String>,
    /// Done reports: pull request URL.
    pub pr_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneCloseArgs {
    pub agent: String,
    /// Lane id or name.
    pub lane: String,
    /// Close as abandoned instead of done.
    #[serde(default)]
    pub abandon: bool,
    pub outcome: Option<String>,
}

#[tool_router(router = lane_router, vis = "pub(crate)")]
impl AnotherMcp {
    #[tool(
        description = "Create a lane: a named unit of work with a charter, a lead agent and a \
        project. Status starts at planned."
    )]
    async fn lane_create(
        &self,
        Parameters(a): Parameters<LaneCreateArgs>,
    ) -> ToolResult<LaneDetail> {
        let scope = remote::current_scope();
        self.run(move |s| {
            s.create_lane(
                NewLane {
                    agent: a.agent,
                    name: a.name,
                    project: a.project,
                    lead: a.lead,
                    charter: a.charter,
                    status: a.status,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(description = "List lanes (open ones by default), newest activity first.")]
    async fn lane_list(&self, Parameters(a): Parameters<LaneListArgs>) -> ToolResult<LanesOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(LanesOut {
                lanes: s.lanes(
                    LaneFilter {
                        status: a.status,
                        project: a.project,
                        lead: a.lead,
                        include_closed: a.include_closed,
                    },
                    scope.as_ref(),
                )?,
            })
        })
        .await
    }

    #[tool(description = "Show one lane: charter, status, lead, links and the latest reports.")]
    async fn lane_show(&self, Parameters(a): Parameters<LaneShowArgs>) -> ToolResult<LaneDetail> {
        let scope = remote::current_scope();
        self.run(move |s| s.lane(&a.lane, scope.as_ref())).await
    }

    #[tool(
        description = "Update a lane's status, charter or lead, and link threads, requests, \
        holds or messages to it."
    )]
    async fn lane_update(
        &self,
        Parameters(a): Parameters<LaneUpdateArgs>,
    ) -> ToolResult<LaneDetail> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let mut links = Vec::new();
            links.extend(
                a.link_threads
                    .into_iter()
                    .map(|i| (LaneLinkKind::Thread, i)),
            );
            links.extend(
                a.link_requests
                    .into_iter()
                    .map(|i| (LaneLinkKind::Request, i)),
            );
            links.extend(a.link_holds.into_iter().map(|i| (LaneLinkKind::Hold, i)));
            links.extend(
                a.link_messages
                    .into_iter()
                    .map(|i| (LaneLinkKind::Message, i)),
            );
            s.update_lane(
                LaneUpdate {
                    agent: a.agent,
                    lane: a.lane,
                    status: a.status,
                    charter: a.charter,
                    lead: a.lead,
                    links,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(
        description = "Post a structured lane report (summary, progress, status, links, \
        artifacts). kind=done closes the lane with an outcome and optional PR link. The report \
        is also mailed to the lane's counterpart (creator or lead) in the lane thread."
    )]
    async fn lane_report(
        &self,
        Parameters(a): Parameters<LaneReportArgs>,
    ) -> ToolResult<LaneReport> {
        let scope = remote::current_scope();
        self.run(move |s| {
            s.report_lane(
                LaneReportInput {
                    agent: a.agent,
                    lane: a.lane,
                    kind: match a.kind {
                        ReportKindArg::Status => ReportKind::Status,
                        ReportKindArg::Done => ReportKind::Done,
                    },
                    summary: a.summary,
                    progress: a.progress,
                    status: a.status,
                    links: a.links,
                    artifacts: a.artifacts,
                    outcome: a.outcome,
                    pr_url: a.pr_url,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(description = "Live path holds grouped by lane: who holds what for which lane.")]
    async fn holds_by_lane(
        &self,
        Parameters(a): Parameters<HoldsByLaneArgs>,
    ) -> ToolResult<HoldsByLaneOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(HoldsByLaneOut {
                lanes: s.holds_by_lane(
                    another_core::HoldFilter {
                        project: a.project,
                        agent: a.agent,
                    },
                    scope.as_ref(),
                )?,
            })
        })
        .await
    }

    #[tool(description = "Close a lane as done (default) or abandoned, without a report.")]
    async fn lane_close(&self, Parameters(a): Parameters<LaneCloseArgs>) -> ToolResult<LaneDetail> {
        let scope = remote::current_scope();
        self.run(move |s| s.close_lane(&a.agent, &a.lane, a.abandon, a.outcome, scope.as_ref()))
            .await
    }
}
