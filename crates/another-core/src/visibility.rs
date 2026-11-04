//! Which events a token scope may see (docs/design/02-push.md §1). When an event can be
//! attributed to projects, the scope's globs alone decide: one of its projects must match.
//! Events with no project are visible only when they concern the scope's own agent. Token,
//! peer and node events are never visible to scoped callers.

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::error::Result;
use crate::tokens::{scope_allows_project, TokenScope};

fn push(out: &mut Vec<String>, v: Option<String>) {
    if let Some(v) = v {
        if !out.contains(&v) {
            out.push(v);
        }
    }
}

fn one(conn: &Connection, sql: &str, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(sql, [key], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten())
}

fn agent_project(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    one(
        conn,
        "SELECT project_id FROM agents WHERE agent_id = ?1",
        agent_id,
    )
}

/// Agent ids of the sender and recipients of a message.
fn message_agents(conn: &Connection, message_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT sender_id FROM messages WHERE message_id = ?1
         UNION SELECT agent_id FROM deliveries WHERE message_id = ?1",
    )?;
    let rows = stmt.query_map([message_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn strs(p: &Value, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn s(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Agents an event is about (actor aside): payload ids, message parties.
fn event_agents(conn: &Connection, kind: &str, p: &Value) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for key in ["agent_id", "sender_id", "owner_id", "lead_id", "asker_id"] {
        push(&mut out, s(p, key));
    }
    if let Some(rs) = p.get("recipients").and_then(Value::as_array) {
        for r in rs {
            push(
                &mut out,
                r.get("agent_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            );
        }
    }
    match kind.split('.').next().unwrap_or_default() {
        "message" | "delivery" => {
            if let Some(m) = s(p, "message_id") {
                for a in message_agents(conn, &m)? {
                    push(&mut out, Some(a));
                }
            }
        }
        "request" => {
            if let Some(r) = s(p, "request_id") {
                if let Some(m) = one(
                    conn,
                    "SELECT message_id FROM requests WHERE request_id = ?1",
                    &r,
                )? {
                    for a in message_agents(conn, &m)? {
                        push(&mut out, Some(a));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// The projects an event touches.
pub(crate) fn event_projects(conn: &Connection, kind: &str, p: &Value) -> Result<Vec<String>> {
    let mut out = Vec::new();
    push(&mut out, s(p, "project_id"));
    match kind.split('.').next().unwrap_or_default() {
        "hold" => {
            let mut ids = strs(p, "hold_ids");
            ids.extend(s(p, "hold_id"));
            for id in ids {
                push(
                    &mut out,
                    one(conn, "SELECT project_id FROM holds WHERE hold_id = ?1", &id)?,
                );
            }
        }
        "resource" => {
            if let Some(r) = s(p, "resource_id") {
                push(
                    &mut out,
                    one(
                        conn,
                        "SELECT project_id FROM resources WHERE resource_id = ?1",
                        &r,
                    )?,
                );
            }
            let mut leases = strs(p, "lease_ids");
            leases.extend(s(p, "lease_id"));
            for l in leases {
                push(
                    &mut out,
                    one(
                        conn,
                        "SELECT r.project_id FROM resource_leases l
                         JOIN resources r ON r.resource_id = l.resource_id WHERE l.lease_id = ?1",
                        &l,
                    )?,
                );
            }
        }
        _ => {}
    }
    // An object's own project decides (a lane in B led by an agent of A is B's event).
    // Only events about no project-bound object (mail, sessions, agents) fall back to the
    // projects of the agents they concern.
    if out.is_empty() {
        for agent in event_agents(conn, kind, p)? {
            push(&mut out, agent_project(conn, &agent)?);
        }
    }
    Ok(out)
}

/// Whether `scope` may see an event.
pub(crate) fn event_visible(
    conn: &Connection,
    scope: &TokenScope,
    event_node: &str,
    actor: &str,
    kind: &str,
    payload: &str,
) -> Result<bool> {
    if matches!(
        kind.split('.').next(),
        Some("token" | "peer" | "node" | "invite" | "summary")
    ) {
        return Ok(false);
    }
    let p: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let projects = event_projects(conn, kind, &p)?;
    if projects.is_empty() {
        // Project-less events (mail between project-less agents, their sessions): only the
        // token's own, matched by agent id. Names are not unique across nodes (another
        // node's `manager` is not this token's `manager`), so the actor counts only when its
        // address resolves, on the event's own node, to the token's agent.
        if event_agents(conn, kind, &p)?.contains(&scope.agent_id) {
            return Ok(true);
        }
        let actor_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM agents WHERE address = ?1 AND node_id = ?2",
                [actor, event_node],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(actor_id.as_deref() == Some(scope.agent_id.as_str()));
    }
    // Project scope wins over agent identity: acting in another project through another
    // channel does not make those events visible to this token.
    for project in projects {
        if scope_allows_project(conn, scope, &project)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether an agent is visible to a scope (it is the scope's agent, or its project is).
pub(crate) fn agent_visible(conn: &Connection, scope: &TokenScope, agent_id: &str) -> Result<bool> {
    if agent_id == scope.agent_id {
        return Ok(true);
    }
    match agent_project(conn, agent_id)? {
        Some(p) => scope_allows_project(conn, scope, &p),
        None => Ok(false),
    }
}

/// Whether a request is visible to a scope (through its message's parties).
pub(crate) fn request_visible(
    conn: &Connection,
    scope: &TokenScope,
    request_id: &str,
) -> Result<bool> {
    // A decision's request belongs to the decision's project, whoever asked it.
    let decision_project: Option<String> = conn
        .query_row(
            "SELECT project_id FROM decisions WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(project) = decision_project {
        return scope_allows_project(conn, scope, &project);
    }
    // The same rule as its events (`event_visible`): the request's projects decide, and the
    // own-agent exception applies only when it has no project at all.
    let p = serde_json::json!({ "request_id": request_id });
    let projects = event_projects(conn, "request.created", &p)?;
    if projects.is_empty() {
        return Ok(event_agents(conn, "request.created", &p)?.contains(&scope.agent_id));
    }
    for project in projects {
        if scope_allows_project(conn, scope, &project)? {
            return Ok(true);
        }
    }
    Ok(false)
}
