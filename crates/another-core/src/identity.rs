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
    let result = binding_scan_inner(tx, deadline);
    if std::time::Instant::now() >= deadline {
        return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
    }
    result.map_err(|err| match err {
        Error::Db(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::TooBig
                    | rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Error::IdentityScanBudgetExceeded(
                "SQLite scan limit, interruption or contention".into(),
            )
        }
        Error::Db(rusqlite::Error::SqliteFailure(code, message))
            if code.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            Error::Db(rusqlite::Error::SqliteFailure(code, message))
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
        let next = rows.next()?;
        check_deadline()?;
        let Some(row) = next else { break };
        if result.len() == 50_000 {
            return Err(Error::IdentityScanBudgetExceeded("row limit".into()));
        }
        let text = |i, max| -> Result<&[u8]> {
            match row.get_ref(i)? {
                ValueRef::Text(value) if value.len() <= max => Ok(value),
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
            ValueRef::Text(v) if v.len() <= 128 => v.len(),
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
        check_deadline()?;
        let decode = |value| std::str::from_utf8(value).map_err(|_| invalid());
        let event = decode(event)?;
        let node = decode(node)?;
        let actor = decode(actor)?;
        let kind = decode(kind)?;
        let payload = decode(payload)?;
        let signature = decode(signature)?;
        if let ValueRef::Text(value) = row.get_ref(7)? {
            decode(value)?;
        }
        check_deadline()?;
        if signature.len() != 86
            || crate::crypto::unb64(signature)
                .ok()
                .filter(|v| v.len() == 64 && crate::crypto::b64(v) == signature)
                .is_none()
        {
            return Err(invalid());
        }
        check_deadline()?;
        if !crate::crypto::verify(
            &public,
            &crate::crypto::event_message(event, node, ts, actor, kind, payload),
            signature,
        ) {
            return Err(invalid());
        }
        check_deadline()?;
        let binding: SessionBinding = serde_json::from_str(payload).map_err(|_| invalid())?;
        check_deadline()?;
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
        Some(Component::Prefix(p)) => match p.kind() {
            // canonicalize returns a verbatim drive path on Windows. Do not combine
            // that prefix with the legacy forward-slash project key representation.
            std::path::Prefix::VerbatimDisk(drive) => format!("{}:\\", char::from(drive)),
            _ => format!("{}\\", p.as_os_str().to_string_lossy()),
        },
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

/// Highest `-N` suffix tried when a project's directory name is already a slug, so
/// registering a project is bounded work.
const MAX_SLUG_SUFFIX: u32 = 1000;

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
        if n > MAX_SLUG_SUFFIX {
            return Err(Error::Invalid(format!(
                "too many projects are named {base:?}; register this one from a differently named directory"
            )));
        }
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

// This helper is not an authorization token and does not brand the transaction.
// The public entry point validates the session ID before entering the transaction.
struct SessionResolution<'a> {
    requested_actor: Option<(&'a str, bool)>,
    create: bool,
}

fn session_actor_tx(
    tx: &Tx<'_>,
    deadline: std::time::Instant,
    client: &str,
    program: Program,
    root: &str,
    session_id: &str,
    resolution: SessionResolution<'_>,
) -> Result<(String, String)> {
    let SessionResolution {
        requested_actor,
        create,
    } = resolution;
    let exact_session = requested_actor.is_some_and(|(_, exact)| exact);
    let project = match find_project(tx, tx.node_id, root) {
        Ok(project) => project,
        Err(Error::NotFound(_)) if create && !exact_session => ensure_project_tx(tx, client, root)?,
        Err(e) => return Err(e),
    };
    let requested = requested_actor.map_or(client, |(actor, _)| actor);
    let (name, slug) = match requested.split_once('@') {
        Some((name, slug)) if !slug.contains('@') => (name, Some(slug)),
        Some(_) => return Err(Error::Invalid("invalid session base address".into())),
        None => (requested, None),
    };
    if slug.is_some_and(|slug| slug != project.slug) {
        return Err(Error::Invalid("session base project does not match".into()));
    }
    if exact_session && slug.is_none() {
        return Err(Error::Invalid(
            "session actor requires a full address".into(),
        ));
    }
    validate_name(name)?;
    let bindings = binding_scan(tx, deadline)?;
    let existing_binding = if exact_session {
        let requested_id: Option<String> = tx
            .query_row(
                "SELECT agent_id FROM agents WHERE node_id=?1 AND name=?2 AND project_id=?3",
                params![tx.node_id, name, project.project_id],
                |r| r.get(0),
            )
            .optional()?;
        bindings
            .iter()
            .find(|b| Some(&b.agent_id) == requested_id.as_ref())
    } else {
        None
    };
    if exact_session && existing_binding.is_none() {
        return Err(Error::NotFound("session binding missing".into()));
    }
    let base = existing_binding.map_or(name, |binding| binding.base_name.as_str());
    let mut hash = Sha256::new();
    for value in [
        "another-session-v1",
        tx.node_id,
        client,
        project.project_id.as_str(),
        base,
        session_id,
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    let digest = format!("{:x}", hash.finalize());
    let actor_name = format!("{}-s{}", &base[..base.len().min(30)], &digest[..32]);
    if let Some(binding) = existing_binding {
        if binding.client != client || binding.digest != digest || actor_name != name {
            return Err(Error::Invalid(
                "session actor does not match this session".into(),
            ));
        }
    }
    let addr = address(&actor_name, Some(&project.slug));
    if let Some(binding) = bindings.iter().find(|b| b.digest == digest) {
        if binding.project_id != project.project_id
            || binding.client != client
            || binding.base_name != base
        {
            return Err(Error::Invalid("session identity collision".into()));
        }
        return Ok((addr, digest));
    }
    if !create || exact_session {
        return Err(Error::NotFound("session identity has not started".into()));
    }
    let occupied: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agents WHERE node_id=?1 AND project_id=?2 AND name=?3)",
        params![tx.node_id, project.project_id, actor_name],
        |r| r.get(0),
    )?;
    if occupied {
        return Err(Error::Invalid("session identity collision".into()));
    }
    let agent_id = new_id();
    let event_id = tx.emit(
        &addr,
        "agent.registered",
        json!({
            "agent_id": agent_id, "name": actor_name, "project_id": project.project_id,
            "program": program.as_str(), "model": null, "description": null, "new": true
        }),
    )?;
    tx.execute(
                "INSERT INTO agents (agent_id,node_id,project_id,name,program,status,created_at,updated_at,last_seen_at,event_id,address) VALUES (?1,?2,?3,?4,?5,'active',?6,?6,?6,?7,?8)",
                params![agent_id, tx.node_id, project.project_id, actor_name, program.as_str(), tx.now, event_id, addr],
            )?;
    tx.emit(&addr, "session.binding.v1", json!({"version":1,"agent_id":agent_id,"project_id":project.project_id,"client":client,"base_name":base,"digest":digest}))?;
    Ok((addr, digest))
}

fn resolve_session_actor_tx(
    tx: &Tx<'_>,
    deadline: std::time::Instant,
    client: &str,
    program: Program,
    root: &str,
    requested_actor: Option<(&str, bool)>,
    session_id: &str,
) -> Result<(String, String)> {
    session_actor_tx(
        tx,
        deadline,
        client,
        program,
        root,
        session_id,
        SessionResolution {
            requested_actor,
            create: false,
        },
    )
}

/// Look up an already-bound session actor by its exact local `name@project` address,
/// inside the caller's identity transaction. Never creates an agent, project or binding.
/// An address is an identifier, not a credential: this says which session actor an
/// address names, not that the caller is that session.
/// Its caller is the controlled-edit admission built on this commit.
#[allow(dead_code)]
pub(crate) fn resolve_bound_session_tx(
    tx: &Tx<'_>,
    deadline: std::time::Instant,
    address: &str,
) -> Result<AgentRef> {
    let exact = || Error::Invalid("session actor requires an exact name@project address".into());
    if address.split('@').count() != 2 {
        return Err(exact());
    }
    let agent = resolve_agent(tx, tx.node_id, address)?;
    if agent.address != address {
        return Err(exact());
    }
    if !binding_scan(tx, deadline)?
        .iter()
        .any(|b| b.agent_id == agent.id)
    {
        return Err(Error::Invalid(
            "agent is not a registered session actor".into(),
        ));
    }
    Ok(agent)
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > 512 || session_id.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid session identifier".into()));
    }
    Ok(())
}

/// Outcome of [`Store::admit_edit`].
#[derive(Debug, Clone)]
pub struct EditAdmission {
    /// The session actor the edit was admitted (or stopped) for.
    pub agent: String,
    /// Other agents' holds that stop the edit. When not empty, no hold was taken.
    pub conflicts: Vec<crate::holds::HoldConflict>,
    /// Directories of unregistered projects the edit reaches. When not empty, no hold was
    /// taken: admission never registers a project.
    pub unregistered: Vec<String>,
    /// Targets outside every project in an edit that also has guarded files. When not
    /// empty, no hold was taken: a partly guarded edit is not admitted.
    pub unguarded: Vec<String>,
}

impl EditAdmission {
    /// Whether the edit may proceed: every guarded file is now held by the session actor
    /// with a full TTL, and no target of a guarded edit is left unguarded.
    pub fn admitted(&self) -> bool {
        self.conflicts.is_empty() && self.unregistered.is_empty() && self.unguarded.is_empty()
    }
}

/// How the files of one edit map onto projects, as decided by the caller of
/// [`Store::admit_edit`] from the registered projects it was given.
#[derive(Debug, Clone, Default)]
pub struct EditScope {
    /// `(project directory, files relative to it)` for targets inside a project.
    pub groups: Vec<(String, Vec<String>)>,
    /// Targets that belong to no git checkout and no registered project.
    pub unguarded: Vec<String>,
}

/// Most distinct project groups one edit may span.
const MAX_ADMISSION_GROUPS: usize = 32;
/// Most files one edit may name across all its projects.
const MAX_ADMISSION_FILES: usize = 128;

#[path = "controlled.rs"]
mod controlled;
pub use controlled::{
    controlled_sha256, ControlledClaim, ControlledEdit, CONTROLLED_EDIT_TOOL, MAX_CONTROLLED_BYTES,
};

fn admission_target_hash(
    key: &[u8; 32],
    admission_id: &str,
    mut targets: Vec<(&str, &str, &str)>,
) -> Result<[u8; 32]> {
    use ring::hmac;
    let count = targets.len();
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() || targets.len() > MAX_ADMISSION_FILES || targets.len() != count {
        return Err(Error::Invalid(
            "edit has no verifiable protected targets".into(),
        ));
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut digest = hmac::Context::with_key(&key);
    digest.update(b"another-admission-hold-set-v1\0");
    digest.update(&(admission_id.len() as u64).to_be_bytes());
    digest.update(admission_id.as_bytes());
    digest.update(&(targets.len() as u64).to_be_bytes());
    for (id, project, path) in targets {
        for value in [id, project, path] {
            digest.update(&(value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    let mut out = [0; 32];
    out.copy_from_slice(digest.sign().as_ref());
    Ok(out)
}

fn validate_tool_use_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::IdentityUnavailable(
            "invalid edit operation identifier".into(),
        ));
    }
    Ok(())
}

fn admission_key(tx: &Tx<'_>) -> Result<[u8; 32]> {
    use rusqlite::types::ValueRef;
    {
        let mut stmt = tx.prepare(
            "SELECT singleton, typeof(secret), CASE WHEN typeof(secret)='blob' THEN length(secret) END
             FROM admission_secret LIMIT 2",
        )?;
        let mut rows = stmt.query([])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::IdentityUnavailable("admission key unavailable".into()))?;
        if !matches!(row.get_ref(0)?, ValueRef::Integer(1))
            || !matches!(row.get_ref(1)?, ValueRef::Text(b"blob"))
            || !matches!(row.get_ref(2)?, ValueRef::Integer(32))
            || rows.next()?.is_some()
        {
            return Err(Error::IdentityUnavailable(
                "admission key unavailable".into(),
            ));
        }
    }
    let secret: Option<Vec<u8>> = tx
        .query_row(
            "SELECT CASE WHEN typeof(secret)='blob' AND length(secret)=32 THEN secret END
         FROM admission_secret WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let secret = secret
        .filter(|s| s.len() == 32)
        .ok_or_else(|| Error::IdentityUnavailable("admission key unavailable".into()))?;
    secret
        .try_into()
        .map_err(|_| Error::IdentityUnavailable("admission key unavailable".into()))
}

fn admission_correlation(
    tx: &Tx<'_>,
    client: &str,
    agent_id: &str,
    session_digest: &str,
    tool_use_id: &str,
) -> Result<[u8; 32]> {
    use ring::hmac;
    validate_tool_use_id(tool_use_id)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &admission_key(tx)?);
    let mut ctx = hmac::Context::with_key(&key);
    for value in [
        "another-admission-v1",
        tx.node_id,
        client,
        agent_id,
        session_digest,
        tool_use_id,
    ] {
        ctx.update(&(value.len() as u64).to_be_bytes());
        ctx.update(value.as_bytes());
    }
    let mut out = [0; 32];
    out.copy_from_slice(ctx.sign().as_ref());
    Ok(out)
}

impl Store {
    /// Confirm only the exact live pending operation after a verified successful tool event.
    /// An absent, stale or already-consumed operation changes no hold.
    #[allow(clippy::too_many_arguments)]
    pub fn confirm_pending_edit(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_actor: Option<(&str, bool)>,
        session_id: &str,
        tool_use_id: &str,
    ) -> Result<bool> {
        validate_session_id(session_id)?;
        validate_tool_use_id(tool_use_id)?;
        if client != "claude" {
            return Err(Error::IdentityUnavailable(
                "this client does not have verified edit receipts".into(),
            ));
        }
        self.write_identity(|tx, deadline| {
            let (agent, session_digest) = resolve_session_actor_tx(
                tx, deadline, client, program, root, requested_actor, session_id,
            )?;
            let me = resolve_agent(tx, tx.node_id, &agent)?;
            let correlation = admission_correlation(tx, client, &me.id, &session_digest, tool_use_id)?;
            let admission: Option<(String, Vec<u8>, i64)> = tx.query_row(
                "SELECT admission_id,targets,created_at FROM admissions WHERE node_id=?1 AND agent_id=?2 AND correlation=?3 AND state='pending' AND call_key IS NULL",
                params![tx.node_id, me.id, &correlation[..]], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).optional()?;
            let Some((admission, target_hash, created_at)) = admission else { return Ok(false) };
            let holds = {
                let mut stmt = tx.prepare(
                    "SELECT h.hold_id,h.node_id,h.agent_id,h.expires_at,h.released_at,h.ttl_seconds,h.project_id,h.pattern,h.exclusive,h.created_at
                     FROM admission_holds ah JOIN holds h ON h.hold_id=ah.hold_id
                     WHERE ah.admission_id=?1 ORDER BY h.hold_id LIMIT 129",
                )?;
                let rows = stmt.query_map([&admission], |r| Ok((
                    r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, bool>(8)?, r.get::<_, i64>(9)?,
                )))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mapped: i64 = tx.query_row(
                "SELECT count(*) FROM (SELECT hold_id FROM admission_holds WHERE admission_id=?1 LIMIT 129)",
                [&admission], |r| r.get(0),
            )?;
            if holds.is_empty() || holds.len() > MAX_ADMISSION_FILES || mapped as usize != holds.len() || holds.iter().any(|(_, node, owner, expires, released, ttl, _, _, exclusive, created)|
                node != tx.node_id || owner != &me.id || *expires <= tx.now || released.is_some()
                    || *ttl != crate::holds::DEFAULT_HOLD_TTL_SECONDS
                    || !exclusive || *created != created_at
                    || created_at.checked_add(ttl.saturating_mul(1000)) != Some(*expires)
            ) {
                return Ok(false);
            }
            let targets = holds.iter().map(|(id, _, _, _, _, _, project, pattern, _, _)| (id.as_str(), project.as_str(), pattern.as_str())).collect();
            if admission_target_hash(&admission_key(tx)?, &admission, targets).ok().as_ref().map(|v| v.as_slice()) != Some(target_hash.as_slice()) {
                return Ok(false);
            }
            let ids: Vec<&str> = holds.iter().map(|(id, ..)| id.as_str()).collect();
            tx.emit(&me.address, "admission.confirmed", json!({"admission_id": admission}))?;
            tx.emit(&me.address, "hold.renewed", json!({"hold_ids": ids}))?;
            tx.execute("UPDATE admissions SET state='confirmed',settled_at=?2 WHERE admission_id=?1 AND state='pending'",
                params![admission, tx.now])?;
            for (id, _, _, _, _, ttl, ..) in &holds {
                tx.execute("UPDATE holds SET renewed_at=?2,expires_at=?2+?3*1000 WHERE hold_id=?1 AND node_id=?4 AND agent_id=?5 AND released_at IS NULL AND expires_at>?2",
                    params![id, tx.now, ttl, tx.node_id, me.id])?;
            }
            Ok(true)
        })
    }

    /// Create separate pending coverage for a protected edit within the identity transaction.
    /// Callers must verify client protocol and filesystem effects before using this result.
    #[allow(clippy::too_many_arguments)]
    pub fn admit_pending_edit(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_actor: Option<(&str, bool)>,
        session_id: &str,
        tool_use_id: &str,
        reason: &str,
        group: impl FnOnce(&[Project]) -> EditScope,
    ) -> Result<EditAdmission> {
        validate_session_id(session_id)?;
        validate_tool_use_id(tool_use_id)?;
        if client != "claude" {
            return Err(Error::IdentityUnavailable(
                "this client does not have verified edit receipts".into(),
            ));
        }
        self.write_identity(|tx, deadline| {
            let (agent, session_digest) = resolve_session_actor_tx(
                tx, deadline, client, program, root, requested_actor, session_id,
            )?;
            let me = resolve_agent(tx, tx.node_id, &agent)?;
            let session_project = find_project(tx, tx.node_id, root)?;
            let projects = {
                let mut stmt = tx.prepare(&format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE node_id = ?1 ORDER BY slug"))?;
                let rows = stmt.query_map([tx.node_id], project_from_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let EditScope { groups, unguarded } = group(&projects);
            if std::time::Instant::now() >= deadline {
                return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
            }
            if groups.is_empty() {
                return Ok(EditAdmission { agent, conflicts: Vec::new(), unregistered: Vec::new(), unguarded: Vec::new() });
            }
            if !unguarded.is_empty() {
                return Ok(EditAdmission { agent, conflicts: Vec::new(), unregistered: Vec::new(), unguarded });
            }
            if groups.len() > MAX_ADMISSION_GROUPS || groups.iter().map(|(_, files)| files.len()).sum::<usize>() > MAX_ADMISSION_FILES {
                return Err(Error::Invalid("edit exceeds admission target limits".into()));
            }
            let mut plans = Vec::new();
            for (directory, files) in &groups {
                let project = find_project(tx, tx.node_id, directory)?;
                if project.project_id != session_project.project_id {
                    return Err(Error::IdentityUnavailable("edit reaches another project".into()));
                }
                plans.push(crate::holds::guard_plan_tx(tx, &me, project, files)?);
            }
            if plans.iter().any(|p| !p.conflicts.is_empty()) {
                let mut conflicts = Vec::new();
                for plan in plans {
                    if !plan.conflicts.is_empty() {
                        conflicts.extend(crate::holds::guard_blocked_tx(tx, &me, plan)?.conflicts);
                    }
                }
                return Ok(EditAdmission { agent, conflicts, unregistered: Vec::new(), unguarded: Vec::new() });
            }
            let correlation = admission_correlation(tx, client, &me.id, &session_digest, tool_use_id)?;
            let duplicate: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM admissions WHERE node_id=?1 AND agent_id=?2 AND correlation=?3)",
                params![tx.node_id, me.id, &correlation[..]], |r| r.get(0),
            )?;
            if duplicate {
                return Err(Error::IdentityUnavailable("edit operation was already seen; coordinate before retrying".into()));
            }
            let mut targets: Vec<(&str, &str)> = plans.iter().flat_map(|plan| {
                plan.patterns.iter().map(move |(_, pattern)| (plan.project.project_id.as_str(), pattern.as_str()))
            }).collect();
            let target_count = targets.len();
            targets.sort_unstable();
            targets.dedup();
            if targets.is_empty() || targets.len() > MAX_ADMISSION_FILES || targets.len() != target_count {
                return Err(Error::Invalid("edit has no verifiable protected targets".into()));
            }
            let admission_id = new_id();
            let key = admission_key(tx)?;
            tx.emit(&me.address, "admission.created", json!({"admission_id": admission_id}))?;
            tx.execute(
                "INSERT INTO admissions(admission_id,node_id,agent_id,correlation,targets,state,created_at) VALUES (?1,?2,?3,?4,?5,'pending',?6)",
                params![admission_id, tx.node_id, me.id, &correlation[..], &[0u8; 32][..], tx.now],
            )?;
            let mut created = Vec::new();
            for plan in plans {
                created.extend(crate::holds::guard_take_pending_tx(tx, &me, plan, &admission_id, reason)?);
            }
            let target_hash = admission_target_hash(&key, &admission_id, created.iter().map(|(id, project, path)| (id.as_str(), project.as_str(), path.as_str())).collect())?;
            tx.execute("UPDATE admissions SET targets=?2 WHERE admission_id=?1", params![admission_id, &target_hash[..]])?;
            Ok(EditAdmission { agent, conflicts: Vec::new(), unregistered: Vec::new(), unguarded: Vec::new() })
        })
    }

    /// Admit one edit atomically: in a single immediate transaction, under one deadline,
    /// resolve the existing session actor (never creating one), map the edited files to
    /// registered projects, check every file against other agents' holds, and only when
    /// nothing conflicts take the session's holds on all of them. Either every file ends up
    /// held or no hold is taken; a conflict leaves only its `guard.blocked` evidence.
    ///
    /// Coverage is fresh for each admitted edit: new holds start with a full TTL and every
    /// live exclusive hold the session already has in the edited projects is renewed to its
    /// full TTL in the same transaction. The commit is fenced like a new binding's, by its
    /// last event ID (see [`Store::reconcile_identity_operations`]).
    ///
    /// `group` receives the registered local projects, read inside the transaction, and
    /// returns the edit's [`EditScope`]. It must not write to the store. Admission creates
    /// no project: a group whose directory is not registered is reported in
    /// [`EditAdmission::unregistered`]. An edit with both guarded files and targets outside
    /// every project is stopped and reported in [`EditAdmission::unguarded`]; an edit with
    /// only such targets is admitted with nothing held.
    ///
    /// Any error means the edit is not protected and the caller must stop it.
    #[allow(clippy::too_many_arguments)]
    pub fn admit_edit(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_actor: Option<(&str, bool)>,
        session_id: &str,
        reason: &str,
        group: impl FnOnce(&[Project]) -> EditScope,
    ) -> Result<EditAdmission> {
        {
            validate_session_id(session_id)?;
            self.write_identity(|tx, deadline| {
                let (agent, _) = resolve_session_actor_tx(
                    tx,
                    deadline,
                    client,
                    program,
                    root,
                    requested_actor,
                    session_id,
                )
                .map_err(|e| match e {
                    Error::Invalid(_)
                    | Error::Forbidden(_)
                    | Error::NotFound(_)
                    | Error::Ambiguous(_) => Error::IdentityUnavailable(
                        "the session binding cannot be resolved; check the session address, base and project, then restart or reconcile the session".into(),
                    ),
                    other => other,
                })?;
                let me = resolve_agent(tx, tx.node_id, &agent)?;
                // Refresh the session project's repository link at admission, never on a
                // passive lifecycle event. The project exists: the binding names it.
                crate::repo::refresh_link(tx, &find_project(tx, tx.node_id, root)?)?;
                let projects = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT {PROJECT_COLUMNS} FROM projects WHERE node_id = ?1 ORDER BY slug"
                    ))?;
                    let rows = stmt.query_map([tx.node_id], project_from_row)?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let EditScope { groups, unguarded } = group(&projects);
                if std::time::Instant::now() >= deadline {
                    return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
                }
                // A target outside every project cannot be held. Alone that is an edit
                // nobody guards; next to guarded files it would be a partly protected
                // edit reported as protected, so the whole edit is stopped.
                if !groups.is_empty() && !unguarded.is_empty() {
                    return Ok(EditAdmission {
                        agent,
                        conflicts: Vec::new(),
                        unregistered: Vec::new(),
                        unguarded,
                    });
                }
                if groups.len() > MAX_ADMISSION_GROUPS
                    || groups.iter().map(|(_, files)| files.len()).sum::<usize>()
                        > MAX_ADMISSION_FILES
                {
                    return Err(Error::Invalid(format!(
                        "one edit may name at most {MAX_ADMISSION_FILES} files in {MAX_ADMISSION_GROUPS} projects"
                    )));
                }
                // Decide first, for every project, without writing any hold.
                let mut plans = Vec::new();
                let mut project_ids = Vec::new();
                let mut unregistered = Vec::new();
                for (directory, files) in &groups {
                    match find_project(tx, tx.node_id, directory) {
                        Ok(project) => {
                            project_ids.push(project.project_id.clone());
                            plans.push(crate::holds::guard_plan_tx(tx, &me, project, files)?)
                        }
                        Err(Error::NotFound(_)) => unregistered.push(directory.clone()),
                        Err(e) => return Err(e),
                    }
                }
                if !unregistered.is_empty() {
                    return Ok(EditAdmission {
                        agent,
                        conflicts: Vec::new(),
                        unregistered,
                        unguarded: Vec::new(),
                    });
                }
                if plans.iter().any(|plan| !plan.conflicts.is_empty()) {
                    let mut conflicts = Vec::new();
                    for plan in plans {
                        if !plan.conflicts.is_empty() {
                            conflicts
                                .extend(crate::holds::guard_blocked_tx(tx, &me, plan)?.conflicts);
                        }
                    }
                    return Ok(EditAdmission {
                        agent,
                        conflicts,
                        unregistered,
                        unguarded: Vec::new(),
                    });
                }
                for plan in plans {
                    crate::holds::guard_take_tx(tx, &me, plan, reason)?;
                }
                // Fresh coverage for this operation: an own hold that already covered a
                // file was left as it was above unless past half its TTL. Renew every live
                // exclusive hold of this session in the edited projects that this
                // transaction has not just written, so the edit never starts on coverage
                // that is about to lapse. (Kept here rather than in `guard_take_tx` so the
                // manual `guard_edit` callers keep their renewal rule.)
                let mut stale: Vec<String> = Vec::new();
                for project_id in &project_ids {
                    let mut stmt = tx.prepare(
                        "SELECT hold_id FROM holds
                         WHERE agent_id = ?1 AND project_id = ?2 AND exclusive = 1
                           AND node_id = ?4
                           AND released_at IS NULL AND expires_at > ?3
                           AND expires_at < ?3 + ttl_seconds * 1000
                         ORDER BY created_at",
                    )?;
                    let ids = stmt
                        .query_map(params![me.id, project_id, tx.now, tx.node_id], |r| {
                            r.get(0)
                        })?
                        .collect::<rusqlite::Result<Vec<String>>>()?;
                    for id in ids {
                        if !stale.contains(&id) {
                            stale.push(id);
                        }
                    }
                }
                if !stale.is_empty() {
                    tx.emit(&me.address, "hold.renewed", json!({ "hold_ids": stale }))?;
                    for id in &stale {
                        tx.execute(
                            "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ttl_seconds * 1000
                             WHERE hold_id = ?1",
                            params![id, tx.now],
                        )?;
                    }
                }
                Ok(EditAdmission {
                    agent,
                    conflicts: Vec::new(),
                    unregistered,
                    unguarded: Vec::new(),
                })
            })
        }
    }

    /// Resolve a client hook's per-session actor without storing its raw session ID.
    /// `requested_actor` is absent for the client default, or contains an explicit
    /// actor and its provenance: `false` means a base, `true` an exact session address.
    /// The tuple is required; this API does not infer provenance from existing bindings.
    /// The earlier unmerged identity branch accepted `Option<&str>`; callers of that
    /// branch must supply provenance explicitly. This method did not exist on main.
    pub fn session_actor(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_actor: Option<(&str, bool)>,
        session_id: &str,
        create: bool,
    ) -> Result<(String, String)> {
        {
            validate_session_id(session_id)?;
            self.write_identity(|tx, deadline| {
                if create {
                    session_actor_tx(
                        tx,
                        deadline,
                        client,
                        program,
                        root,
                        session_id,
                        SessionResolution {
                            requested_actor,
                            create: true,
                        },
                    )
                } else {
                    resolve_session_actor_tx(
                        tx,
                        deadline,
                        client,
                        program,
                        root,
                        requested_actor,
                        session_id,
                    )
                }
            })
        }
    }

    /// Check that an address identifies a locally registered session actor.
    pub fn is_session_actor(&self, who: &str) -> Result<bool> {
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

    #[cfg(not(windows))]
    #[test]
    fn borrowed_session_resolution_sees_uncommitted_binding_and_rolls_back() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let before = store
            .read(|conn| {
                Ok((
                    conn.query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))?,
                    conn.query_row("SELECT count(*) FROM agents", [], |r| r.get::<_, i64>(0))?,
                    conn.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))?,
                ))
            })
            .unwrap();
        let result: Result<()> = store.write_identity(|tx, deadline| {
            let created = session_actor_tx(
                tx,
                deadline,
                "claude",
                Program::Claude,
                root,
                "private-session",
                SessionResolution {
                    requested_actor: None,
                    create: true,
                },
            )?;
            let changes_before: i64 = tx.query_row("SELECT total_changes()", [], |r| r.get(0))?;
            let by_base = resolve_session_actor_tx(
                tx,
                deadline,
                "claude",
                Program::Claude,
                root,
                None,
                "private-session",
            )?;
            let by_exact = resolve_session_actor_tx(
                tx,
                deadline,
                "claude",
                Program::Claude,
                root,
                Some((&created.0, true)),
                "private-session",
            )?;
            assert_eq!(created, by_base);
            assert_eq!(created, by_exact);
            assert!(matches!(
                resolve_session_actor_tx(
                    tx,
                    deadline,
                    "claude",
                    Program::Claude,
                    root,
                    Some((&created.0, true)),
                    "other-session"
                ),
                Err(Error::Invalid(_))
            ));
            assert!(matches!(
                resolve_session_actor_tx(
                    tx,
                    deadline,
                    "claude",
                    Program::Claude,
                    root,
                    None,
                    "other-session"
                ),
                Err(Error::NotFound(_))
            ));
            let changes_after: i64 = tx.query_row("SELECT total_changes()", [], |r| r.get(0))?;
            assert_eq!(changes_before, changes_after);
            Err(Error::Invalid("test rollback".into()))
        });
        assert!(matches!(result, Err(Error::Invalid(ref s)) if s == "test rollback"));
        let after = store
            .read(|conn| {
                Ok((
                    conn.query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))?,
                    conn.query_row("SELECT count(*) FROM agents", [], |r| r.get::<_, i64>(0))?,
                    conn.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))?,
                ))
            })
            .unwrap();
        assert_eq!(before, after);
        assert!(matches!(
            store.session_actor(
                "claude",
                Program::Claude,
                root,
                None,
                "private-session",
                false
            ),
            Err(Error::NotFound(_))
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn session_resolution_contends_across_processes_with_parent_timeout() {
        use std::time::{Duration, Instant};
        struct ChildGuard {
            child: std::process::Child,
            fixture: Option<tempfile::TempDir>,
            reaped: bool,
        }
        impl ChildGuard {
            fn path(&self) -> &std::path::Path {
                self.fixture.as_ref().unwrap().path()
            }

            fn stop_and_reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
                if let Some(status) = self.child.try_wait()? {
                    self.reaped = true;
                    return Ok(status);
                }
                std::fs::write(self.path().join("cancel"), b"cancel")?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = self.child.try_wait()? {
                        self.reaped = true;
                        return Ok(status);
                    }
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "cooperative child exit not verified",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.reaped {
                    return;
                }
                if let Err(error) = self.stop_and_reap() {
                    let path = self.fixture.take().unwrap().keep();
                    eprintln!(
                        "owned child {} exit not verified: {}; preserved fixture {}",
                        self.child.id(),
                        error,
                        path.display()
                    );
                }
            }
        }
        if let Some(fixture) = std::env::var_os("ANOTHER_RESOLVER_CONTENTION_FIXTURE") {
            let fixture = std::path::PathBuf::from(fixture);
            let store = Store::open(&fixture.join("home")).unwrap();
            std::fs::write(fixture.join("ready"), b"ready").unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            while !fixture.join("go").exists() {
                if fixture.join("cancel").exists() {
                    return;
                }
                assert!(Instant::now() < end, "parent did not signal");
                std::thread::sleep(Duration::from_millis(10));
            }
            let root = fixture.join("work");
            assert!(matches!(
                store.session_actor(
                    "claude",
                    Program::Claude,
                    root.to_str().unwrap(),
                    None,
                    "session",
                    false
                ),
                Err(Error::IdentityScanBudgetExceeded(_))
            ));
            return;
        }

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("work");
        std::fs::create_dir(&root).unwrap();
        let store = Store::open(&fixture.path().join("home")).unwrap();
        let expected = store
            .session_actor(
                "claude",
                Program::Claude,
                root.to_str().unwrap(),
                None,
                "session",
                true,
            )
            .unwrap();
        let child_process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "identity::tests::session_resolution_contends_across_processes_with_parent_timeout",
                "--nocapture",
            ])
            .env("ANOTHER_RESOLVER_CONTENTION_FIXTURE", fixture.path())
            .spawn()
            .unwrap();
        let mut child = ChildGuard {
            child: child_process,
            fixture: Some(fixture),
            reaped: false,
        };
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while !child.path().join("ready").exists() {
            if Instant::now() >= ready_deadline {
                panic!("child did not become ready");
            }
            assert!(
                child.child.try_wait().unwrap().is_none(),
                "child exited before ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        store
            .write(|_tx| {
                std::fs::write(child.path().join("go"), b"go").unwrap();
                let deadline = Instant::now() + Duration::from_secs(8);
                loop {
                    if let Some(status) = child.child.try_wait().unwrap() {
                        child.reaped = true;
                        assert!(status.success(), "child failed: {status}");
                        println!("contention child exited and was reaped");
                        break;
                    }
                    if Instant::now() >= deadline {
                        panic!("child exceeded parent timeout");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .session_actor(
                    "claude",
                    Program::Claude,
                    root.to_str().unwrap(),
                    None,
                    "session",
                    false
                )
                .unwrap(),
            expected
        );
        std::fs::remove_file(child.path().join("ready")).unwrap();
        std::fs::remove_file(child.path().join("go")).unwrap();
        let fixture = child.fixture.take().unwrap();
        drop(child);
        let child_process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "identity::tests::session_resolution_contends_across_processes_with_parent_timeout",
                "--nocapture",
            ])
            .env("ANOTHER_RESOLVER_CONTENTION_FIXTURE", fixture.path())
            .spawn()
            .unwrap();
        let mut cancelled = ChildGuard {
            child: child_process,
            fixture: Some(fixture),
            reaped: false,
        };
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while !cancelled.path().join("ready").exists() {
            assert!(
                Instant::now() < ready_deadline,
                "cancellation child did not become ready"
            );
            assert!(
                cancelled.child.try_wait().unwrap().is_none(),
                "cancellation child exited early"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(cancelled.stop_and_reap().unwrap().success());
        println!("waiting child cooperatively cancelled and was reaped");
    }

    #[test]
    fn interrupted_fetch_reports_elapsed_deadline() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let outcome = store.write_identity(|tx, _| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);
            tx.progress_handler(1, Some(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                true
            }))?;
            let result = binding_scan(tx, deadline);
            tx.progress_handler(0, None::<fn() -> bool>)?;
            assert!(matches!(result, Err(Error::IdentityScanBudgetExceeded(ref reason)) if reason == "deadline"));
            Ok(())
        });
        assert!(outcome.is_ok());
    }

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
            .session_actor(
                "claude",
                Program::Claude,
                root,
                Some((&a, true)),
                raw,
                false,
            )
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
                Some((&a, true)),
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

    #[cfg(not(windows))]
    #[test]
    fn bound_session_lookup_resolves_only_exact_local_session_actors_and_creates_nothing() {
        let store = Store::open_in_memory(crate::store::Clock::System).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let (actor, _) = store
            .session_actor("claude", Program::Claude, root, None, "one", true)
            .unwrap();
        let plain = store
            .register_agent(RegisterAgent {
                name: "plain".into(),
                program: Program::Claude,
                project: Some(root.into()),
                model: None,
                description: None,
            })
            .unwrap();
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO nodes (node_id,display_name,role,is_local,created_at) VALUES ('peer-node','peer','home',0,?1)",
                    params![tx.now],
                )?;
                tx.execute(
                    "INSERT INTO agents (agent_id,node_id,project_id,name,program,status,created_at,updated_at,last_seen_at,event_id,address) VALUES ('peer-agent','peer-node',NULL,'ghost','claude','active',?1,?1,?1,'peer-event','ghost@@peer')",
                    params![tx.now],
                )?;
                Ok(())
            })
            .unwrap();
        let counts = || {
            store
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM agents),(SELECT count(*) FROM projects)",
                        [],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
                    )?)
                })
                .unwrap()
        };
        let before = counts();
        let lookup = |who: &str| {
            store.write_identity(|tx, deadline| resolve_bound_session_tx(tx, deadline, who))
        };

        let found = lookup(&actor).unwrap();
        assert_eq!(found.address, actor);
        assert_eq!(found.id, store.agent(&actor).unwrap().agent_id);
        assert_eq!(found.node_id, store.node_id());

        let (name, slug) = actor.split_once('@').unwrap();
        assert!(matches!(lookup(name), Err(Error::Invalid(_))));
        assert!(matches!(
            lookup(&format!("{name}@{root}")),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            lookup(&format!("{name}@{slug}@peer")),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            lookup(&format!(" {actor}")),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(lookup(&plain.address), Err(Error::Invalid(_))));
        assert!(matches!(
            lookup(&format!("nobody@{slug}")),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(lookup("ghost@peer"), Err(Error::Forbidden(_))));
        assert_eq!(counts(), before);

        store
            .write(|tx| {
                tx.execute(
                    "UPDATE events SET payload='{}' WHERE kind='session.binding.v1'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(lookup(&actor), Err(Error::Invalid(_))));
        assert_eq!(counts(), before);
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
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = a.join("b");
        let c = a.join("c");
        let b_normalized = normalize_project_path(&b.to_string_lossy()).unwrap();
        let c_normalized = normalize_project_path(&c.to_string_lossy()).unwrap();
        assert_eq!(
            normalize_project_path(&format!("{}/", b.display())).unwrap(),
            b_normalized
        );
        assert_eq!(
            normalize_project_path(&a.join("./b/../c").to_string_lossy()).unwrap(),
            c_normalized
        );
        assert!(normalize_project_path("rel/path").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_drive_prefix_keeps_the_same_project_key() {
        assert_eq!(
            normalize_project_path(r"\\?\C:\work\project").unwrap(),
            normalize_project_path(r"C:\work\project").unwrap()
        );
    }

    #[test]
    fn slugs() {
        assert_eq!(slug_base("/work/My Repo"), "my-repo");
        assert_eq!(slug_base("/"), "root");
    }
}
