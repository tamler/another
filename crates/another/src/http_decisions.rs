//! HTTP endpoints for decisions (docs/design/03-decisions.md), for a phone shortcut or a bot
//! without MCP: `GET /decisions`, `GET /decisions/{id}`, `POST /decisions/{id}/answer`. Same
//! scope rules as the MCP tools; every remote call is audited.

use another_core::{AnswerDecision, AuditRecord, DecisionFilter, DecisionStatus, Store};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::remote::{audit, RemotePeer};

fn status_of(e: &another_core::Error) -> StatusCode {
    match e.code() {
        "not_found" => StatusCode::NOT_FOUND,
        "forbidden" => StatusCode::FORBIDDEN,
        "conflict" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}

pub(crate) async fn finish(
    store: &Store,
    peer: Option<RemotePeer>,
    op: String,
    detail: String,
    result: Result<another_core::Result<Value>, tokio::task::JoinError>,
) -> Response {
    let (status, body, outcome) = match result {
        Ok(Ok(v)) => (StatusCode::OK, v, "ok"),
        Ok(Err(e)) => (status_of(&e), json!({ "error": e.body() }), "error"),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": { "code": "internal", "message": e.to_string() } }),
            "error",
        ),
    };
    if let Some(peer) = peer {
        audit(
            store,
            AuditRecord {
                token_id: Some(peer.scope.token_id.clone()),
                agent: Some(peer.scope.agent.clone()),
                operation: op,
                outcome: outcome.into(),
                detail: Some(detail),
                remote: peer.remote.clone(),
            },
        )
        .await;
    }
    (status, axum::Json(body)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub status: Option<String>,
    pub all: Option<bool>,
    pub project: Option<String>,
    pub lane: Option<String>,
    pub limit: Option<u32>,
}

/// `GET /decisions?status=open&project=&lane=&all=true`
pub async fn list(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Query(p): Query<ListParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let detail = format!(
        "status={:?} project={:?} lane={:?}",
        p.status, p.project, p.lane
    );
    let s = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let status = p
            .status
            .as_deref()
            .map(str::parse::<DecisionStatus>)
            .transpose()?;
        let list = s.decisions(
            DecisionFilter {
                status,
                all: p.all.unwrap_or(false),
                project: p.project,
                lane: p.lane,
                human: None,
                limit: p.limit.unwrap_or(50),
            },
            scope.as_ref(),
        )?;
        Ok(json!({ "decisions": list }))
    })
    .await;
    finish(&store, peer, "GET /decisions".into(), detail, result).await
}

/// `GET /decisions/{id}` — the decision and its phone-readable markdown.
pub async fn show(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Path(id): Path<String>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let s = store.clone();
    let key = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let d = s.decision(&key, scope.as_ref())?;
        Ok(json!({ "markdown": another_core::decision_markdown(&d), "decision": d }))
    })
    .await;
    finish(
        &store,
        peer,
        "GET /decisions/{id}".into(),
        format!("id={id}"),
        result,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct AnswerBody {
    pub value: Option<String>,
    pub text: Option<String>,
    pub answered_by: Option<String>,
}

/// `POST /decisions/{id}/answer` with `{"value": "ship", "text": "...", "answered_by": "owner"}`.
pub async fn answer(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<AnswerBody>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let detail = format!(
        "id={id} value={:?} text={} answered_by={:?}",
        body.value,
        body.text
            .as_ref()
            .map_or("none".into(), |t| format!("<{} bytes>", t.len())),
        body.answered_by
    );
    let s = store.clone();
    let key = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let d = s.answer_decision(
            AnswerDecision {
                decision: key,
                value: body.value,
                text: body.text,
                answered_by: body.answered_by,
                // Remote: the token's agent relayed it. Local: no relaying agent.
                relayed_by: scope.as_ref().map(|sc| sc.agent.clone()),
            },
            scope.as_ref(),
        )?;
        crate::wake::wake_if_targeted(&s, &d.asker);
        Ok(json!(d))
    })
    .await;
    finish(
        &store,
        peer,
        "POST /decisions/{id}/answer".into(),
        detail,
        result,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct SpendParams {
    pub period: Option<String>,
    pub account: Option<String>,
    pub lane: Option<String>,
}

/// `GET /spend?period=current` — the spend report (scoped for remote callers).
pub async fn spend(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Query(p): Query<SpendParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let detail = format!(
        "period={:?} account={:?} lane={:?}",
        p.period, p.account, p.lane
    );
    let s = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = s.spend_report(
            another_core::SpendQuery {
                period: p.period.as_deref().unwrap_or("current").parse()?,
                account: p.account,
                lane: p.lane,
                agent: None,
            },
            scope.as_ref(),
        )?;
        Ok(json!(r))
    })
    .await;
    finish(&store, peer, "GET /spend".into(), detail, result).await
}

#[derive(Debug, Deserialize)]
pub struct SummaryParams {
    pub since: Option<String>,
    /// `json` (default) or `markdown`.
    pub format: Option<String>,
}

/// `GET /summary?since=24h[&format=markdown]` — the daily summary (scoped for remote callers).
pub async fn summary(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    headers: axum::http::HeaderMap,
    Query(p): Query<SummaryParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let markdown = p.format.as_deref() == Some("markdown")
        || headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/markdown"));
    let detail = format!(
        "since={:?} format={}",
        p.since,
        if markdown { "markdown" } else { "json" }
    );
    let s = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let since = p
            .since
            .as_deref()
            .map(|v| another_core::Since::parse(v, s.now()))
            .transpose()?;
        Ok(json!(s.daily_summary(since, scope.as_ref())?))
    })
    .await;
    if markdown {
        if let Ok(Ok(v)) = &result {
            let text = v["markdown"].as_str().unwrap_or_default().to_string();
            if let Some(peer) = &peer {
                audit(
                    &store,
                    AuditRecord {
                        token_id: Some(peer.scope.token_id.clone()),
                        agent: Some(peer.scope.agent.clone()),
                        operation: "GET /summary".into(),
                        outcome: "ok".into(),
                        detail: Some(detail),
                        remote: peer.remote.clone(),
                    },
                )
                .await;
            }
            return (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/markdown; charset=utf-8",
                )],
                text,
            )
                .into_response();
        }
    }
    finish(&store, peer, "GET /summary".into(), detail, result).await
}
