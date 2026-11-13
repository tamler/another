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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionBinding {
    version: u8,
    agent_id: String,
    project_id: String,
    client: String,
    base_name: String,
    digest: String,
}

fn binding_scan(tx: &Tx<'_>, deadline: std::time::Instant) -> Result<Vec<SessionBinding>> {
    binding_scan_inner(tx, deadline).map_err(|err| match err {
        Error::Db(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::TooBig
                    | rusqlite::ErrorCode::OperationInterrupted
                    | rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Error::IdentityScanBudgetExceeded(
                "SQLite scan limit, interruption or contention".into(),
            )
        }
        Error::Db(_) => Error::Invalid("invalid or unavailable session binding evidence".into()),
        other => other,
    })
}

fn binding_scan_inner(tx: &Tx<'_>, deadline: std::time::Instant) -> Result<Vec<SessionBinding>> {
    use rusqlite::types::ValueRef;
    use std::collections::HashSet;
    let invalid = || Error::Invalid("invalid session binding evidence".into());
    let check_deadline = || {
        if std::time::Instant::now() >= deadline {
            Err(Error::IdentityScanBudgetExceeded("deadline".into()))
        } else {
            Ok(())
        }
    };
    let mut stmt = tx.prepare("SELECT seq,event_id,node_id,ts,actor,kind,payload,prev_id,sig FROM events INDEXED BY events_by_kind WHERE kind=?1 AND node_id=?2 ORDER BY seq LIMIT 50001")?;
    let mut rows = stmt.query(params!["session.binding.v1", tx.node_id])?;
    let mut result = Vec::new();
    let mut bytes = 0usize;
    let mut ids = HashSet::new();
    let mut digests = HashSet::new();
    let public = crate::crypto::public_of(tx.key);
    loop {
        check_deadline()?;
        let Some(row) = rows.next()? else { break };
        if result.len() == 50_000 {
            return Err(Error::IdentityScanBudgetExceeded("row limit".into()));
        }
        let text = |i, max| -> Result<&str> {
            match row.get_ref(i)? {
                ValueRef::Text(value) if value.len() <= max => {
                    std::str::from_utf8(value).map_err(|_| invalid())
                }
                _ => Err(invalid()),
            }
        };
        if !matches!(row.get_ref(0)?, ValueRef::Integer(_)) {
            return Err(invalid());
        }
        let event = text(1, 128)?;
        let node = text(2, 128)?;
        let ts = match row.get_ref(3)? {
            ValueRef::Integer(n) => n,
            _ => return Err(invalid()),
        };
        let actor = text(4, 256)?;
        let kind = text(5, 64)?;
        let payload = text(6, 2048)?;
        let prev = match row.get_ref(7)? {
            ValueRef::Null => 0,
            ValueRef::Text(v) if v.len() <= 128 && std::str::from_utf8(v).is_ok() => v.len(),
            _ => return Err(invalid()),
        };
        let signature = text(8, 256)?;
        bytes = bytes
            .checked_add(
                event.len()
                    + node.len()
                    + actor.len()
                    + kind.len()
                    + payload.len()
                    + prev
                    + signature.len(),
            )
            .ok_or_else(invalid)?;
        if bytes > 16 * 1024 * 1024 {
            return Err(Error::IdentityScanBudgetExceeded("byte limit".into()));
        }
        if signature.len() != 86
            || crate::crypto::unb64(signature)
                .ok()
                .filter(|v| v.len() == 64 && crate::crypto::b64(v) == signature)
                .is_none()
        {
            return Err(invalid());
        }
        if !crate::crypto::verify(
            &public,
            &crate::crypto::event_message(event, node, ts, actor, kind, payload),
            signature,
        ) {
            return Err(invalid());
        }
        check_deadline()?;
        let binding: SessionBinding = serde_json::from_str(payload).map_err(|_| invalid())?;
        if binding.version != 1
            || binding.digest.len() != 64
            || !binding
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || validate_name(&binding.base_name).is_err()
            || !matches!(
                binding.client.as_str(),
                "claude" | "codex" | "cursor" | "grok"
            )
            || !ids.insert(binding.agent_id.clone())
            || !digests.insert(binding.digest.clone())
        {
            return Err(invalid());
        }
        let actual: Option<(String,String,String,String,Option<String>,String)> = tx.query_row("SELECT a.node_id,a.name,a.project_id,p.slug,a.address,p.node_id FROM agents a JOIN projects p ON p.project_id=a.project_id WHERE a.agent_id=?1", [&binding.agent_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let Some((agent_node, name, project_id, slug, stored_address, project_node)) = actual
        else {
            return Err(invalid());
        };
        let expected = format!(
            "{}-s{}",
            &binding.base_name[..binding.base_name.len().min(30)],
            &binding.digest[..32]
        );
        if agent_node != tx.node_id
            || project_node != tx.node_id
            || project_id != binding.project_id
            || name != expected
            || actor != address(&name, Some(&slug))
            || stored_address.as_deref() != Some(actor)
        {
            return Err(invalid());
        }
        check_deadline()?;
        result.push(binding);
    }
    Ok(result)
}

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
        #[cfg(windows)]
        {
            let _ = (client, program, root, requested_base, session_id, create);
            return Err(Error::Invalid(
                "session binding is unsupported on native Windows".into(),
            ));
        }
        #[cfg(not(windows))]
        {
            if session_id.is_empty()
                || session_id.len() > 512
                || session_id.chars().any(char::is_control)
            {
                return Err(Error::Invalid("invalid session identifier".into()));
            }
            self.write_identity(|tx, deadline| {
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
            let bindings = binding_scan(tx, deadline)?;
            let requested_id: Option<String> = tx.query_row("SELECT agent_id FROM agents WHERE node_id=?1 AND name=?2 AND project_id=?3", params![tx.node_id,name,project.project_id], |r| r.get(0)).optional()?;
            let existing_binding = bindings.iter().find(|b| Some(&b.agent_id) == requested_id.as_ref());
            let base = existing_binding.map_or(name, |binding| binding.base_name.as_str());
            let mut hash = Sha256::new();
            for value in ["another-session-v1", tx.node_id, client, project.project_id.as_str(), base, session_id] {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            let digest = format!("{:x}", hash.finalize());
            let actor_name = format!("{}-s{}", &base[..base.len().min(30)], &digest[..32]);
            if let Some(binding) = existing_binding {
                if binding.client != client || binding.digest != digest || actor_name != name {
                    return Err(Error::Invalid("session actor does not match this session".into()));
                }
            }
            let addr = address(&actor_name, Some(&project.slug));
            if let Some(binding) = bindings.iter().find(|b| b.digest == digest) {
                if binding.project_id != project.project_id || binding.client != client || binding.base_name != base {
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
            let agent_id = new_id();
            let event_id = tx.emit(&addr, "agent.registered", json!({
                "agent_id": agent_id, "name": actor_name, "project_id": project.project_id,
                "program": program.as_str(), "model": null, "description": null, "new": true
            }))?;
            tx.execute(
                "INSERT INTO agents (agent_id,node_id,project_id,name,program,status,created_at,updated_at,last_seen_at,event_id,address) VALUES (?1,?2,?3,?4,?5,'active',?6,?6,?6,?7,?8)",
                params![agent_id, tx.node_id, project.project_id, actor_name, program.as_str(), tx.now, event_id, addr],
            )?;
            tx.emit(&addr, "session.binding.v1", json!({"version":1,"agent_id":agent_id,"project_id":project.project_id,"client":client,"base_name":base,"digest":digest}))?;
            Ok((addr, digest))
        })
        }
    }

    /// Check that an address identifies a locally registered session actor.
    pub fn is_session_actor(&self, who: &str) -> Result<bool> {
        #[cfg(windows)]
        {
            let _ = who;
            return Err(Error::Invalid(
                "session binding is unsupported on native Windows".into(),
            ));
        }
        #[cfg(not(windows))]
        {
            self.write_identity(|tx, deadline| {
                let actor = resolve_agent(tx, self.node_id(), who)?;
                Ok(binding_scan(tx, deadline)?
                    .iter()
                    .any(|b| b.agent_id == actor.id))
            })
        }
    }

    /// Resolve a session actor to its complete local address.
    pub fn session_actor_address(&self, who: &str) -> Result<String> {
        #[cfg(windows)]
        {
            let _ = who;
            return Err(Error::Invalid(
                "session binding is unsupported on native Windows".into(),
            ));
        }
        #[cfg(not(windows))]
        {
            self.write_identity(|tx, deadline| {
                let actor = resolve_agent(tx, self.node_id(), who)?;
                let is_session = binding_scan(tx, deadline)?
                    .iter()
                    .any(|b| b.agent_id == actor.id);
                if !is_session {
                    return Err(Error::Invalid(
                        "agent is not a registered session actor".into(),
                    ));
                }
                Ok(actor.address)
            })
        }
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
        self.write_identity(|tx, deadline| {
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
            if let Some(id) = &existing {
                if binding_scan(tx, deadline)?
                    .iter()
                    .any(|b| &b.agent_id == id)
                {
                    return Err(Error::Invalid(
                        "cannot update a session actor with agent register".into(),
                    ));
                }
            }
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
                tx.execute("DELETE FROM events WHERE kind='session.binding.v1'", [])?;
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
    fn emitted_binding_roundtrips_and_corruption_denies_without_new_actor() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let (actor, _) = store
            .session_actor("claude", Program::Claude, root, None, "one", true)
            .unwrap();
        assert_eq!(
            store
                .session_actor("claude", Program::Claude, root, None, "one", false)
                .unwrap()
                .0,
            actor
        );
        store
            .read(|conn| {
                let (kind, sig, ts_type): (String, String, String) = conn.query_row(
                    "SELECT kind,sig,typeof(ts) FROM events WHERE kind='session.binding.v1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                assert_eq!(kind, "session.binding.v1");
                assert!(!sig.is_empty());
                assert_eq!(ts_type, "integer");
                Ok(())
            })
            .unwrap();
        store
            .write(|tx| {
                tx.execute(
                    "UPDATE events SET payload='{}' WHERE kind='session.binding.v1'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.session_actor("claude", Program::Claude, root, None, "two", true),
            Err(Error::Invalid(_))
        ));
        assert_eq!(store.agents().unwrap().len(), 1);
    }

    #[test]
    fn a_thousand_valid_binding_events_fit_the_identity_budget() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        store.write(|tx| {
            let project = ensure_project_tx(tx, "fixture", root)?;
            tx.execute("INSERT INTO nodes(node_id,display_name,role,is_local,created_at) VALUES ('foreign-fixture','foreign','home',0,?1)", [tx.now])?;
            for _ in 0..1000 {
                tx.execute("INSERT INTO events(event_id,node_id,ts,actor,kind,payload) VALUES (?1,'foreign-fixture',?2,'foreign','session.binding.v1','{}')", params![new_id(),tx.now])?;
            }
            for n in 0..1000u64 {
                // Vary the truncated prefix as the real tuple hash does.
                let digest = format!("{:032x}{:032x}", n + 1, n);
                let name = format!("claude-s{}", &digest[..32]);
                let addr = address(&name, Some(&project.slug));
                let agent_id = new_id();
                let event_id = tx.emit(&addr, "agent.registered", json!({"agent_id":agent_id}))?;
                tx.execute("INSERT INTO agents (agent_id,node_id,project_id,name,program,status,created_at,updated_at,last_seen_at,event_id,address) VALUES (?1,?2,?3,?4,'claude','active',?5,?5,?5,?6,?7)", params![agent_id,tx.node_id,project.project_id,name,tx.now,event_id,addr])?;
                tx.emit(&addr, "session.binding.v1", json!({"version":1,"agent_id":agent_id,"project_id":project.project_id,"client":"claude","base_name":"claude","digest":digest}))?;
            }
            Ok(())
        }).unwrap();
        store
            .session_actor("claude", Program::Claude, root, None, "new-session", true)
            .unwrap();
    }

    #[test]
    fn binding_timestamp_extremes_verify_and_oversize_scan_denies() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let (actor, _) = store
            .session_actor("claude", Program::Claude, root, None, "original", true)
            .unwrap();
        for timestamp in [i64::MIN, 0, i64::MAX] {
            store.write(|tx| {
                let (id,node,who,kind,payload): (String,String,String,String,String) = tx.query_row(
                    "SELECT event_id,node_id,actor,kind,payload FROM events WHERE kind='session.binding.v1'", [],
                    |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
                let sig = crate::crypto::sign(tx.key, &crate::crypto::event_message(&id,&node,timestamp,&who,&kind,&payload));
                tx.execute("UPDATE events SET ts=?1,sig=?2 WHERE event_id=?3", params![timestamp,sig,id])?;
                Ok(())
            }).unwrap();
            assert_eq!(
                store
                    .session_actor("claude", Program::Claude, root, None, "original", false)
                    .unwrap()
                    .0,
                actor
            );
        }
        store
            .write(|tx| {
                tx.execute(
                    "UPDATE events SET payload=?1 WHERE kind='session.binding.v1'",
                    ["x".repeat(9000)],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.session_actor("claude", Program::Claude, root, None, "other", true),
            Err(Error::IdentityScanBudgetExceeded(_))
        ));
        assert_eq!(store.agents().unwrap().len(), 1);
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
