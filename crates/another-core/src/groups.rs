//! Groups: named sets of local agents in one project, the address of a work queue.
//!
//! A request sent to `group:<name>[@project]` is a queue item: no member gets mail for it,
//! and members pull items with `request next` (see `requests.rs`). Groups, their members
//! and queue items are node-local and never federate. Membership gates pulling and claiming
//! items; among local agents, who share one database, it is not a security boundary.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{find_project, get_agent, resolve_agent, AgentRef};
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;

/// Prefix that marks a group address in a recipient list.
pub const GROUP_PREFIX: &str = "group:";
const MAX_GROUP_NAME: usize = 40;

/// How urgent a request is. Stored as 0 (urgent) .. 3 (low); `normal` is the default.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Urgent,
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::High => "high",
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }

    pub(crate) fn rank(self) -> i64 {
        match self {
            Priority::Urgent => 0,
            Priority::High => 1,
            Priority::Normal => 2,
            Priority::Low => 3,
        }
    }

    pub(crate) fn from_rank(rank: i64) -> Priority {
        match rank {
            0 => Priority::Urgent,
            1 => Priority::High,
            3 => Priority::Low,
            _ => Priority::Normal,
        }
    }
}

impl std::str::FromStr for Priority {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "urgent" => Ok(Priority::Urgent),
            "high" => Ok(Priority::High),
            "normal" => Ok(Priority::Normal),
            "low" => Ok(Priority::Low),
            _ => Err(Error::Invalid(format!(
                "unknown priority {s:?}; expected urgent, high, normal or low"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueCounts {
    pub open: usize,
    pub claimed: usize,
    pub stale: usize,
    /// Open items per priority: urgent, high, normal, low.
    pub open_by_priority: [usize; 4],
    pub oldest_open_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Group {
    pub group_id: String,
    pub name: String,
    pub project: String,
    /// `name@project`; send to it as `group:name@project`.
    pub address: String,
    pub members: Vec<String>,
    pub created_by: String,
    pub created_at: String,
    pub queue: QueueCounts,
}

/// A live group row: (group_id, name, project slug, project_id).
#[derive(Debug, Clone)]
pub(crate) struct GroupRef {
    pub id: String,
    pub name: String,
    pub slug: String,
}

impl GroupRef {
    pub fn address(&self) -> String {
        format!("{}@{}", self.name, self.slug)
    }
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_GROUP_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid group name {name:?}: use 1-{MAX_GROUP_NAME} characters from a-z, 0-9, '-' and '_'"
        )))
    }
}

/// The project id an agent belongs to, if any.
fn agent_project(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project_id FROM agents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Resolve `name` or `name@project` (an optional `group:` prefix is accepted) to a live
/// group of this node. A bare name is looked up in `default_project` (the acting agent's
/// project when called for a send).
pub(crate) fn resolve_group(
    conn: &Connection,
    node_id: &str,
    spec: &str,
    default_project: Option<&str>,
) -> Result<GroupRef> {
    let spec = spec.strip_prefix(GROUP_PREFIX).unwrap_or(spec);
    let (name, project_id) = match spec.split_once('@') {
        Some((name, project)) => (name, find_project(conn, node_id, project)?.project_id),
        None => match default_project {
            Some(p) => (spec, p.to_string()),
            None => {
                return Err(Error::Invalid(format!(
                    "group {spec:?} needs a project: use group:{spec}@<project>"
                )))
            }
        },
    };
    conn.query_row(
        "SELECT g.group_id, g.name, p.slug FROM groups g JOIN projects p ON p.project_id = g.project_id
         WHERE g.node_id = ?1 AND g.project_id = ?2 AND g.name = ?3 AND g.deleted_at IS NULL",
        params![node_id, project_id, name],
        |r| {
            Ok(GroupRef {
                id: r.get(0)?,
                name: r.get(1)?,
                slug: r.get(2)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("unknown group {spec:?}")))
}

/// Resolve a group for an acting agent: bare names are looked up in the agent's project.
pub(crate) fn resolve_group_for(
    conn: &Connection,
    node_id: &str,
    agent: &AgentRef,
    spec: &str,
) -> Result<GroupRef> {
    let project = agent_project(conn, &agent.id)?;
    resolve_group(conn, node_id, spec, project.as_deref())
}

pub(crate) fn is_member(conn: &Connection, group_id: &str, agent_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM group_members WHERE group_id = ?1 AND agent_id = ?2",
            params![group_id, agent_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn add_member_tx(
    tx: &Tx<'_>,
    actor: &AgentRef,
    group: &GroupRef,
    member: &AgentRef,
) -> Result<bool> {
    if is_member(tx, &group.id, &member.id)? {
        return Ok(false);
    }
    let event_id = tx.emit(
        &actor.address,
        "group.member_added",
        json!({ "group_id": group.id, "agent_id": member.id, "by": actor.id }),
    )?;
    tx.execute(
        "INSERT INTO group_members (group_id, agent_id, joined_at, event_id) VALUES (?1, ?2, ?3, ?4)",
        params![group.id, member.id, tx.now, event_id],
    )?;
    Ok(true)
}

fn remove_member_tx(
    tx: &Tx<'_>,
    actor: &AgentRef,
    group: &GroupRef,
    member: &AgentRef,
) -> Result<bool> {
    if !is_member(tx, &group.id, &member.id)? {
        return Ok(false);
    }
    tx.emit(
        &actor.address,
        "group.member_removed",
        json!({ "group_id": group.id, "agent_id": member.id, "by": actor.id }),
    )?;
    tx.execute(
        "DELETE FROM group_members WHERE group_id = ?1 AND agent_id = ?2",
        params![group.id, member.id],
    )?;
    Ok(true)
}

fn queue_counts(conn: &Connection, group_id: &str) -> Result<QueueCounts> {
    let mut counts = QueueCounts {
        open: 0,
        claimed: 0,
        stale: 0,
        open_by_priority: [0; 4],
        oldest_open_at: None,
    };
    let mut stmt = conn.prepare(
        "SELECT status, priority, count(*), min(created_at) FROM requests
         WHERE group_id = ?1 AND status IN ('open', 'claimed', 'stale') GROUP BY status, priority",
    )?;
    let rows = stmt.query_map([group_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)? as usize,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut oldest: Option<i64> = None;
    for row in rows {
        let (status, priority, n, min_created) = row?;
        match status.as_str() {
            "open" => {
                counts.open += n;
                counts.open_by_priority[Priority::from_rank(priority).rank() as usize] += n;
                oldest = Some(oldest.map_or(min_created, |o| o.min(min_created)));
            }
            "claimed" => counts.claimed += n,
            _ => counts.stale += n,
        }
    }
    counts.oldest_open_at = oldest.map(fmt_ts);
    Ok(counts)
}

fn group_from_ref(conn: &Connection, g: &GroupRef) -> Result<Group> {
    let (created_by, created_at): (String, i64) = conn.query_row(
        "SELECT created_by, created_at FROM groups WHERE group_id = ?1",
        [&g.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut stmt = conn.prepare(
        "SELECT agent_id FROM group_members WHERE group_id = ?1 ORDER BY joined_at, agent_id",
    )?;
    let ids = stmt
        .query_map([&g.id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let members = ids
        .iter()
        .map(|id| get_agent(conn, id).map(|a| a.address))
        .collect::<Result<Vec<_>>>()?;
    Ok(Group {
        group_id: g.id.clone(),
        name: g.name.clone(),
        project: g.slug.clone(),
        address: g.address(),
        members,
        created_by: get_agent(conn, &created_by)?.address,
        created_at: fmt_ts(created_at),
        queue: queue_counts(conn, &g.id)?,
    })
}

/// Groups an agent belongs to, with their queue counts.
pub(crate) fn groups_of(conn: &Connection, agent_id: &str) -> Result<Vec<Group>> {
    let mut stmt = conn.prepare(
        "SELECT g.group_id, g.name, p.slug FROM group_members m
         JOIN groups g ON g.group_id = m.group_id JOIN projects p ON p.project_id = g.project_id
         WHERE m.agent_id = ?1 AND g.deleted_at IS NULL ORDER BY p.slug, g.name",
    )?;
    let refs = stmt
        .query_map([agent_id], |r| {
            Ok(GroupRef {
                id: r.get(0)?,
                name: r.get(1)?,
                slug: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    refs.iter().map(|g| group_from_ref(conn, g)).collect()
}

/// A thread that holds a queue item. Remote callers neither read it, reply in it nor act on
/// its mail (group queues are local-only in this release, D77), whatever else the thread
/// contains: the claimer and any direct recipient of a mixed send are participants.
pub(crate) fn is_queue_thread(conn: &Connection, thread_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM requests WHERE thread_id = ?1 AND group_id IS NOT NULL)",
        [thread_id],
        |r| r.get(0),
    )?)
}

/// Whether `message_id` belongs to a queue thread. An unknown message is not.
pub(crate) fn is_queue_message(conn: &Connection, message_id: &str) -> Result<bool> {
    let thread: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM messages WHERE message_id = ?1",
            [message_id],
            |r| r.get(0),
        )
        .optional()?;
    match thread {
        Some(t) => is_queue_thread(conn, &t),
        None => Ok(false),
    }
}

impl Store {
    /// Whether `thread_id` holds a group queue item; see the remote rules in QUEUES.md.
    pub fn is_queue_thread(&self, thread_id: &str) -> Result<bool> {
        self.read(|conn| is_queue_thread(conn, thread_id))
    }

    /// Whether `message_id` is in a thread that holds a group queue item.
    pub fn is_queue_message(&self, message_id: &str) -> Result<bool> {
        self.read(|conn| is_queue_message(conn, message_id))
    }

    /// Create a group in `project` (path or slug); the creator becomes its first member.
    pub fn create_group(&self, agent: &str, project: &str, name: &str) -> Result<Group> {
        validate_name(name)?;
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let exists = tx
                .query_row(
                    "SELECT 1 FROM groups WHERE project_id = ?1 AND name = ?2 AND deleted_at IS NULL",
                    params![project.project_id, name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                return Err(Error::Conflict(format!(
                    "group {name}@{} already exists",
                    project.slug
                )));
            }
            let group_id = new_id();
            let event_id = tx.emit(
                &me.address,
                "group.created",
                json!({ "group_id": group_id, "project_id": project.project_id, "name": name }),
            )?;
            tx.execute(
                "INSERT INTO groups (group_id, node_id, project_id, name, created_by, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![group_id, tx.node_id, project.project_id, name, me.id, tx.now, event_id],
            )?;
            let g = GroupRef {
                id: group_id,
                name: name.to_string(),
                slug: project.slug.clone(),
            };
            add_member_tx(tx, &me, &g, &me)?;
            group_from_ref(tx, &g)
        })
    }

    /// Add `member` (default: the acting agent itself) to a group.
    pub fn join_group(&self, agent: &str, group: &str, member: Option<&str>) -> Result<Group> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let g = resolve_group_for(tx, tx.node_id, &me, group)?;
            let who = match member {
                Some(m) => resolve_agent(tx, tx.node_id, m)?,
                None => me.clone(),
            };
            add_member_tx(tx, &me, &g, &who)?;
            group_from_ref(tx, &g)
        })
    }

    /// Remove `member` (default: the acting agent itself) from a group. Items it owns stay
    /// its own; it can no longer pull or claim new ones from this group.
    pub fn leave_group(&self, agent: &str, group: &str, member: Option<&str>) -> Result<Group> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let g = resolve_group_for(tx, tx.node_id, &me, group)?;
            let who = match member {
                Some(m) => resolve_agent(tx, tx.node_id, m)?,
                None => me.clone(),
            };
            if !remove_member_tx(tx, &me, &g, &who)? {
                return Err(Error::NotFound(format!(
                    "{} is not a member of {}",
                    who.address,
                    g.address()
                )));
            }
            group_from_ref(tx, &g)
        })
    }

    /// Delete a group with no open, claimed or stale items.
    pub fn delete_group(&self, agent: &str, group: &str) -> Result<()> {
        self.write(|tx| {
            crate::requests::stale_overdue_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let g = resolve_group_for(tx, tx.node_id, &me, group)?;
            let live: i64 = tx.query_row(
                "SELECT count(*) FROM requests WHERE group_id = ?1 AND status IN ('open', 'claimed', 'stale')",
                [&g.id],
                |r| r.get(0),
            )?;
            if live > 0 {
                return Err(Error::Conflict(format!(
                    "group {} has {live} open, claimed or stale item(s); complete, cancel or release them first",
                    g.address()
                )));
            }
            tx.emit(&me.address, "group.deleted", json!({ "group_id": g.id }))?;
            tx.execute(
                "UPDATE groups SET deleted_at = ?2 WHERE group_id = ?1",
                params![g.id, tx.now],
            )?;
            tx.execute("DELETE FROM group_members WHERE group_id = ?1", [&g.id])?;
            Ok(())
        })
    }

    /// Live groups of this node, optionally of one project (path or slug).
    pub fn groups(&self, project: Option<&str>) -> Result<Vec<Group>> {
        self.read(|conn| {
            let project_id = match project {
                Some(p) => Some(find_project(conn, self.node_id(), p)?.project_id),
                None => None,
            };
            let mut stmt = conn.prepare(
                "SELECT g.group_id, g.name, p.slug FROM groups g JOIN projects p ON p.project_id = g.project_id
                 WHERE g.node_id = ?1 AND g.deleted_at IS NULL AND (?2 IS NULL OR g.project_id = ?2)
                 ORDER BY p.slug, g.name",
            )?;
            let refs = stmt
                .query_map(params![self.node_id(), project_id], |r| {
                    Ok(GroupRef {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        slug: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            refs.iter().map(|g| group_from_ref(conn, g)).collect()
        })
    }

    /// Groups an agent belongs to, with queue counts (for inbox and digest).
    pub fn agent_groups(&self, agent: &str) -> Result<Vec<Group>> {
        self.read(|conn| {
            let me = resolve_agent(conn, self.node_id(), agent)?;
            groups_of(conn, &me.id)
        })
    }
}
