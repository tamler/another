//! MCP tools for push (docs/design/02-push.md): `events_poll` long-poll and webhook
//! subscriptions. Remote callers only see events and webhooks within their token scope.

use std::time::Duration;

use another_core::{
    ActivityFeed, ActivityKind, NewWebhook, Webhook, WebhookCreated, WebhookFilter,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::{AnotherMcp, ToolResult};
use crate::push::{long_poll, MAX_MCP_POLL_SECONDS};
use crate::remote;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EventsPollArgs {
    /// Cursor (`next_since` from a previous call), a duration like `15m`, or an RFC 3339
    /// time. Omit to wait for events from now on.
    pub since: Option<String>,
    /// Only these categories (empty = all).
    #[serde(default)]
    pub kinds: Vec<ActivityKind>,
    /// Seconds to wait for at least one event (default 25, at most 30).
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebhookSubscribeArgs {
    /// `https://` URL (plain http only to loopback). Redirects are not followed.
    pub url: String,
    /// Event filters: lane_status, lane_report, needs_decision, request_created,
    /// request_answered, hold_conflict, lease_expiring, all.
    pub filters: Vec<WebhookFilter>,
    /// Project path or slug globs. Remote callers default to (and may only narrow) their
    /// token's globs.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Every project (local callers only).
    #[serde(default)]
    pub all_projects: bool,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebhookRemoveArgs {
    pub webhook_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebhookListArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DailySummaryArgs {
    /// Duration like `24h` (default), an RFC 3339 time, or an event id.
    pub since: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WebhooksOut {
    pub webhooks: Vec<Webhook>,
}

#[tool_router(router = push_router, vis = "pub(crate)")]
impl AnotherMcp {
    #[tool(
        description = "Wait for new activity (long-poll): returns as soon as at least one \
        event is available, or after timeout_seconds (at most 30). Pass next_since back as \
        since. Items are one-line summaries with ids, filtered to what your token may see."
    )]
    async fn events_poll(
        &self,
        Parameters(a): Parameters<EventsPollArgs>,
    ) -> ToolResult<ActivityFeed> {
        let scope = remote::current_scope();
        let timeout =
            Duration::from_secs(a.timeout_seconds.unwrap_or(25).min(MAX_MCP_POLL_SECONDS));
        long_poll(self.store.clone(), scope, a.since, a.kinds, timeout)
            .await
            .map(Json)
            .map_err(|e| serde_json::json!({ "error": e.body() }).to_string())
    }

    #[tool(
        description = "Subscribe a URL to events (HMAC-SHA256 signed POSTs with retries). \
        Returns the signing secret once. Filters: lane_status, lane_report, needs_decision, \
        request_created, request_answered, hold_conflict, lease_expiring, all."
    )]
    async fn webhook_subscribe(
        &self,
        Parameters(a): Parameters<WebhookSubscribeArgs>,
    ) -> ToolResult<WebhookCreated> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let globs = match (a.all_projects, a.projects.is_empty(), &scope) {
                (true, _, Some(_)) => {
                    return Err(another_core::Error::Forbidden(
                        "remote peers cannot subscribe to every project".into(),
                    ))
                }
                (true, true, None) => None,
                (true, false, None) => {
                    return Err(another_core::Error::Invalid(
                        "give projects or all_projects, not both".into(),
                    ))
                }
                (false, true, None) => {
                    return Err(another_core::Error::Invalid(
                        "give project globs (projects) or all_projects=true".into(),
                    ))
                }
                (false, true, Some(_)) => None,
                (false, false, _) => Some(a.projects),
            };
            s.create_webhook(
                NewWebhook {
                    url: a.url,
                    filters: a.filters,
                    project_globs: globs,
                    description: a.description,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(
        description = "List webhook subscriptions (remote callers see their token's only). \
        Secrets are never shown."
    )]
    async fn webhook_list(
        &self,
        Parameters(_): Parameters<WebhookListArgs>,
    ) -> ToolResult<WebhooksOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(WebhooksOut {
                webhooks: s.webhooks(scope.as_ref())?,
            })
        })
        .await
    }

    #[tool(
        description = "Daily summary (default: last 24h): what needs a person (open \
        decisions, stuck lanes, budgets, approvals, stale requests, dead webhooks), lanes moved \
        and closed, decisions, requests, holds and conflicts, spend, active agents. JSON plus a \
        short phone-readable `markdown`."
    )]
    async fn daily_summary(
        &self,
        Parameters(a): Parameters<DailySummaryArgs>,
    ) -> ToolResult<another_core::DailySummary> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let since = a
                .since
                .as_deref()
                .map(|v| another_core::Since::parse(v, s.now()))
                .transpose()?;
            s.daily_summary(since, scope.as_ref())
        })
        .await
    }

    #[tool(description = "Remove a webhook subscription; pending deliveries are dropped.")]
    async fn webhook_remove(
        &self,
        Parameters(a): Parameters<WebhookRemoveArgs>,
    ) -> ToolResult<Webhook> {
        let scope = remote::current_scope();
        self.run(move |s| s.remove_webhook(&a.webhook_id, scope.as_ref()))
            .await
    }
}
