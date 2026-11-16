//! Project notes (D70): short, durable facts an agent leaves for a project ("the e2e
//! tests need the dev server", "schema.rs is generated"). Every local agent working in the
//! project or a sibling checkout of its repository sees them.
//!
//! Notes are node-local. Their text lives only in the `notes` table: events carry the
//! note id, project, key, size and (for `note.set`) a SHA-256 of the text written, never
//! the text, and are never shared with peers. Forgetting or replacing a note overwrites its
//! text in the same transaction. An expired note is never listed, and its text is erased by
//! the next note write or sweep.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, Project};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};

/// Largest note text, in bytes.
pub const MAX_NOTE_BYTES: usize = 4096;
/// Most live notes one project may hold.
pub const MAX_LIVE_NOTES: usize = 200;
/// Most notes one list returns.
pub const MAX_NOTE_LIST: u32 = 50;
const MAX_KEY_CHARS: usize = 100;
const MIN_TTL_SECONDS: i64 = 60;
const MAX_TTL_SECONDS: i64 = 365 * 24 * 3600;

/// A note to write. With a `key`, it replaces the project's live note with that key.
#[derive(Debug, Clone, Default)]
pub struct NewNote {
    /// The writing agent (name or name@project).
    pub agent: String,
    /// Project absolute path or slug.
    pub project: String,
    pub key: Option<String>,
    pub text: String,
    /// Forget the note automatically after this many seconds (60 s to 365 days).
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Note {
    pub note_id: String,
    /// Slug of the checkout the note was written in.
    pub project: String,
    pub key: Option<String>,
    pub text: String,
    /// The agent that wrote the current text.
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
}

/// Live notes of a project and its sibling checkouts, newest first.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NoteList {
    pub project: String,
    pub notes: Vec<Note>,
    /// Live notes in all, including any beyond the list's limit.
    pub total: usize,
}

/// Which note to forget.
#[derive(Debug, Clone)]
pub enum NoteTarget {
    Id(String),
    Key { project: String, key: String },
}

/// What a forgotten note was. Its text is gone.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ForgottenNote {
    pub note_id: String,
    pub project: String,
    pub key: Option<String>,
    pub bytes: usize,
}

fn validate(note: &NewNote) -> Result<()> {
    if note.text.trim().is_empty() {
        return Err(Error::Invalid("a note needs text".into()));
    }
    if note.text.len() > MAX_NOTE_BYTES {
        return Err(Error::Invalid(format!(
            "a note is at most {MAX_NOTE_BYTES} bytes (this one is {}); keep notes to short facts",
            note.text.len()
        )));
    }
    if let Some(key) = &note.key {
        if key.trim().is_empty()
            || key.trim() != key
            || key.chars().count() > MAX_KEY_CHARS
            || key.chars().any(char::is_control)
        {
            return Err(Error::Invalid(format!(
                "a note key is 1-{MAX_KEY_CHARS} printable characters without surrounding spaces"
            )));
        }
    }
    if let Some(ttl) = note.ttl_seconds {
        if !(MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(&ttl) {
            return Err(Error::Invalid(format!(
                "a note's ttl is {MIN_TTL_SECONDS} seconds to 365 days"
            )));
        }
    }
    Ok(())
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

const NOTE_SELECT: &str = "SELECT n.note_id, p.slug, n.key, n.text, a.name, ap.slug,
        n.created_at, n.updated_at, n.expires_at
    FROM notes n
    JOIN projects p ON p.project_id = n.project_id
    JOIN agents a ON a.agent_id = n.author_id
    LEFT JOIN projects ap ON ap.project_id = a.project_id";

fn note_from_row(r: &Row<'_>) -> rusqlite::Result<Note> {
    let name: String = r.get(4)?;
    let home: Option<String> = r.get(5)?;
    Ok(Note {
        note_id: r.get(0)?,
        project: r.get(1)?,
        key: r.get(2)?,
        text: r.get(3)?,
        author: address(&name, home.as_deref()),
        created_at: fmt_ts(r.get(6)?),
        updated_at: fmt_ts(r.get(7)?),
        expires_at: fmt_opt(r.get(8)?),
    })
}

/// Live, unexpired notes of `project_id` only.
const LIVE: &str = "n.deleted_at IS NULL AND (n.expires_at IS NULL OR n.expires_at > ?2)";

/// The project ids a list for `project` covers: the project and every sibling checkout of
/// the same repository at the same path in it. Read from the repository links cached when
/// projects are registered, so listing writes nothing.
fn scope_ids(conn: &Connection, project: &Project) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT o.project_id FROM project_repos me
         JOIN project_repos o ON o.repo_key = me.repo_key AND o.repo_prefix = me.repo_prefix
         JOIN projects p ON p.project_id = o.project_id AND p.node_id = ?2
         WHERE me.project_id = ?1 AND me.repo_key != ''",
    )?;
    let mut ids: Vec<String> = stmt
        .query_map(params![project.project_id, project.node_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !ids.contains(&project.project_id) {
        ids.push(project.project_id.clone());
    }
    Ok(ids)
}

fn list_in(conn: &Connection, project: &Project, now: i64, limit: u32) -> Result<NoteList> {
    let ids = scope_ids(conn, project)?;
    let ids_json = serde_json::to_string(&ids).map_err(|e| Error::Invalid(e.to_string()))?;
    let scope = "n.project_id IN (SELECT value FROM json_each(?1))";
    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM notes n WHERE {scope} AND {LIVE}"),
        params![ids_json, now],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "{NOTE_SELECT} WHERE {scope} AND {LIVE}
         ORDER BY n.updated_at DESC, n.note_id DESC LIMIT ?3"
    ))?;
    let notes = stmt
        .query_map(
            params![ids_json, now, limit.clamp(1, MAX_NOTE_LIST)],
            note_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(NoteList {
        project: project.slug.clone(),
        notes,
        total: total as usize,
    })
}

/// Erase every note whose expiry has passed. Every note write runs this first, and so does
/// the sweep.
pub(crate) fn expire_notes_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT note_id, project_id FROM notes
         WHERE node_id = ?1 AND deleted_at IS NULL AND expires_at IS NOT NULL AND expires_at <= ?2",
    )?;
    let expired = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (note_id, project_id) in &expired {
        tx.emit(
            "system",
            "note.expired",
            json!({ "note_id": note_id, "project_id": project_id }),
        )?;
        tx.execute(
            "UPDATE notes SET text = '', deleted_at = ?2 WHERE note_id = ?1",
            params![note_id, tx.now],
        )?;
    }
    Ok(expired.len())
}

impl Store {
    /// Write a note. With a key, the project's live note under that key is replaced in place
    /// (its old text is overwritten); an expired note under the key is replaced too.
    pub fn set_note(&self, note: NewNote) -> Result<Note> {
        validate(&note)?;
        self.write(|tx| {
            expire_notes_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &note.agent)?;
            let project = find_project(tx, tx.node_id, &note.project)?;
            let expires_at = note.ttl_seconds.map(|ttl| tx.now + ttl * 1000);
            let existing: Option<String> = match &note.key {
                Some(key) => tx
                    .query_row(
                        "SELECT note_id FROM notes
                         WHERE project_id = ?1 AND key = ?2 AND deleted_at IS NULL",
                        params![project.project_id, key],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            };
            if existing.is_none() {
                let live: i64 = tx.query_row(
                    "SELECT count(*) FROM notes n WHERE n.project_id = ?1 AND n.deleted_at IS NULL
                       AND (n.expires_at IS NULL OR n.expires_at > ?2)",
                    params![project.project_id, tx.now],
                    |r| r.get(0),
                )?;
                if live as usize >= MAX_LIVE_NOTES {
                    return Err(Error::Invalid(format!(
                        "project {} already has {MAX_LIVE_NOTES} notes; forget or replace one (`another note list --project {}`)",
                        project.slug, project.slug
                    )));
                }
            }
            let note_id = existing.clone().unwrap_or_else(new_id);
            let event_id = tx.emit(
                &me.address,
                "note.set",
                json!({
                    "note_id": note_id,
                    "project_id": project.project_id,
                    "key": note.key,
                    "bytes": note.text.len(),
                    "sha256": sha256_hex(&note.text),
                    "expires_at": expires_at,
                    "replaced": existing.is_some(),
                }),
            )?;
            match &existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE notes SET text = ?2, author_id = ?3, updated_at = ?4,
                                expires_at = ?5, event_id = ?6
                         WHERE note_id = ?1",
                        params![id, note.text, me.id, tx.now, expires_at, event_id],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO notes (note_id, node_id, project_id, key, text, author_id,
                                            created_at, updated_at, expires_at, event_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
                        params![
                            note_id,
                            tx.node_id,
                            project.project_id,
                            note.key,
                            note.text,
                            me.id,
                            tx.now,
                            expires_at,
                            event_id
                        ],
                    )?;
                }
            }
            Ok(tx.query_row(
                &format!("{NOTE_SELECT} WHERE n.note_id = ?1"),
                [&note_id],
                note_from_row,
            )?)
        })
    }

    /// Live notes of a project and its sibling checkouts, newest first. Writes nothing.
    pub fn notes(&self, project: &str, limit: u32) -> Result<NoteList> {
        let now = self.now();
        self.read(|conn| {
            let project = find_project(conn, self.node_id(), project)?;
            list_in(conn, &project, now, limit)
        })
    }

    /// The notes for an agent's own project, for its session-start digest. `None` when the
    /// agent belongs to no project. Writes nothing.
    pub fn notes_for_agent(&self, agent: &str, limit: u32) -> Result<Option<NoteList>> {
        let now = self.now();
        self.read(|conn| {
            let me = resolve_agent(conn, self.node_id(), agent)?;
            let slug: Option<String> = conn.query_row(
                "SELECT p.slug FROM agents a LEFT JOIN projects p ON p.project_id = a.project_id
                 WHERE a.agent_id = ?1",
                [&me.id],
                |r| r.get(0),
            )?;
            let Some(slug) = slug else {
                return Ok(None);
            };
            let project = find_project(conn, self.node_id(), &slug)?;
            Ok(Some(list_in(conn, &project, now, limit)?))
        })
    }

    /// Forget a note: its text is overwritten in the same transaction that records it.
    pub fn forget_note(&self, agent: &str, target: NoteTarget) -> Result<ForgottenNote> {
        self.write(|tx| {
            expire_notes_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let found: Option<(String, String, String, Option<String>, String)> = match &target {
                NoteTarget::Id(id) => tx
                    .query_row(
                        "SELECT n.note_id, n.project_id, p.slug, n.key, n.text FROM notes n
                         JOIN projects p ON p.project_id = n.project_id
                         WHERE n.note_id = ?1 AND n.node_id = ?2 AND n.deleted_at IS NULL",
                        params![id, tx.node_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?,
                NoteTarget::Key { project, key } => {
                    let project = find_project(tx, tx.node_id, project)?;
                    tx.query_row(
                        "SELECT n.note_id, n.project_id, p.slug, n.key, n.text FROM notes n
                         JOIN projects p ON p.project_id = n.project_id
                         WHERE n.project_id = ?1 AND n.key = ?2 AND n.deleted_at IS NULL",
                        params![project.project_id, key],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?
                }
            };
            let Some((note_id, project_id, slug, key, text)) = found else {
                return Err(Error::NotFound(
                    "no such note; `another note list` shows the live ones".into(),
                ));
            };
            tx.emit(
                &me.address,
                "note.forgotten",
                json!({
                    "note_id": note_id,
                    "project_id": project_id,
                    "key": key,
                    "bytes": text.len(),
                }),
            )?;
            tx.execute(
                "UPDATE notes SET text = '', deleted_at = ?2 WHERE note_id = ?1",
                params![note_id, tx.now],
            )?;
            Ok(ForgottenNote {
                note_id,
                project: slug,
                key,
                bytes: text.len(),
            })
        })
    }
}
