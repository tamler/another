//! What a remote token may see of this node's holds (FLE-23 M-H, owner decision Q2: "If it
//! can't and shouldn't know about another project... it shouldn't know").
//!
//! One rule, used by the hold list, the activity feed and its summary, the confined work
//! board, and the text of refusals and warnings returned to a remote caller. A hold is shown
//! when, in checkout-relative form, its pattern overlaps the whole tree of some local project
//! the token's globs allow, on that project's volume rules for letter case. A pattern that
//! cannot be read stands for every path of its project. Holds of other nodes are never shown.
//!
//! Conflict checks never use this: acquire, check and the edit guard compare against every
//! hold of the checkout, and only what is returned to the remote caller changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::error::Result;
use crate::holds::{folds_case, overlap_on_volume, pattern_unreadable, Hold, HoldConflict};
use crate::repo::in_repo;
use crate::tokens::{scope_allows_project, TokenScope};

/// What a remote caller is told in place of a blocking hold it may not see.
pub(crate) const HIDDEN_HOLD: &str = "the requested path is held by another agent in this checkout";
/// What a remote caller is told in place of another node's overlapping hold.
pub(crate) const HIDDEN_FOREIGN_HOLD: &str = "the requested path overlaps a hold on another node";

/// A project's place in its checkout, from the link cache: `(checkout, prefix)`.
type Link = Option<(String, String)>;

/// One local project the token's globs allow.
struct Tree {
    project_id: String,
    link: Link,
    fold: bool,
}

/// What one token may see of this node's holds.
pub(crate) struct HoldView {
    local: String,
    trees: Vec<Tree>,
    /// Per project id: `None` when the project is unknown or on another node.
    places: RefCell<HashMap<String, Option<Link>>>,
}

/// The cached link of `project_id`, only when it is current and names a checkout. The cache
/// is read as it is and never refreshed here.
fn cached_link(conn: &Connection, project_id: &str) -> Result<Link> {
    Ok(crate::repo::cached(conn, project_id)?
        .flatten()
        .filter(|l| !l.checkout.is_empty())
        .map(|l| (l.checkout, l.prefix)))
}

impl HoldView {
    /// The local projects `scope` allows, each with its cached checkout link and whether its
    /// volume folds letter case.
    pub(crate) fn of(conn: &Connection, local: &str, scope: &TokenScope) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT project_id, abs_path FROM projects WHERE node_id = ?1 ORDER BY project_id",
        )?;
        let projects = stmt
            .query_map([local], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut trees = Vec::new();
        for (project_id, abs_path) in projects {
            if !scope_allows_project(conn, scope, &project_id)? {
                continue;
            }
            trees.push(Tree {
                link: cached_link(conn, &project_id)?,
                fold: folds_case(Path::new(&abs_path)),
                project_id,
            });
        }
        Ok(HoldView {
            local: local.to_string(),
            trees,
            places: RefCell::new(HashMap::new()),
        })
    }

    /// The link of a local project; `None` for an unknown project or one on another node.
    fn place(&self, conn: &Connection, project_id: &str) -> Result<Option<Link>> {
        if let Some(found) = self.places.borrow().get(project_id) {
            return Ok(found.clone());
        }
        let node: Option<String> = conn
            .query_row(
                "SELECT node_id FROM projects WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .optional()?;
        let found = match node {
            Some(node) if node == self.local => Some(cached_link(conn, project_id)?),
            _ => None,
        };
        self.places
            .borrow_mut()
            .insert(project_id.to_string(), found.clone());
        Ok(found)
    }

    /// Whether a hold on `pattern` (relative to project `project_id`) is shown.
    pub(crate) fn shows(&self, conn: &Connection, project_id: &str, pattern: &str) -> Result<bool> {
        let Some(link) = self.place(conn, project_id)? else {
            return Ok(false);
        };
        let pattern = if pattern_unreadable(pattern) {
            "**"
        } else {
            pattern
        };
        for tree in &self.trees {
            // Within one project both sides are project-relative: this is how a project with
            // no repository link is judged.
            if tree.project_id == project_id && overlap_on_volume(pattern, "**", tree.fold) {
                return Ok(true);
            }
            if let (Some((checkout, prefix)), Some((tree_checkout, tree_prefix))) =
                (&link, &tree.link)
            {
                if checkout == tree_checkout
                    && overlap_on_volume(
                        &in_repo(prefix, pattern),
                        &in_repo(tree_prefix, "**"),
                        tree.fold,
                    )
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// [`HoldView::shows`] for a stored hold, by id. Released and expired rows count, for the
    /// feed; an unknown id and a hold of another node are hidden.
    pub(crate) fn shows_hold(&self, conn: &Connection, hold_id: &str) -> Result<bool> {
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT node_id, project_id, pattern FROM holds WHERE hold_id = ?1",
                [hold_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            Some((node, project_id, pattern)) if node == self.local => {
                self.shows(conn, &project_id, &pattern)
            }
            _ => Ok(false),
        }
    }

    /// `conflict` as a remote caller may see it: unchanged when its hold is shown, otherwise
    /// with every name dropped and `text` as the only explanation.
    pub(crate) fn conflict(
        &self,
        conn: &Connection,
        conflict: HoldConflict,
        text: &str,
    ) -> Result<HoldConflict> {
        Ok(if self.shows_hold(conn, &conflict.held.hold_id)? {
            conflict
        } else {
            redact(conflict, text)
        })
    }

    /// A hold event's payload as a remote caller may see it, or `None` when the event is
    /// hidden. Every `hold.*` kind goes through here; one this build does not know, or a
    /// known one whose payload is not the shape its producer writes, is hidden.
    pub(crate) fn hold_event(
        &self,
        conn: &Connection,
        node_id: &str,
        kind: &str,
        payload: &str,
    ) -> Result<Option<String>> {
        if node_id != self.local {
            return Ok(None);
        }
        let Ok(Value::Object(mut p)) = serde_json::from_str::<Value>(payload) else {
            return Ok(None);
        };
        let text = |p: &serde_json::Map<String, Value>, k: &str| -> Option<String> {
            p.get(k).and_then(Value::as_str).map(str::to_string)
        };
        let ids = |p: &serde_json::Map<String, Value>, k: &str| -> Option<Vec<String>> {
            p.get(k)?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        let kept = match kind {
            "hold.acquired" | "hold.updated" => {
                let (Some(project_id), Some(pattern)) = (text(&p, "project_id"), text(&p, "pattern"))
                else {
                    return Ok(None);
                };
                self.shows(conn, &project_id, &pattern)?
            }
            "hold.expired" => {
                let Some(hold_id) = text(&p, "hold_id") else {
                    return Ok(None);
                };
                self.shows_hold(conn, &hold_id)?
            }
            "hold.renewed" | "hold.released" => {
                let Some(all) = ids(&p, "hold_ids") else {
                    return Ok(None);
                };
                let mut shown = Vec::new();
                for id in all {
                    if self.shows_hold(conn, &id)? {
                        shown.push(Value::String(id));
                    }
                }
                let any = !shown.is_empty();
                p.insert("hold_ids".into(), Value::Array(shown));
                any
            }
            "hold.blocked" => return self.blocked_event(conn, p).map(|p| p.map(|p| p.to_string())),
            "hold.waiting" | "hold.wait.rechecked" | "hold.available" | "hold.recheck_failed"
            | "hold.wait_failed" | "hold.notify_failed" | "hold.wait.finished" => {
                let (Some(project_id), Some(path)) = (text(&p, "project_id"), text(&p, "path"))
                else {
                    return Ok(None);
                };
                if !self.shows(conn, &project_id, &globset::escape(&path))? {
                    return Ok(None);
                }
                if p.contains_key("blockers") {
                    let Some(all) = ids(&p, "blockers") else {
                        return Ok(None);
                    };
                    let mut shown = Vec::new();
                    for id in all {
                        if self.shows_hold(conn, &id)? {
                            shown.push(Value::String(id));
                        }
                    }
                    p.insert("blockers".into(), Value::Array(shown));
                }
                // Error text quotes whatever failed, which may name other projects.
                p.remove("error");
                true
            }
            _ => false,
        };
        Ok(kept.then(|| Value::Object(p).to_string()))
    }

    /// A stopped edit: kept when one of its conflicting paths is shown, with only those
    /// conflicts, each naming its holder only when that hold is shown.
    fn blocked_event(
        &self,
        conn: &Connection,
        mut p: serde_json::Map<String, Value>,
    ) -> Result<Option<Value>> {
        let Some(project_id) = p.get("project_id").and_then(Value::as_str).map(str::to_string)
        else {
            return Ok(None);
        };
        let Some(conflicts) = p.get("conflicts").and_then(Value::as_array).cloned() else {
            return Ok(None);
        };
        let mut kept = Vec::new();
        for c in conflicts {
            let Value::Object(mut c) = c else {
                return Ok(None);
            };
            let (Some(path), Some(hold_id)) = (
                c.get("path").and_then(Value::as_str).map(str::to_string),
                c.get("hold_id").and_then(Value::as_str).map(str::to_string),
            ) else {
                return Ok(None);
            };
            if !self.shows(conn, &project_id, &globset::escape(&path))? {
                continue;
            }
            if !self.shows_hold(conn, &hold_id)? {
                for k in ["holder", "hold_id", "pattern", "project"] {
                    c.remove(k);
                }
            }
            kept.push(Value::Object(c));
        }
        if kept.is_empty() {
            return Ok(None);
        }
        if let Some(paths) = p.get("paths").and_then(Value::as_array).cloned() {
            let mut shown = Vec::new();
            for path in paths {
                let Some(pattern) = path.as_str() else {
                    return Ok(None);
                };
                if self.shows(conn, &project_id, pattern)? {
                    shown.push(path);
                }
            }
            p.insert("paths".into(), Value::Array(shown));
        }
        p.insert("conflicts".into(), Value::Array(kept));
        Ok(Some(Value::Object(p)))
    }
}

/// `conflict` with every name dropped: only the caller's own requested path and `text`.
pub(crate) fn redact(conflict: HoldConflict, text: &str) -> HoldConflict {
    HoldConflict {
        path: conflict.path,
        held: Hold {
            hold_id: String::new(),
            project: String::new(),
            agent: String::new(),
            pattern: String::new(),
            exclusive: conflict.held.exclusive,
            reason: None,
            ttl_seconds: 0,
            created_at: String::new(),
            expires_at: String::new(),
        },
        unreadable: None,
        hidden: Some(text.to_string()),
    }
}
