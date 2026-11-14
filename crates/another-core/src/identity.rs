//! Projects and agent identities.
//!
//! Agent names are unique per project (plus one project-less registration per name) and
//! addressed as `name@project`. A bare name resolves when exactly one agent has it;
//! otherwise resolution fails as ambiguous and the caller must use `name@project`.

use std::path::{Component, Path};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};

/// The coding-agent program behind an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Program {
    Claude,
    Grok,
    Codex,
    Cursor,
    GrokBot,
    Other,
}

impl Program {
    pub const ALL: [Program; 6] = [
        Program::Claude,
        Program::Grok,
        Program::Codex,
        Program::Cursor,
        Program::GrokBot,
        Program::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Program::Claude => "claude",
            Program::Grok => "grok",
            Program::Codex => "codex",
            Program::Cursor => "cursor",
            Program::GrokBot => "grok_bot",
            Program::Other => "other",
        }
    }
}

impl std::str::FromStr for Program {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Program::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown program {s:?}; expected one of claude, grok, codex, cursor, grok_bot, other"
                ))
            })
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Project {
    pub project_id: String,
    pub node_id: String,
    pub abs_path: String,
    pub slug: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Agent {
    pub agent_id: String,
    pub node_id: String,
    pub name: String,
    /// Address other agents use: `name` or `name@project` locally; `name@project@node`
    /// (or `name@@node`) for agents on other nodes.
    pub address: String,
    /// Home node display name, for agents on other nodes.
    pub node: Option<String>,
    pub project: Option<String>,
    pub program: String,
    pub model: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
}

/// Input for [`Store::register_agent`].
#[derive(Debug, Clone)]
pub struct RegisterAgent {
    pub name: String,
    pub program: Program,
    /// Absolute project path; the project is ensured automatically.
    pub project: Option<String>,
    pub model: Option<String>,
    pub description: Option<String>,
}

/// Internal handle on a resolved agent.
#[derive(Debug, Clone)]
pub(crate) struct AgentRef {
    pub id: String,
    pub address: String,
    /// The agent's home node.
    pub node_id: String,
}

pub(crate) const AGENT_COLUMNS: &str =
    "a.agent_id, a.node_id, a.name, p.slug, a.program, a.model, \
     a.description, a.status, a.created_at, a.last_seen_at, a.address, \
     (SELECT CASE WHEN n.is_local = 1 THEN NULL ELSE n.display_name END \
      FROM nodes n WHERE n.node_id = a.node_id)";

pub(crate) fn address(name: &str, slug: Option<&str>) -> String {
    match slug {
        Some(slug) => format!("{name}@{slug}"),
        None => name.to_string(),
    }
}

pub(crate) fn agent_from_row(r: &Row<'_>) -> rusqlite::Result<Agent> {
    let name: String = r.get(2)?;
    let slug: Option<String> = r.get(3)?;
    let stored: Option<String> = r.get(10)?;
    Ok(Agent {
        agent_id: r.get(0)?,
        node_id: r.get(1)?,
        address: stored.unwrap_or_else(|| address(&name, slug.as_deref())),
        node: r.get(11)?,
        name,
        project: slug,
        program: r.get(4)?,
        model: r.get(5)?,
        description: r.get(6)?,
        status: r.get(7)?,
        created_at: fmt_ts(r.get(8)?),
        last_seen_at: fmt_opt(r.get(9)?),
    })
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid agent name {name:?}: use 1-64 ASCII letters, digits, '-', '_' or '.'"
        )))
    }
}

/// Normalize an absolute project path: no trailing slash, no `.`/`..` components.
pub(crate) fn normalize_project_path(raw: &str) -> Result<String> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(Error::Invalid(format!(
            "project path must be absolute, got {raw:?}"
        )));
    }
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let prefix = match path.components().next() {
        Some(Component::Prefix(p)) => format!("{}\\", p.as_os_str().to_string_lossy()),
        _ => "/".to_string(),
    };
    Ok(format!("{prefix}{}", parts.join("/")))
}

fn slug_base(abs_path: &str) -> String {
    let last = abs_path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("root");
    let slug: String = last
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug.to_string()
    }
}

fn project_from_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        project_id: r.get(0)?,
        node_id: r.get(1)?,
        abs_path: r.get(2)?,
        slug: r.get(3)?,
        created_at: fmt_ts(r.get(4)?),
    })
}

const PROJECT_COLUMNS: &str = "project_id, node_id, abs_path, slug, created_at";

pub(crate) fn ensure_project_tx(tx: &Tx<'_>, actor: &str, raw_path: &str) -> Result<Project> {
    let abs_path = normalize_project_path(raw_path)?;
    let existing = tx
        .query_row(
            &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE node_id = ?1 AND abs_path = ?2"),
            params![tx.node_id, abs_path],
            project_from_row,
        )
        .optional()?;
    if let Some(p) = existing {
        crate::repo::refresh_link(tx, &p)?;
        return Ok(p);
    }
    let base = slug_base(&abs_path);
    let mut slug = base.clone();
    let mut n = 2;
    while tx
        .query_row(
            "SELECT 1 FROM projects WHERE node_id = ?1 AND slug = ?2",
            params![tx.node_id, slug],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        slug = format!("{base}-{n}");
        n += 1;
    }
    let project_id = new_id();
    let event_id = tx.emit(
        actor,
        "project.ensured",
        json!({ "project_id": project_id, "abs_path": abs_path, "slug": slug }),
    )?;
    tx.execute(
        "INSERT INTO projects (project_id, node_id, abs_path, slug, created_at, event_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![project_id, tx.node_id, abs_path, slug, tx.now, event_id],
    )?;
    let project = Project {
        project_id,
        node_id: tx.node_id.to_string(),
        abs_path,
        slug,
        created_at: fmt_ts(tx.now),
    };
    crate::repo::refresh_link(tx, &project)?;
    Ok(project)
}

/// Find a project by absolute path or slug.
pub(crate) fn find_project(conn: &Connection, node_id: &str, key: &str) -> Result<Project> {
    let by_path = if Path::new(key).is_absolute() {
        Some(normalize_project_path(key)?)
    } else {
        None
    };
    conn.query_row(
        &format!(
            "SELECT {PROJECT_COLUMNS} FROM projects
             WHERE node_id = ?1 AND (abs_path = ?2 OR slug = ?3)"
        ),
        params![node_id, by_path, key],
        project_from_row,
    )
    .optional()?
    .ok_or_else(|| {
        Error::NotFound(format!(
            "unknown project {key:?}; call ensure_project with its absolute path first"
        ))
    })
}

/// Resolve an acting agent (must live on this node). Addresses: `name` or `name@project` on this node, or
/// `name@project@node` / `name@@node` (and `name@node` when no local project matches) for
/// agents on other nodes.
pub(crate) fn resolve_agent(conn: &Connection, node_id: &str, who: &str) -> Result<AgentRef> {
    let agent = resolve_any(conn, node_id, who)?;
    if agent.node_id != node_id {
        return Err(Error::Forbidden(format!(
            "{} belongs to another node; agents act only on their own node",
            agent.address
        )));
    }
    Ok(agent)
}

/// Resolve an address on any known node (recipients, filters). See [`resolve_agent`] for
/// identities that act.
pub(crate) fn resolve_any(conn: &Connection, node_id: &str, who: &str) -> Result<AgentRef> {
    let who = who.trim();
    let parts: Vec<&str> = who.split('@').collect();
    if parts.len() == 3 {
        return resolve_remote(conn, who, parts[0], Some(parts[1]), parts[2]);
    }
    let (name, project) = match parts.as_slice() {
        [n] => (*n, None),
        [n, p] => (*n, Some(*p)),
        _ => {
            return Err(Error::Invalid(format!(
                "invalid agent address {who:?}: use name, name@project or name@project@node"
            )))
        }
    };
    let mut stmt = conn.prepare(
        "SELECT a.agent_id, a.address, p.slug, p.abs_path FROM agents a
         LEFT JOIN projects p ON p.project_id = a.project_id
         WHERE a.node_id = ?1 AND a.name = ?2",
    )?;
    let rows = stmt
        .query_map(params![node_id, name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let candidates: Vec<_> = match project {
        Some(p) => rows
            .into_iter()
            .filter(|(_, _, slug, path)| slug.as_deref() == Some(p) || path.as_deref() == Some(p))
            .collect(),
        None => rows,
    };
    match candidates.as_slice() {
        [] => match project {
            // `name@node` for a project-less agent on another node.
            Some(node) => resolve_remote(conn, who, name, Some(""), node),
            None => Err(Error::NotFound(format!(
                "unknown agent {who:?}; register it with register_agent first"
            ))),
        },
        [(id, addr, _, _)] => Ok(AgentRef {
            id: id.clone(),
            address: addr.clone(),
            node_id: node_id.to_string(),
        }),
        many => Err(Error::Ambiguous(format!(
            "agent name {who:?} is ambiguous: it is registered in several projects; use name@project, one of: {}",
            many.iter()
                .map(|(_, a, _, _)| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn resolve_remote(
    conn: &Connection,
    who: &str,
    name: &str,
    project: Option<&str>,
    node: &str,
) -> Result<AgentRef> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT a.agent_id, a.address, a.node_id FROM agents a
             JOIN nodes n ON n.node_id = a.node_id
             LEFT JOIN projects p ON p.project_id = a.project_id
             WHERE a.name = ?1 AND (n.display_name = ?2 OR n.node_id = ?2)
               AND ((?3 = '' AND a.project_id IS NULL) OR p.slug = ?3)
             LIMIT 1",
            params![name, node, project.unwrap_or("")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        Some((id, address, node_id)) => Ok(AgentRef {
            id,
            address,
            node_id,
        }),
        None => Err(Error::NotFound(format!(
            "unknown agent {who:?}; register it with register_agent first (or sync with its node)"
        ))),
    }
}

pub(crate) fn get_agent(conn: &Connection, agent_id: &str) -> Result<Agent> {
    Ok(conn.query_row(
        &format!(
            "SELECT {AGENT_COLUMNS} FROM agents a
             LEFT JOIN projects p ON p.project_id = a.project_id WHERE a.agent_id = ?1"
        ),
        [agent_id],
        agent_from_row,
    )?)
}

impl Store {
    /// Resolve a client hook's per-session actor without storing its raw session ID.
    pub fn session_actor(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_base: Option<&str>,
        session_id: &str,
        create: bool,
    ) -> Result<(String, String)> {
        if session_id.is_empty()
            || session_id.len() > 512
            || session_id.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid session identifier".into()));
        }
        self.write(|tx| {
            let project = match find_project(tx, tx.node_id, root) {
                Ok(project) => project,
                Err(Error::NotFound(_)) if create => ensure_project_tx(tx, client, root)?,
                Err(e) => return Err(e),
            };
            let requested = requested_base.unwrap_or(client);
            let (name, slug) = match requested.split_once('@') {
                Some((name, slug)) if !slug.contains('@') => (name, Some(slug)),
                Some(_) => return Err(Error::Invalid("invalid session base address".into())),
                None => (requested, None),
            };
            if slug.is_some_and(|slug| slug != project.slug) {
                return Err(Error::Invalid("session base project does not match".into()));
            }
            validate_name(name)?;
            let existing_binding: Option<(String, String, String)> = tx.query_row(
                "SELECT b.base_name, b.client, b.digest FROM session_bindings b JOIN agents a ON a.agent_id=b.agent_id WHERE a.node_id=?1 AND a.name=?2 AND b.project_id=?3",
                params![tx.node_id, name, project.project_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).optional()?;
            let base = existing_binding.as_ref().map_or(name, |binding| binding.0.as_str());
            let mut hash = Sha256::new();
            for value in ["another-session-v1", tx.node_id, client, project.project_id.as_str(), base, session_id] {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            let digest = format!("{:x}", hash.finalize());
            let actor_name = format!("{}-s{}", &base[..base.len().min(30)], &digest[..32]);
            if let Some((_, bound_client, bound_digest)) = &existing_binding {
                if bound_client != client || bound_digest != &digest || actor_name != name {
                    return Err(Error::Invalid("session actor does not match this session".into()));
                }
            }
            let addr = address(&actor_name, Some(&project.slug));
            let binding: Option<(String, String)> = tx.query_row(
                "SELECT b.agent_id, b.digest FROM session_bindings b JOIN agents a ON a.agent_id=b.agent_id WHERE a.node_id=?1 AND a.name=?2 AND b.project_id=?3",
                params![tx.node_id, actor_name, project.project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?;
            if let Some((_, saved)) = binding {
                if saved != digest {
                    return Err(Error::Invalid("session identity collision".into()));
                }
                return Ok((addr, digest));
            }
            if !create {
                return Err(Error::NotFound("session identity has not started".into()));
            }
            let occupied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE node_id=?1 AND project_id=?2 AND name=?3)",
                params![tx.node_id, project.project_id, actor_name], |r| r.get(0),
            )?;
            if occupied {
                return Err(Error::Invalid("session identity collision".into()));
            }
            let digest_used: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_bindings WHERE digest=?1)",
                [&digest],
                |r| r.get(0),
            )?;
            if digest_used {
                return Err(Error::Invalid("session identity collision".into()));
            }
            let agent_id = new_id();
            let event_id = tx.emit(&addr, "agent.registered", json!({
                "agent_id": agent_id, "name": actor_name, "project_id": project.project_id,
                "program": program.as_str(), "model": null, "description": null, "new": true
            }))?;
            tx.execute(
                "INSERT INTO agents (agent_id,node_id,project_id,name,program,status,created_at,updated_at,last_seen_at,event_id,address) VALUES (?1,?2,?3,?4,?5,'active',?6,?6,?6,?7,?8)",
                params![agent_id, tx.node_id, project.project_id, actor_name, program.as_str(), tx.now, event_id, addr],
            )?;
            tx.execute(
                "INSERT INTO session_bindings(digest,agent_id,project_id,client,base_name) VALUES (?1,?2,?3,?4,?5)",
                params![digest, agent_id, project.project_id, client, base],
            )?;
            Ok((addr, digest))
        })
    }

    /// Check that an address identifies a locally registered session actor.
    pub fn is_session_actor(&self, who: &str) -> Result<bool> {
        self.read(|conn| {
            let actor = resolve_agent(conn, self.node_id(), who)?;
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_bindings WHERE agent_id=?1)",
                [&actor.id],
                |r| r.get(0),
            )?)
        })
    }

    /// Resolve a session actor to its complete local address.
    pub fn session_actor_address(&self, who: &str) -> Result<String> {
        self.read(|conn| {
            let actor = resolve_agent(conn, self.node_id(), who)?;
            let is_session: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_bindings WHERE agent_id=?1)",
                [&actor.id],
                |r| r.get(0),
            )?;
            if !is_session {
                return Err(Error::Invalid(
                    "agent is not a registered session actor".into(),
                ));
            }
            Ok(actor.address)
        })
    }
    /// Register an absolute project path (idempotent).
    pub fn ensure_project(&self, actor: &str, abs_path: &str) -> Result<Project> {
        self.write(|tx| ensure_project_tx(tx, actor, abs_path))
    }

    /// Look up a local project by absolute path or slug (read only).
    pub fn project(&self, key: &str) -> Result<Project> {
        self.read(|conn| find_project(conn, self.node_id(), key))
    }

    /// The registered local project whose directory most closely contains `path`.
    pub fn project_containing(&self, path: &str) -> Result<Option<Project>> {
        let path = normalize_project_path(path)?;
        Ok(self
            .projects()?
            .into_iter()
            .filter(|p| {
                let root = p.abs_path.trim_end_matches(['/', '\\']);
                path == p.abs_path
                    || path
                        .strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with(['/', '\\']))
            })
            .max_by_key(|p| p.abs_path.len()))
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {PROJECT_COLUMNS} FROM projects WHERE node_id = ?1 ORDER BY slug"
            ))?;
            let rows = stmt.query_map([self.node_id()], project_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Register a new agent or update an existing one's details; touches `last_seen_at`.
    pub fn register_agent(&self, input: RegisterAgent) -> Result<Agent> {
        validate_name(&input.name)?;
        self.write(|tx| {
            let project = match &input.project {
                Some(path) => Some(ensure_project_tx(tx, &input.name, path)?),
                None => None,
            };
            let project_id = project.as_ref().map(|p| p.project_id.clone());
            let slug = project.as_ref().map(|p| p.slug.as_str());
            let addr = address(&input.name, slug);
            let existing: Option<String> = tx
                .query_row(
                    "SELECT agent_id FROM agents
                     WHERE node_id = ?1 AND name = ?2 AND ifnull(project_id, '') = ifnull(?3, '')",
                    params![tx.node_id, input.name, project_id],
                    |r| r.get(0),
                )
                .optional()?;
            let agent_id = existing.clone().unwrap_or_else(new_id);
            let event_id = tx.emit(
                &addr,
                "agent.registered",
                json!({
                    "agent_id": agent_id,
                    "name": input.name,
                    "project_id": project_id,
                    "program": input.program.as_str(),
                    "model": input.model,
                    "description": input.description,
                    "new": existing.is_none(),
                }),
            )?;
            if existing.is_some() {
                tx.execute(
                    "UPDATE agents SET program = ?2, model = ?3, description = ?4,
                       status = 'active', updated_at = ?5, last_seen_at = ?5, event_id = ?6,
                       address = ?7
                     WHERE agent_id = ?1",
                    params![
                        agent_id,
                        input.program.as_str(),
                        input.model,
                        input.description,
                        tx.now,
                        event_id,
                        addr
                    ],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO agents (agent_id, node_id, project_id, name, program, model,
                       description, status, created_at, updated_at, last_seen_at, event_id, address)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?8, ?8, ?9, ?10)",
                    params![
                        agent_id,
                        tx.node_id,
                        project_id,
                        input.name,
                        input.program.as_str(),
                        input.model,
                        input.description,
                        tx.now,
                        event_id,
                        addr
                    ],
                )?;
            }
            get_agent(tx, &agent_id)
        })
    }

    /// Look up one agent by address (`name`, `name@project`, or `name@project@node`).
    pub fn agent(&self, who: &str) -> Result<Agent> {
        self.read(|conn| {
            let r = resolve_any(conn, self.node_id(), who)?;
            get_agent(conn, &r.id)
        })
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {AGENT_COLUMNS} FROM agents a
                 LEFT JOIN projects p ON p.project_id = a.project_id
                 WHERE a.node_id = ?1 ORDER BY a.name, p.slug"
            ))?;
            let rows = stmt.query_map([self.node_id()], agent_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_bindings_are_distinct_stable_and_do_not_persist_raw_ids() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let raw = "unique-private-session-98";
        let (a, fingerprint) = store
            .session_actor("claude", Program::Claude, root, None, raw, true)
            .unwrap();
        let (same, _) = store
            .session_actor("claude", Program::Claude, root, Some(&a), raw, false)
            .unwrap();
        let (b, _) = store
            .session_actor("claude", Program::Claude, root, None, "other-session", true)
            .unwrap();
        assert_eq!(a, same);
        assert_ne!(a, b);
        assert!(store.is_session_actor(&a).unwrap());
        let bare = a.split('@').next().unwrap();
        assert_eq!(store.session_actor_address(bare).unwrap(), a);
        let other = tempfile::tempdir().unwrap();
        store
            .register_agent(RegisterAgent {
                name: bare.into(),
                program: Program::Claude,
                project: Some(other.path().to_str().unwrap().into()),
                model: None,
                description: None,
            })
            .unwrap();
        assert!(matches!(
            store.session_actor_address(bare),
            Err(Error::Ambiguous(_))
        ));
        assert_eq!(store.session_actor_address(&a).unwrap(), a);
        assert!(store
            .session_actor(
                "claude",
                Program::Claude,
                root,
                Some(&a),
                "other-session",
                false
            )
            .is_err());
        assert!(store
            .session_actor("claude", Program::Claude, root, None, "", true)
            .is_err());
        store
            .session(
                &a,
                crate::holds::SessionPhase::Start,
                crate::holds::SessionInfo {
                    client: Some("claude".into()),
                    session_id: Some(raw.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let events = serde_json::to_string(&store.events(100).unwrap()).unwrap();
        assert!(!events.contains(raw));
        assert!(events.contains(&fingerprint));
    }

    #[test]
    fn concurrent_connections_converge_on_one_session_actor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir(&root).unwrap();
        let home = dir.path().join("home");
        let first = Store::open(&home).unwrap();
        let second = Store::open(&home).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let root = root.to_str().unwrap().to_owned();
        let spawn = |store: Store| {
            let barrier = barrier.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .session_actor("codex", Program::Codex, &root, None, "same-session", true)
                    .unwrap()
                    .0
            })
        };
        let a = spawn(first);
        let b = spawn(second);
        assert_eq!(a.join().unwrap(), b.join().unwrap());
    }

    #[test]
    fn existing_unbound_actor_is_never_reused() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let (actor, _) = store
            .session_actor("claude", Program::Claude, root, None, "session", true)
            .unwrap();
        store
            .write(|tx| {
                tx.execute("DELETE FROM session_bindings", [])?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.session_actor("claude", Program::Claude, root, None, "session", true),
            Err(Error::Invalid(_))
        ));
        assert!(store.agent(&actor).is_ok());
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_project_path("/a/b/").unwrap(), "/a/b");
        assert_eq!(normalize_project_path("/a/./b/../c").unwrap(), "/a/c");
        assert!(normalize_project_path("rel/path").is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slug_base("/work/My Repo"), "my-repo");
        assert_eq!(slug_base("/"), "root");
    }
}
