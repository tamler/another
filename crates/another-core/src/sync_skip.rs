//! An event that is too large to send is skipped, loudly (D-next).
//!
//! The node whose log holds the event (the holder) steps past it when it builds a push
//! batch or a pull page, and writes one small signed `sync.skipped` event (the marker)
//! that names it and never carries its large field. Every sync and `another needs`
//! report the unacknowledged skips of this node's log and of its hub's log.
//! `another sync ack` writes `sync.skip_acked`, which stops the repeating report. Nothing
//! is deleted, and neither kind has a projection.

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::federation::{hub_of, WireEvent, PULL_BODY_LIMIT_BYTES, PUSH_BODY_LIMIT_BYTES};
use crate::mail::{display_line, single_line};
use crate::store::{Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::{insert_audit_tx, AuditRecord};

pub(crate) const KIND_SKIPPED: &str = "sync.skipped";
pub(crate) const KIND_ACKED: &str = "sync.skip_acked";

/// Bounds of a marker's fields, in characters (Unicode scalar values). The holder cuts by
/// them and a receiver refuses a marker over them.
const ID_CHARS: usize = 128;
const KIND_CHARS: usize = 80;
const ACTOR_CHARS: usize = 128;
const ABOUT_CHARS: usize = 120;
const ROUTE_NODES_MAX: usize = 1000;

/// Bounds of a marker's values where they are shown.
const SHOWN_ID_CHARS: usize = 40;
const SHOWN_KIND_CHARS: usize = 80;
const SHOWN_ACTOR_CHARS: usize = 80;
const SHOWN_NODE_CHARS: usize = 40;

/// Skips `another sync` prints in full; the rest are counted.
pub const SYNC_SKIPS_SHOWN: usize = 5;

/// Which transport an event did not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Push,
    Pull,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Push => "push",
            Direction::Pull => "pull",
        }
    }

    fn of(text: &str) -> Direction {
        if text == "pull" {
            Direction::Pull
        } else {
            Direction::Push
        }
    }

    /// `push` or `pull answer`: what the event is larger than one of.
    fn one(self) -> &'static str {
        match self {
            Direction::Push => "push",
            Direction::Pull => "pull answer",
        }
    }

    fn limit(self) -> usize {
        match self {
            Direction::Push => PUSH_BODY_LIMIT_BYTES,
            Direction::Pull => PULL_BODY_LIMIT_BYTES,
        }
    }

    /// What reads the transport, with this build's limit.
    fn reader(self) -> String {
        match self {
            Direction::Push => format!("a hub reads in one push ({} bytes)", self.limit()),
            Direction::Pull => {
                format!("a spoke reads in one pull answer ({} bytes)", self.limit())
            }
        }
    }
}

/// An event a push or pull scan stepped past because it does not fit the transport even
/// alone: what its marker says. The event's large field is never copied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEvent {
    pub event_id: String,
    pub event_node: String,
    pub event_kind: String,
    pub event_actor: String,
    /// [`WireEvent::wire_bytes`] of the event.
    pub bytes: usize,
    /// A short name of the thing, built from small fields only.
    pub about: Option<String>,
    /// `directory`, `feed` or `mail`: how the event would have travelled.
    pub route: String,
    pub route_nodes: Vec<String>,
}

fn cut(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// The id as a marker stores it, so a lookup compares what was written.
fn marker_id(event_id: &str) -> String {
    single_line(event_id, ID_CHARS)
}

fn about(kind: &str, p: &Value) -> Option<String> {
    // A field is cut before it is used, so a planted large value is never copied whole.
    let text = |key: &str| {
        p.get(key)
            .and_then(Value::as_str)
            .map(|t| cut(t, ABOUT_CHARS))
    };
    let count = |key: &str, one: &str, many: &str| {
        p.get(key)
            .and_then(Value::as_array)
            .map(|a| format!("{} {}", a.len(), if a.len() == 1 { one } else { many }))
    };
    let family = kind.split('.').next().unwrap_or_default();
    let about = match (family, kind) {
        (_, "agent.registered") => format!("agent {}", text("name")?),
        (_, "message.sent") => format!(
            "message {} to {}",
            text("message_id")?,
            count("recipients", "recipient", "recipients")?
        ),
        ("hold", _) => match text("hold_id") {
            Some(id) => format!("hold {id}"),
            None => count("hold_ids", "hold", "holds")?,
        },
        ("resource", _) => count("lease_ids", "lease", "leases")?,
        ("request", _) => format!("request {}", text("request_id")?),
        _ => return None,
    };
    Some(single_line(&about, ABOUT_CHARS))
}

impl SkippedEvent {
    /// What a marker for `e` says. `route` and `nodes` are the class and the node ids
    /// `classify` gives the event.
    pub(crate) fn of(e: &WireEvent, payload: &Value, route: &str, nodes: Vec<String>) -> Self {
        let fits =
            nodes.len() <= ROUTE_NODES_MAX && nodes.iter().all(|n| n.chars().count() <= ID_CHARS);
        let (route, route_nodes) = if fits {
            (route.to_string(), nodes)
        } else {
            ("feed".to_string(), Vec::new())
        };
        SkippedEvent {
            event_id: marker_id(&e.event_id),
            event_node: cut(&e.node_id, ID_CHARS),
            event_kind: single_line(&e.kind, KIND_CHARS),
            event_actor: single_line(&e.actor, ACTOR_CHARS),
            bytes: e.wire_bytes(),
            about: about(&e.kind, payload),
            route,
            route_nodes,
        }
    }

    fn payload(&self, direction: Direction) -> Value {
        let mut p = json!({
            "event_id": self.event_id,
            "event_node": self.event_node,
            "event_kind": self.event_kind,
            "event_actor": self.event_actor,
            "bytes": self.bytes,
            "direction": direction.as_str(),
            "route": self.route,
            "route_nodes": self.route_nodes,
        });
        if let Some(about) = &self.about {
            p["about"] = json!(about);
        }
        p
    }
}

fn insert_audit(tx: &Tx<'_>, operation: &str, outcome: &str, detail: String) -> Result<()> {
    insert_audit_tx(
        tx,
        AuditRecord {
            agent: Some(format!("node:{}", tx.node_id)),
            operation: operation.into(),
            outcome: outcome.into(),
            detail: Some(detail),
            ..Default::default()
        },
    )
}

/// The event id a stored marker or acknowledgement names, as SQL over `events.payload`:
/// the text at `event_id`, or NULL for a payload that is not JSON or has no text there. A
/// log can hold a payload of any shape (an earlier build imported these kinds unchecked),
/// and such a row must match nothing instead of failing the query.
const NAMED_EVENT: &str = "CASE WHEN json_valid(payload) THEN
       CASE WHEN json_type(payload, '$.event_id') = 'text'
            THEN json_extract(payload, '$.event_id') END
     END";

/// This node's own marker for `event_id` (as a marker stores it): its event id, time and
/// payload.
fn own_marker(conn: &Connection, node: &str, event_id: &str) -> Result<Option<Marker>> {
    let row: Option<(String, i64, String)> = conn
        .query_row(
            &format!(
                "SELECT event_id, ts, payload FROM events
                 WHERE kind = ?1 AND node_id = ?2 AND {NAMED_EVENT} = ?3
                 ORDER BY seq LIMIT 1"
            ),
            params![KIND_SKIPPED, node, event_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(row.map(|(id, ts, payload)| Marker::of(id, node.to_string(), ts, &payload)))
}

/// When `node` acknowledged its skip of `event_id`, if it did.
fn acked_at(conn: &Connection, node: &str, event_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT ts FROM events
                 WHERE kind = ?1 AND node_id = ?2 AND {NAMED_EVENT} = ?3
                 ORDER BY seq LIMIT 1"
            ),
            params![KIND_ACKED, node, event_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Write the marker and its audit row for each event a scan stepped past, unless this node
/// already has a marker for it. The caller's transaction is the one that moves the cursor
/// past the event (a push) or that runs before the pull is answered, so the lookup and the
/// write cannot be split.
pub(crate) fn mark_tx(tx: &Tx<'_>, skipped: &[SkippedEvent], direction: Direction) -> Result<()> {
    for s in skipped {
        if own_marker(tx, tx.node_id, &s.event_id)?.is_some() {
            continue;
        }
        tx.emit("system", KIND_SKIPPED, s.payload(direction))?;
        insert_audit(
            tx,
            "sync skip",
            "skipped",
            format!(
                "event {} ({}, {} bytes, written by {}) in this node's log was not sent: \
                 larger than one {} ({} bytes)",
                display_line(&s.event_id, SHOWN_ID_CHARS),
                display_line(&s.event_kind, SHOWN_KIND_CHARS),
                s.bytes,
                display_line(&s.event_actor, SHOWN_ACTOR_CHARS),
                direction.one(),
                direction.limit(),
            ),
        )?;
    }
    Ok(())
}

/// The class and node ids a marker or an acknowledgement travels by: those of the event it
/// is about, as its payload states them.
pub(crate) fn route_of(payload: &Value) -> (&str, Vec<String>) {
    let route = payload
        .get("route")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let nodes = payload
        .get("route_nodes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (route, nodes)
}

fn text_field<'a>(kind: &str, p: &'a Value, key: &str, min: usize, max: usize) -> Result<&'a str> {
    let Some(text) = p.get(key).and_then(Value::as_str) else {
        return Err(Error::Invalid(format!("{kind} needs {key} as text")));
    };
    let chars = text.chars().count();
    if chars < min || chars > max {
        return Err(Error::Invalid(format!(
            "{kind} needs {key} of {min} to {max} characters"
        )));
    }
    Ok(text)
}

fn check_route(kind: &str, p: &Value) -> Result<()> {
    let route = text_field(kind, p, "route", 0, ID_CHARS)?;
    if !matches!(route, "directory" | "feed" | "mail") {
        return Err(Error::Invalid(format!(
            "{kind} needs route to be directory, feed or mail"
        )));
    }
    let Some(nodes) = p.get("route_nodes").and_then(Value::as_array) else {
        return Err(Error::Invalid(format!(
            "{kind} needs route_nodes as a list of node ids"
        )));
    };
    let ok = nodes.len() <= ROUTE_NODES_MAX
        && nodes
            .iter()
            .all(|n| n.as_str().is_some_and(|n| n.chars().count() <= ID_CHARS));
    if !ok {
        return Err(Error::Invalid(format!(
            "{kind} needs route_nodes of at most {ROUTE_NODES_MAX} node ids of at most \
             {ID_CHARS} characters"
        )));
    }
    Ok(())
}

/// What a receiver checks of a peer's `sync.skipped`: its shape and lengths, and that its
/// origin may speak about the event's node. A node reports its own events; a hub may also
/// report an event it relays, and a spoke accepts that only from its own hub. That the
/// event exists, and its size, cannot be checked.
pub(crate) fn check_marker(conn: &Connection, origin: &str, p: &Value) -> Result<()> {
    let kind = KIND_SKIPPED;
    text_field(kind, p, "event_id", 1, ID_CHARS)?;
    let event_node = text_field(kind, p, "event_node", 1, ID_CHARS)?;
    text_field(kind, p, "event_kind", 1, KIND_CHARS)?;
    text_field(kind, p, "event_actor", 0, ACTOR_CHARS)?;
    if p.get("bytes").and_then(Value::as_u64).is_none() {
        return Err(Error::Invalid(format!(
            "{kind} needs bytes as a whole number, zero or more"
        )));
    }
    let direction = text_field(kind, p, "direction", 0, ID_CHARS)?;
    if !matches!(direction, "push" | "pull") {
        return Err(Error::Invalid(format!(
            "{kind} needs direction to be push or pull"
        )));
    }
    if p.get("about").is_some() {
        text_field(kind, p, "about", 0, ABOUT_CHARS)?;
    }
    check_route(kind, p)?;
    if event_node != origin {
        let from_hub = hub_of(conn)?.is_some_and(|(hub, _, _)| hub == origin);
        if !from_hub {
            return Err(Error::Forbidden(format!(
                "sync.skipped names an event of node {}; a node reports its own events, and \
                 only this node's hub those of others",
                display_line(event_node, SHOWN_NODE_CHARS)
            )));
        }
    }
    Ok(())
}

/// What a receiver checks of a peer's `sync.skip_acked`: its shape only. It counts for a
/// marker only when both have the same origin node and name the same event.
pub(crate) fn check_ack(p: &Value) -> Result<()> {
    text_field(KIND_ACKED, p, "event_id", 1, ID_CHARS)?;
    check_route(KIND_ACKED, p)
}

/// One stored `sync.skipped` event.
struct Marker {
    marker_id: String,
    holder: String,
    ts: i64,
    event_id: String,
    event_node: String,
    kind: String,
    actor: String,
    bytes: u64,
    direction: Direction,
    about: Option<String>,
    route: String,
    route_nodes: Vec<String>,
}

impl Marker {
    fn of(marker_id: String, holder: String, ts: i64, payload: &str) -> Marker {
        let p: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        let text = |key: &str| {
            p.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (route, route_nodes) = route_of(&p);
        Marker {
            marker_id,
            ts,
            event_id: text("event_id"),
            // A marker without the field is one this node wrote about its own event.
            event_node: match text("event_node") {
                node if node.is_empty() => holder.clone(),
                node => node,
            },
            holder,
            kind: text("event_kind"),
            actor: text("event_actor"),
            bytes: p.get("bytes").and_then(Value::as_u64).unwrap_or_default(),
            direction: Direction::of(&text("direction")),
            about: p.get("about").and_then(Value::as_str).map(str::to_string),
            route: route.to_string(),
            route_nodes,
        }
    }
}

/// An id as this software makes them: a ULID. Only such a value from a marker is put into
/// a printed command; anything else leaves the placeholder, so a peer's text is never an
/// argument of a command the reader copies.
fn usable_id(text: &str) -> bool {
    crate::federation::validate_node_id(text).is_ok()
}

/// An agent name by the local name rule that a command line does not read as an option.
fn usable_name(text: &str) -> bool {
    crate::identity::validate_name(text).is_ok() && !text.starts_with('-')
}

const NAME_NOT_USABLE: &str = "the agent's name in this report is not a valid agent name, \
    so <name> is left for you to fill";
const HOLD_ID_NOT_USABLE: &str = "the hold id in this report is not an id this software \
    makes, so <hold id> is left for you to fill";
const EVENT_ID_NOT_USABLE: &str = "the event id in this report is not an id this software \
    makes, so <event-id> is left for you to fill";
const FOREIGN_MARKER_NAME: &str = "this report is a hub's marker about another node's event, so <name> is left for you to fill; check `another events` before acting";
const FOREIGN_MARKER_HOLD: &str = "this report is a hub's marker about another node's event, so <hold id> is left for you to fill; check `another events` before acting";
const HUB_SYNC_SENTENCE: &str = "spokes receive it at their next sync";

/// `another sync ack <id> --owner`, with the id only when it is usable.
fn ack_command(event_id: &str) -> (String, bool) {
    if usable_id(event_id) {
        (format!("another sync ack {event_id} --owner"), true)
    } else {
        ("another sync ack <event-id> --owner".to_string(), false)
    }
}

const GENERAL_MISSING: &str = "this event and whatever it changed on other nodes; \
    docs/FEDERATION.md, \"What other nodes miss\", says what that is for each kind";
const GENERAL_STEP: &str = "no command re-sends this event. For most kinds nothing needs \
    doing (a release runs out on other nodes by itself, a request only loses a feed line); \
    if other nodes need what it recorded, do that action again";

const AGENT_MISSING: &str = "what this registration changed: the agent's description and \
    model. If it was the agent's first registration, other nodes do not know the agent at \
    all: nobody there can address it; mail it sent them is stored and not delivered; they do \
    not see its holds, so an agent there is not warned before it edits the same files; they \
    do not see its leases, so a resource it owns shows no owner there and cannot be borrowed \
    from another node, and a resource it borrowed on another node is not held for it there";
const AGENT_STEPS: [&str; 6] = [
    "another agent register <name> --program <program> --model <model> --description \
     \"<4096 bytes or fewer>\" (add --project <path> if it has one; a model left out is \
     cleared)",
    "each hold it has (another holds --agent <name> lists them): another hold release \
     --agent <name> <hold id>, then another hold acquire \
     --agent <name> -p <project> <the same paths>, with --shared, --repo-wide, --reason and \
     --ttl as the hold has them (a fresh checkout is sent; without -p from another \
     directory it is a different hold, and without --shared a shared hold becomes exclusive \
     or is refused)",
    "each resource of this node it holds a lease on: another release <resource id or \
     kind:name> --agent <name>, then at once another borrow <the same> --agent <name> (a \
     borrow without the release only renews, and a renewal brings nothing to a node that \
     never had the lease. Between the two commands the resource has no lease unless another \
     agent holds one: another agent on this node that borrows it in that moment becomes its \
     owner, and if the resource is exclusive this agent's borrow is then refused. If another \
     agent already holds a lease, that agent becomes the owner at the release and this agent \
     comes back as a borrower)",
    "each resource of another node it borrowed: another release <resource id> --agent \
     <name>, then another borrow <resource id> --agent <name> (the id is in brackets in \
     another resources; kind:name names only this node's resources. If the borrow is \
     refused, the release has already happened: it is refused when the hub does not grant \
     borrowing, when the resource has no live owner, or when it cannot be reached from this \
     machine)",
    "mail it sent to other nodes before step 1 is not delivered there and no command \
     delivers it: send it again",
    "another sync",
];

const HOLD_MISSING: &str = "this hold: an agent on another node is not warned before it \
    edits the same files, and renewals of it change nothing there";
const HOLD_STEPS: [&str; 3] = [
    "another hold release --agent <agent> <hold id>",
    "another hold acquire --agent <agent> -p <project> <paths>, with --shared, --repo-wide \
     and --ttl as the hold had them, each pattern 4096 bytes or fewer and --reason 1024 \
     bytes or fewer",
    "another sync",
];

const HOLD_RENEWED_MISSING: &str = "this renewal: other nodes may show these holds as \
    expired, and not warn their agents, until the next one arrives";
const HOLD_RENEWED_STEPS: [&str; 1] = [
    "another heartbeat --agent <agent>, then another sync (the next heartbeat does the \
      same by itself, in events of at most 10000 ids)",
];

const LEASE_RENEWED_MISSING: &str = "this renewal. A lease on this node's own resource \
    shows as expired on other nodes until the next renewal arrives. A lease borrowed on \
    another node's resource is different: if that node saw no renewal before the lease ran \
    out there, it ended the lease, on every node including this one, and a later renewal \
    does not bring it back";
const LEASE_RENEWED_STEPS: [&str; 3] = [
    "another sync",
    "each resource of another node the agent borrowed: another borrow <resource id> --agent \
     <agent> (the id is in brackets in another resources; kind:name names only this node's \
     resources; this renews a lease that is still live and borrows again one that ended)",
    "another sync",
];

const MESSAGE_MISSING: &str = "this message: its recipients on other nodes never received it";
const MESSAGE_STEPS: [&str; 2] = [
    "another send again to fewer recipients in each message (a body is at most 262144 \
     bytes, so only the list of recipients makes a message this large)",
    "another sync",
];

// The same steps as `another needs` prints them. Its rule for a step (docs/NEEDS.md): an
// entry that starts with `another` is a complete command that runs as it stands. So only
// such a command is printed bare; every other step starts with a word and puts each command
// in backticks, and a placeholder is never in a bare command.
const AGENT_NEEDS: [&str; 6] = [
    "register the agent again: `another agent register <name> --program <program> --model \
     <model> --description \"<4096 bytes or fewer>\"` (add `--project <path>` if it has one; \
     a model left out is cleared)",
    "each hold it has (`another holds --agent <name>` lists them): `another hold release \
     --agent <name> <hold id>`, then `another hold acquire \
     --agent <name> -p <project> <the same paths>`, with --shared, --repo-wide, --reason and \
     --ttl as the hold has them (a fresh checkout is sent; without -p from another \
     directory it is a different hold, and without --shared a shared hold becomes exclusive \
     or is refused)",
    "each resource of this node it holds a lease on: `another release <resource id or \
     kind:name> --agent <name>`, then at once `another borrow <the same> --agent <name>` (a \
     borrow without the release only renews, and a renewal brings nothing to a node that \
     never had the lease. Between the two commands the resource has no lease unless another \
     agent holds one. Another agent on this node that borrows it in that moment becomes its \
     owner, and if the resource is exclusive this agent's borrow is then refused. If another \
     agent already holds a lease, that agent becomes the owner at the release and this agent \
     comes back as a borrower)",
    "each resource of another node it borrowed: `another release <resource id> --agent \
     <name>`, then `another borrow <resource id> --agent <name>` (the id is in brackets in \
     `another resources`; kind:name names only this node's resources. If the borrow is \
     refused, the release has already happened: it is refused when the hub does not grant \
     borrowing, when the resource has no live owner, or when it cannot be reached from this \
     machine)",
    "mail it sent to other nodes before step 1 is not delivered there and no command \
     delivers it: send it again",
    "another sync",
];
const HOLD_NEEDS: [&str; 3] = [
    "release the hold: `another hold release --agent <agent> <hold id>`",
    "acquire it again: `another hold acquire --agent <agent> -p <project> <paths>`, with \
     --shared, --repo-wide and --ttl as the hold had them, each pattern 4096 bytes or fewer \
     and --reason 1024 bytes or fewer",
    "another sync",
];
const HOLD_RENEWED_NEEDS: [&str; 1] = [
    "renew the holds: `another heartbeat --agent <agent>`, then `another sync` (the next \
     heartbeat does the same by itself, in events of at most 10000 ids)",
];
const LEASE_RENEWED_NEEDS: [&str; 3] = [
    "another sync",
    "each resource of another node the agent borrowed: `another borrow <resource id> --agent \
     <agent>` (the id is in brackets in `another resources`; kind:name names only this \
     node's resources; this renews a lease that is still live and borrows again one that \
     ended)",
    "another sync",
];
const MESSAGE_NEEDS: [&str; 2] = [
    "send it again with `another send`, to fewer recipients in each message (a body is at \
     most 262144 bytes, so only the list of recipients makes a message this large)",
    "another sync",
];

/// What other nodes miss while an event is skipped, and the steps that get the information
/// across, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remedy {
    pub missing: &'static str,
    /// The steps as `another sync` and the docs word them.
    pub steps: Vec<String>,
    /// The same steps as `another needs` words them: a bare entry is a complete command.
    pub needs_steps: Vec<String>,
    /// Said beside the steps when a value in the report could not be put into them.
    pub note: Option<&'static str>,
}

/// The remedy for a skipped event of `kind`. The lines are fixed per kind. `about` (the
/// marker's short name of the thing) fills only `<name>`, and only with a valid agent
/// name, and `<hold id>`, and only with an id this software makes, and only when the
/// marker speaks of its writer's own event. Any other value leaves the placeholder and is
/// noted: a peer chose it, and the lines are there to be copied. Every other placeholder
/// is for the reader to fill.
#[allow(dead_code)]
pub fn remedy(kind: &str, about: Option<&str>) -> Remedy {
    remedy_from(kind, about, true)
}

/// [`remedy`], filling from `about` only when `own_event` is true (`event_node == holder`).
pub fn remedy_from(kind: &str, about: Option<&str>, own_event: bool) -> Remedy {
    let named = |prefix: &str| {
        about
            .and_then(|a| a.strip_prefix(prefix))
            .filter(|rest| !rest.is_empty())
    };
    let fill = |steps: &[&str], placeholder: &str, value: Option<&str>| -> Vec<String> {
        steps
            .iter()
            .map(|step| match value {
                Some(value) => step.replace(placeholder, value),
                None => (*step).to_string(),
            })
            .collect()
    };
    let (missing, steps, needs, placeholder, value, unusable): (
        _,
        &[&str],
        &[&str],
        _,
        Option<&str>,
        _,
    ) = match kind {
        "agent.registered" => (
            AGENT_MISSING,
            &AGENT_STEPS,
            &AGENT_NEEDS,
            "<name>",
            named("agent "),
            NAME_NOT_USABLE,
        ),
        "hold.acquired" | "hold.updated" => (
            HOLD_MISSING,
            &HOLD_STEPS,
            &HOLD_NEEDS,
            "<hold id>",
            named("hold "),
            HOLD_ID_NOT_USABLE,
        ),
        "hold.renewed" => (
            HOLD_RENEWED_MISSING,
            &HOLD_RENEWED_STEPS,
            &HOLD_RENEWED_NEEDS,
            "",
            None,
            "",
        ),
        "resource.renewed" => (
            LEASE_RENEWED_MISSING,
            &LEASE_RENEWED_STEPS,
            &LEASE_RENEWED_NEEDS,
            "",
            None,
            "",
        ),
        "message.sent" => (
            MESSAGE_MISSING,
            &MESSAGE_STEPS,
            &MESSAGE_NEEDS,
            "",
            None,
            "",
        ),
        _ => (
            GENERAL_MISSING,
            &[GENERAL_STEP],
            &[GENERAL_STEP],
            "",
            None,
            "",
        ),
    };
    let would_fill = match placeholder {
        "<name>" => value.filter(|v| usable_name(v)),
        _ => value.filter(|v| usable_id(v)),
    };
    let usable = if own_event { would_fill } else { None };
    let note = if !own_event && would_fill.is_some() {
        Some(match placeholder {
            "<name>" => FOREIGN_MARKER_NAME,
            "<hold id>" => FOREIGN_MARKER_HOLD,
            _ => unusable,
        })
        .filter(|n| !n.is_empty())
    } else {
        (value.is_some() && usable.is_none()).then_some(unusable)
    };
    Remedy {
        missing,
        steps: fill(steps, placeholder, usable),
        needs_steps: fill(needs, placeholder, usable),
        note,
    }
}

/// A bare `another sync` step is not a command a hub can run. Replace it when the event
/// lives on a node that has no hub.
fn replace_hub_sync_steps(steps: Vec<String>) -> Vec<String> {
    steps
        .into_iter()
        .map(|step| {
            if step == "another sync" {
                HUB_SYNC_SENTENCE.to_string()
            } else {
                step
            }
        })
        .collect()
}

/// One unacknowledged skip this node reports: one of its own log, or one of its hub's log.
/// Every value a marker carried is one bounded, safe line here.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Skip {
    pub event_id: String,
    /// The node that wrote the skipped event: where the remedy runs.
    pub event_node: String,
    pub event_node_name: String,
    /// The node whose log holds the event and whose sync skipped it: where the
    /// acknowledgement runs.
    pub holder: String,
    pub holder_name: String,
    pub kind: String,
    pub bytes: u64,
    pub actor: String,
    pub about: Option<String>,
    /// `push` or `pull`.
    pub direction: String,
    pub marker_id: String,
    /// When the holder first stepped past the event.
    pub since: String,
    /// What other nodes miss while the event is skipped.
    pub missing: String,
    /// The steps that get the information across, in order, to run on `event_node`.
    pub remedy: Vec<String>,
    /// The command that stops the report, to run on `holder`.
    pub ack: String,
    /// What is said when a value of the report was not put into a command: it was not a
    /// valid agent name, or not an id this software makes. Absent when there is nothing
    /// to say.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The steps as `another needs` words them.
    #[serde(skip)]
    pub needs_remedy: Vec<String>,
    /// `ack` holds the event's id, so it runs as it stands.
    #[serde(skip)]
    pub ack_runs: bool,
    /// The holder is this node.
    #[serde(skip)]
    pub own: bool,
    /// The node that wrote the event is this node.
    #[serde(skip)]
    pub event_is_local: bool,
    /// The marker's time in milliseconds.
    #[serde(skip)]
    pub since_ms: i64,
}

impl Skip {
    fn head(&self) -> String {
        format!("{} ({}, {} bytes", self.event_id, self.kind, self.bytes)
    }

    fn direction(&self) -> Direction {
        Direction::of(&self.direction)
    }

    /// `this node`, or `node <name>`: where the remedy runs.
    fn remedy_place(&self) -> String {
        if self.event_is_local {
            "this node".to_string()
        } else {
            format!("node {}", self.event_node_name)
        }
    }

    /// The lines `another sync` prints for this skip.
    pub fn sync_block(&self) -> String {
        let reader = self.direction().reader();
        let mut out = format!("skipped {}, written by {}): ", self.head(), self.actor);
        if self.own {
            out.push_str(&format!(
                "it is in this node's log and is larger than {reader}, so it was not sent; \
                 the events after it were sent"
            ));
        } else {
            let hub = &self.holder_name;
            out.push_str(&format!(
                "it is in node {hub}'s log and is larger than {reader}, so node {hub} did \
                 not send it; the events after it were sent"
            ));
        }
        if let Some(about) = &self.about {
            out.push_str(&format!("\n  about: {about}"));
        }
        for note in &self.notes {
            out.push_str(&format!("\n  note: {note}"));
        }
        let place = self.remedy_place();
        if self.own {
            out.push_str(&format!("\n  missing on other nodes: {}", self.missing));
            if self.event_is_local {
                out.push_str("\n  to get it across:");
            } else {
                out.push_str(&format!("\n  to get it across, on {place}:"));
            }
        } else {
            out.push_str(&format!("\n  missing on this node: {}", self.missing));
            out.push_str(&format!("\n  to get it across, on {place}:"));
        }
        for step in &self.remedy {
            out.push_str(&format!("\n    {step}"));
        }
        if self.own {
            out.push_str(&format!("\n  then: {}", self.ack));
        } else {
            out.push_str(&format!(
                "\n  then, on node {}: {}",
                self.holder_name, self.ack
            ));
        }
        out
    }

    /// The row `another needs` prints for this skip.
    pub fn needs_line(&self) -> String {
        let one = self.direction().one();
        if self.own {
            format!(
                "skipped event  {}) in this node's log was not sent: larger than one {one}",
                self.head()
            )
        } else {
            format!(
                "skipped event  {}) in node {}'s log was not sent to this node: larger than \
                 one {one}",
                self.head(),
                self.holder_name
            )
        }
    }

    /// The steps `another needs` prints under the row: any note, the remedy, then the
    /// acknowledgement. An entry that starts with `another`, or that is `on <node>: `
    /// followed by one, is a complete command; every other entry starts with a word and has
    /// its commands in backticks.
    pub fn needs_steps(&self) -> Vec<String> {
        let place = self.remedy_place();
        let mut steps: Vec<String> = self.notes.iter().map(|n| format!("note: {n}")).collect();
        steps.extend(self.needs_remedy.iter().map(|step| {
            if self.event_is_local {
                step.clone()
            } else {
                format!("on {place}: {step}")
            }
        }));
        let ack = if self.ack_runs {
            self.ack.clone()
        } else {
            format!("acknowledge it with `{}`", self.ack)
        };
        steps.push(if self.own {
            ack
        } else {
            format!("on node {}: {ack}", self.holder_name)
        });
        steps
    }
}

/// The lines `another sync` prints after a sync for the unacknowledged skips: those given,
/// which are at most [`SYNC_SKIPS_SHOWN`], in full, then a count of the rest of `total`.
/// Empty when there is none.
pub fn sync_skip_lines(skips: &[Skip], total: usize) -> String {
    let mut out: Vec<String> = skips
        .iter()
        .take(SYNC_SKIPS_SHOWN)
        .map(Skip::sync_block)
        .collect();
    let more = total.saturating_sub(out.len());
    if more > 0 {
        out.push(format!("and {more} more skipped event(s): another needs"));
    }
    out.join("\n")
}

fn node_name(conn: &Connection, node_id: &str) -> Result<String> {
    let name: Option<String> = conn
        .query_row(
            "SELECT display_name FROM nodes WHERE node_id = ?1",
            [node_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(display_line(
        name.as_deref().unwrap_or(node_id),
        SHOWN_NODE_CHARS,
    ))
}

fn holds_event(conn: &Connection, event_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM events WHERE event_id = ?1",
            [event_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn unacknowledged(conn: &Connection, local: &str) -> Result<Vec<Skip>> {
    let local_hub = hub_of(conn)?.map(|(hub, _, _)| hub);
    let hub = local_hub.as_deref().unwrap_or(local);
    let mut acked: BTreeSet<(String, String)> = BTreeSet::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT node_id, {NAMED_EVENT} FROM events WHERE kind = ?1 AND node_id IN (?2, ?3)"
    ))?;
    let mut rows = stmt.query(params![KIND_ACKED, local, hub])?;
    while let Some(r) = rows.next()? {
        if let Some(id) = r.get::<_, Option<String>>(1)? {
            acked.insert((r.get(0)?, id));
        }
    }
    let mut stmt = conn.prepare(
        "SELECT event_id, node_id, ts, payload FROM events
         WHERE kind = ?1 AND node_id IN (?2, ?3) ORDER BY ts, seq",
    )?;
    let mut rows = stmt.query(params![KIND_SKIPPED, local, hub])?;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let payload: String = r.get(3)?;
        let m = Marker::of(r.get(0)?, r.get(1)?, r.get(2)?, &payload);
        let key = (m.holder.clone(), m.event_id.clone());
        if m.event_id.is_empty() || acked.contains(&key) || !seen.insert(key) {
            continue;
        }
        let own = m.holder == local;
        if !own && holds_event(conn, &m.event_id)? {
            continue;
        }
        let event_id = display_line(&m.event_id, SHOWN_ID_CHARS);
        let about = m.about.as_deref().map(|a| display_line(a, ABOUT_CHARS));
        let own_event = m.event_node == m.holder;
        let mut remedy = remedy_from(&m.kind, about.as_deref(), own_event);
        let event_node_is_hub = match local_hub.as_deref() {
            None => m.event_node == local,
            Some(h) => m.event_node == h,
        };
        if event_node_is_hub {
            remedy.steps = replace_hub_sync_steps(remedy.steps);
            remedy.needs_steps = replace_hub_sync_steps(remedy.needs_steps);
        }
        let (ack, ack_runs) = ack_command(&event_id);
        let mut notes: Vec<String> = remedy.note.iter().map(|n| (*n).to_string()).collect();
        if !ack_runs {
            notes.push(EVENT_ID_NOT_USABLE.to_string());
        }
        out.push(Skip {
            ack,
            ack_runs,
            notes,
            needs_remedy: remedy.needs_steps,
            event_id,
            event_node_name: node_name(conn, &m.event_node)?,
            event_is_local: m.event_node == local,
            event_node: display_line(&m.event_node, SHOWN_NODE_CHARS),
            holder_name: node_name(conn, &m.holder)?,
            holder: m.holder,
            kind: display_line(&m.kind, SHOWN_KIND_CHARS),
            bytes: m.bytes,
            actor: display_line(&m.actor, SHOWN_ACTOR_CHARS),
            about,
            direction: m.direction.as_str().to_string(),
            marker_id: m.marker_id,
            since: fmt_ts(m.ts),
            since_ms: m.ts,
            missing: remedy.missing.to_string(),
            remedy: remedy.steps,
            own,
        });
    }
    Ok(out)
}

/// What `another sync ack` found for an event id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipAck {
    /// This node skipped the event and has not acknowledged it (from
    /// [`Store::skip_to_acknowledge`]), or has acknowledged it just now (from
    /// [`Store::acknowledge_skip`]). The values are bounded, safe lines.
    Skip {
        event_id: String,
        kind: String,
        bytes: u64,
        actor: String,
        /// `push` or `pull answer`: what the event is larger than one of.
        larger_than_one: &'static str,
    },
    /// It was already acknowledged, at this time.
    Already { at: String },
}

fn ack_lookup(conn: &Connection, local: &str, event_id: &str) -> Result<(Marker, Option<i64>)> {
    let id = marker_id(event_id);
    if let Some(marker) = own_marker(conn, local, &id)? {
        let at = acked_at(conn, local, &id)?;
        return Ok((marker, at));
    }
    let shown = display_line(event_id, SHOWN_ID_CHARS);
    let other: Option<String> = conn
        .query_row(
            &format!(
                "SELECT node_id FROM events
                 WHERE kind = ?1 AND node_id != ?2 AND {NAMED_EVENT} = ?3
                 ORDER BY seq LIMIT 1"
            ),
            params![KIND_SKIPPED, local, id],
            |r| r.get(0),
        )
        .optional()?;
    match other {
        Some(node) => Err(Error::Invalid(format!(
            "event {shown} is in node {}'s log; acknowledge it there: {}",
            node_name(conn, &node)?,
            ack_command(&shown).0
        ))),
        None => Err(Error::NotFound(format!(
            "no skipped event {shown}; skipped events are listed by: another activity --kinds sync"
        ))),
    }
}

fn ack_view(marker: &Marker) -> SkipAck {
    SkipAck::Skip {
        event_id: display_line(&marker.event_id, SHOWN_ID_CHARS),
        kind: display_line(&marker.kind, SHOWN_KIND_CHARS),
        bytes: marker.bytes,
        actor: display_line(&marker.actor, SHOWN_ACTOR_CHARS),
        larger_than_one: marker.direction.one(),
    }
}

impl Store {
    /// The unacknowledged skips this node reports: those of its own log and those of its
    /// unrevoked hub's log, oldest first, one per holder and event. A skip in the hub's log
    /// of an event this node holds is left out. Markers of other spokes are never
    /// repeated here; they show in the activity feed. Read only.
    pub fn unacknowledged_skips(&self) -> Result<Vec<Skip>> {
        self.read(|conn| unacknowledged(conn, self.node_id()))
    }

    /// Hub side: write the missing markers, and their audit rows, for the events a pull
    /// page stepped past. One write transaction, run before the page is answered.
    pub fn mark_pull_skips(&self, skipped: &[SkippedEvent]) -> Result<()> {
        if skipped.is_empty() {
            return Ok(());
        }
        self.write(|tx| mark_tx(tx, skipped, Direction::Pull))
    }

    /// What `another sync ack <event_id>` would acknowledge. Fails when another node's
    /// marker names the event and none of this node's does (`invalid`), and when no marker
    /// names it (`not_found`). Read only.
    pub fn skip_to_acknowledge(&self, event_id: &str) -> Result<SkipAck> {
        self.read(|conn| {
            let (marker, acked) = ack_lookup(conn, self.node_id(), event_id)?;
            Ok(match acked {
                Some(at) => SkipAck::Already { at: fmt_ts(at) },
                None => ack_view(&marker),
            })
        })
    }

    /// Record that the owner has seen the skip of `event_id`: one `sync.skip_acked` event
    /// and one audit row, in one transaction that checks again that the skip is still
    /// unacknowledged. Nothing is deleted and the event is not sent. The caller has
    /// already had `--owner`, a terminal and a typed yes.
    pub fn acknowledge_skip(&self, event_id: &str) -> Result<SkipAck> {
        self.write(|tx| {
            let (marker, acked) = ack_lookup(tx, tx.node_id, event_id)?;
            if let Some(at) = acked {
                return Ok(SkipAck::Already { at: fmt_ts(at) });
            }
            tx.emit(
                "human (another sync ack)",
                KIND_ACKED,
                json!({
                    "event_id": marker.event_id,
                    "route": marker.route,
                    "route_nodes": marker.route_nodes,
                }),
            )?;
            insert_audit(
                tx,
                "sync ack",
                "ok",
                format!(
                    "event {} ({}, {} bytes) acknowledged with --owner at a terminal; it \
                     stays in the log and is not sent",
                    display_line(&marker.event_id, SHOWN_ID_CHARS),
                    display_line(&marker.kind, SHOWN_KIND_CHARS),
                    marker.bytes,
                ),
            )?;
            Ok(ack_view(&marker))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One assertion per entry of the remedy lines (D-next).
    #[test]
    fn each_kind_gets_its_remedy_lines() {
        let Remedy { missing, steps, .. } = remedy("agent.registered", Some("agent wordy"));
        assert!(missing
            .starts_with("what this registration changed: the agent's description and model. "));
        assert!(
            missing.ends_with("a resource it borrowed on another node is not held for it there")
        );
        assert_eq!(steps.len(), 6);
        assert_eq!(
            steps[0],
            "another agent register wordy --program <program> --model <model> --description \
             \"<4096 bytes or fewer>\" (add --project <path> if it has one; a model left out \
             is cleared)"
        );
        assert!(steps[1].starts_with(
            "each hold it has (another holds --agent wordy lists them): another hold release \
             --agent wordy <hold id>, then another hold acquire \
             --agent wordy -p <project> <the same paths>, with --shared, --repo-wide, --reason \
             and --ttl as the hold has them "
        ));
        // Steps 3 and 4 release before they borrow: a borrow alone only renews.
        assert!(steps[2].starts_with(
            "each resource of this node it holds a lease on: another release <resource id or \
             kind:name> --agent wordy, then at once another borrow <the same> --agent wordy "
        ));
        assert!(steps[3].starts_with(
            "each resource of another node it borrowed: another release <resource id> --agent \
             wordy, then another borrow <resource id> --agent wordy (the id is in brackets in \
             another resources; kind:name names only this node's resources. "
        ));
        assert_eq!(
            steps[4],
            "mail it sent to other nodes before step 1 is not delivered there and no command \
             delivers it: send it again"
        );
        assert_eq!(steps[5], "another sync");

        for kind in ["hold.acquired", "hold.updated"] {
            let Remedy { missing, steps, .. } =
                remedy(kind, Some("hold 01HD0000000000000000000000"));
            assert_eq!(
                missing,
                "this hold: an agent on another node is not warned before it edits the same \
                 files, and renewals of it change nothing there"
            );
            assert_eq!(
                steps,
                [
                    "another hold release --agent <agent> 01HD0000000000000000000000",
                    "another hold acquire --agent <agent> -p <project> <paths>, with --shared, \
                     --repo-wide and --ttl as the hold had them, each pattern 4096 bytes or \
                     fewer and --reason 1024 bytes or fewer",
                    "another sync",
                ]
            );
        }

        let Remedy { missing, steps, .. } = remedy("hold.renewed", Some("3 holds"));
        assert_eq!(
            missing,
            "this renewal: other nodes may show these holds as expired, and not warn their \
             agents, until the next one arrives"
        );
        assert_eq!(
            steps,
            [
                "another heartbeat --agent <agent>, then another sync (the next heartbeat does \
              the same by itself, in events of at most 10000 ids)"
            ]
        );

        let Remedy { missing, steps, .. } = remedy("resource.renewed", Some("2 leases"));
        assert!(missing.starts_with("this renewal. A lease on this node's own resource shows as expired on other nodes until the next renewal arrives. "));
        assert!(missing.ends_with("it ended the lease, on every node including this one, and a later renewal does not bring it back"));
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0], "another sync");
        assert_eq!(
            steps[1],
            "each resource of another node the agent borrowed: another borrow <resource id> \
             --agent <agent> (the id is in brackets in another resources; kind:name names only \
             this node's resources; this renews a lease that is still live and borrows again \
             one that ended)"
        );
        assert_eq!(steps[2], "another sync");

        let Remedy { missing, steps, .. } =
            remedy("message.sent", Some("message 01MSG to 9 recipients"));
        assert_eq!(
            missing,
            "this message: its recipients on other nodes never received it"
        );
        assert_eq!(
            steps,
            [
                "another send again to fewer recipients in each message (a body is at most \
                 262144 bytes, so only the list of recipients makes a message this large)",
                "another sync",
            ]
        );

        for kind in [
            "hold.released",
            "resource.released",
            "request.completed",
            "no.such_kind",
        ] {
            let Remedy { missing, steps, .. } = remedy(kind, None);
            assert_eq!(
                missing,
                "this event and whatever it changed on other nodes; docs/FEDERATION.md, \"What \
                 other nodes miss\", says what that is for each kind"
            );
            assert_eq!(
                steps,
                [
                    "no command re-sends this event. For most kinds nothing needs doing (a \
                  release runs out on other nodes by itself, a request only loses a feed \
                  line); if other nodes need what it recorded, do that action again"
                ]
            );
        }
    }

    /// `kind:name` names only a resource of the node the command runs on, so every step
    /// that can mean another node's resource names it by its id and nothing else.
    #[test]
    fn a_step_for_another_nodes_resource_names_it_by_id_only() {
        let mut foreign = 0;
        for kind in ["agent.registered", "resource.renewed"] {
            let r = remedy(kind, None);
            for step in r.steps.iter().chain(&r.needs_steps) {
                if step.starts_with("each resource of another node") {
                    foreign += 1;
                    assert!(step.contains("<resource id>"), "{step}");
                    assert!(!step.contains("<resource id or kind:name>"), "{step}");
                    assert!(
                        step.contains("kind:name names only this node's resources"),
                        "{step}"
                    );
                }
            }
        }
        assert_eq!(foreign, 4);
    }

    /// A value from a marker goes into a printed command only when it is a valid agent
    /// name or an id this software makes. Anything else, which a hub could have chosen,
    /// leaves the placeholder and is noted, in both wordings of the steps.
    #[test]
    fn a_value_from_a_marker_fills_a_command_only_when_it_is_a_name_or_an_id() {
        // A hub's `about: "hold ."` once printed a release of every hold under `.`.
        for hostile in [
            "hold .",
            "hold --help",
            "hold -p/x",
            "hold 01H0LD",
            "hold 01hd0000000000000000000000",
            "hold 01HL0000000000000000000000",
            "hold 01HD0000000000000000000000 x",
            "hold '; rm -rf ~; echo '",
            "hold $(id)",
            "hold 81HD0000000000000000000000",
        ] {
            let r = remedy("hold.acquired", Some(hostile));
            assert_eq!(
                r.steps[0], "another hold release --agent <agent> <hold id>",
                "{hostile}"
            );
            assert_eq!(
                r.needs_steps[0],
                "release the hold: `another hold release --agent <agent> <hold id>`",
                "{hostile}"
            );
            assert_eq!(
                r.note,
                Some(
                    "the hold id in this report is not an id this software makes, so <hold \
                     id> is left for you to fill"
                ),
                "{hostile}"
            );
        }
        for hostile in [
            "agent x; rm -rf ~",
            "agent it's",
            "agent '$(touch pwned)'",
            "agent --help",
            "agent -x9",
            "agent q z",
            "agent wordy@shop",
            "agent \u{202e}ydrow",
        ] {
            let r = remedy("agent.registered", Some(hostile));
            for step in r.steps.iter().chain(&r.needs_steps) {
                assert!(!step.contains(&hostile[6..]), "{hostile}: {step}");
            }
            assert!(
                r.steps[0].starts_with("another agent register <name> --program"),
                "{hostile}: {}",
                r.steps[0]
            );
            assert_eq!(
                r.note,
                Some(
                    "the agent's name in this report is not a valid agent name, so <name> is \
                     left for you to fill"
                ),
                "{hostile}"
            );
        }
        // No value at all is not a bad value: the placeholder stays and nothing is noted.
        for about in [None, Some("agent "), Some("3 holds")] {
            let r = remedy("agent.registered", about);
            assert!(r.steps[0].starts_with("another agent register <name> --program"));
            assert_eq!(r.note, None);
        }
        // An honest name and an honest id fill, with nothing noted.
        let r = remedy("agent.registered", Some("agent word.y_2-b"));
        assert!(r.steps[0].starts_with("another agent register word.y_2-b --program"));
        assert_eq!(r.note, None);
        let r = remedy("hold.updated", Some("hold 01HD0000000000000000000000"));
        assert_eq!(r.note, None);

        assert_eq!(
            ack_command("01SK1PPED00000000000000000"),
            (
                "another sync ack 01SK1PPED00000000000000000 --owner".to_string(),
                true
            )
        );
        for hostile in [".", "--all", "01SKIPPED", "x; id", "it's"] {
            assert_eq!(
                ack_command(hostile),
                ("another sync ack <event-id> --owner".to_string(), false),
                "{hostile}"
            );
        }
    }

    /// The rule of `another needs` for a step: an entry that starts with `another` is a
    /// complete command that runs as it stands. In the needs wording a bare entry is such a
    /// command, with no placeholder and nothing after it; every other entry starts with a
    /// word and has each command in backticks.
    #[test]
    fn a_bare_step_for_needs_is_a_complete_command() {
        let mut bare = 0;
        for (kind, about) in [
            ("agent.registered", Some("agent wordy")),
            ("agent.registered", Some("agent .; x")),
            ("hold.acquired", Some("hold 01HD0000000000000000000000")),
            ("hold.updated", Some("hold .")),
            ("hold.renewed", Some("3 holds")),
            ("resource.renewed", Some("1 lease")),
            ("message.sent", Some("message 01M to 1 recipient")),
            ("request.completed", None),
        ] {
            let r = remedy(kind, about);
            assert_eq!(r.steps.len(), r.needs_steps.len(), "{kind}");
            for step in &r.needs_steps {
                if step.starts_with("another") {
                    bare += 1;
                    assert_eq!(step, "another sync", "{kind}");
                    continue;
                }
                // Each `another` in it opens a backtick pair, and no pair is left open.
                assert_eq!(step.matches('`').count() % 2, 0, "{kind}: {step}");
                assert_eq!(
                    step.matches("another ").count(),
                    step.matches("`another ").count()
                        + step.matches("another agent on this node").count()
                        + step.matches("unless another agent").count()
                        + step.matches("If another agent").count()
                        + step.matches("of another node").count()
                        + step.matches("from another directory").count(),
                    "{kind}: {step}"
                );
                assert!(!step.contains(": another "), "{kind}: {step}");
            }
        }
        assert!(bare >= 6, "{bare}");
        let needs = remedy("message.sent", None).needs_steps;
        assert_eq!(
            needs[0],
            "send it again with `another send`, to fewer recipients in each message (a body \
             is at most 262144 bytes, so only the list of recipients makes a message this \
             large)"
        );
        let needs = remedy("hold.renewed", None).needs_steps;
        assert_eq!(
            needs,
            [
                "renew the holds: `another heartbeat --agent <agent>`, then `another sync` (the \
              next heartbeat does the same by itself, in events of at most 10000 ids)"
            ]
        );
        let needs = remedy("agent.registered", Some("agent wordy")).needs_steps;
        assert_eq!(
            needs[0],
            "register the agent again: `another agent register wordy --program <program> \
             --model <model> --description \"<4096 bytes or fewer>\"` (add `--project <path>` \
             if it has one; a model left out is cleared)"
        );
    }

    #[test]
    fn about_is_built_from_small_fields_and_cut() {
        let p = json!({ "name": "wordy", "description": "d".repeat(4096) });
        assert_eq!(
            about("agent.registered", &p).as_deref(),
            Some("agent wordy")
        );
        let p = json!({ "hold_id": "01H" });
        assert_eq!(about("hold.acquired", &p).as_deref(), Some("hold 01H"));
        let p = json!({ "hold_ids": ["a", "b", "c"] });
        assert_eq!(about("hold.renewed", &p).as_deref(), Some("3 holds"));
        let p = json!({ "lease_ids": ["a", "b"] });
        assert_eq!(about("resource.renewed", &p).as_deref(), Some("2 leases"));
        let p = json!({ "message_id": "01M", "recipients": [{}, {}], "request_id": "01R" });
        assert_eq!(
            about("message.sent", &p).as_deref(),
            Some("message 01M to 2 recipients")
        );
        // One of a thing is not "1 holds".
        let p = json!({ "hold_ids": ["a"], "lease_ids": ["a"] });
        assert_eq!(about("hold.released", &p).as_deref(), Some("1 hold"));
        assert_eq!(about("resource.released", &p).as_deref(), Some("1 lease"));
        let p = json!({ "message_id": "01M", "recipients": [{}] });
        assert_eq!(
            about("message.sent", &p).as_deref(),
            Some("message 01M to 1 recipient")
        );
        let p = json!({ "request_id": "01R" });
        assert_eq!(
            about("request.completed", &p).as_deref(),
            Some("request 01R")
        );
        assert_eq!(about("session.started", &p), None);
        assert_eq!(about("agent.registered", &json!({})), None);
        let p = json!({ "name": "n".repeat(5000) });
        let long = about("agent.registered", &p).unwrap();
        assert_eq!(long.chars().count(), ABOUT_CHARS);
    }
    #[test]
    fn a_hub_marker_about_a_spoke_event_does_not_fill() {
        let id = "01M7WV7X0X33GJDE8SX6BVSBP7";
        let r = remedy_from("hold.acquired", Some(&format!("hold {id}")), false);
        assert!(
            r.needs_steps.iter().any(|s| s.contains("<hold id>")),
            "{:#?}",
            r.needs_steps
        );
        assert!(
            !r.needs_steps.iter().any(|s| s.contains(id)),
            "{:#?}",
            r.needs_steps
        );
        assert_eq!(r.note, Some(FOREIGN_MARKER_HOLD));

        let r = remedy_from("hold.acquired", Some(&format!("hold {id}")), true);
        assert!(
            r.needs_steps.iter().any(|s| s.contains(id)),
            "{:#?}",
            r.needs_steps
        );
        assert_eq!(r.note, None);

        let r = remedy_from("agent.registered", Some("agent bob"), false);
        assert!(
            r.needs_steps.iter().any(|s| s.contains("<name>")),
            "{:#?}",
            r.needs_steps
        );
        assert!(
            !r.needs_steps
                .iter()
                .any(|s| s.contains(" bob") || s.ends_with("bob") || s.contains("register bob ")),
            "{:#?}",
            r.needs_steps
        );
        assert_eq!(r.note, Some(FOREIGN_MARKER_NAME));
    }

    #[test]
    fn a_hubs_own_skip_does_not_print_another_sync_as_a_command() {
        let r = remedy("hold.acquired", Some("hold 01HD0000000000000000000000"));
        let needs = replace_hub_sync_steps(r.needs_steps);
        let steps = replace_hub_sync_steps(r.steps);
        assert!(!needs.iter().any(|s| s == "another sync"), "{needs:#?}");
        assert!(needs.iter().any(|s| s == HUB_SYNC_SENTENCE), "{needs:#?}");
        assert!(!steps.iter().any(|s| s == "another sync"), "{steps:#?}");
        assert!(steps.iter().any(|s| s == HUB_SYNC_SENTENCE), "{steps:#?}");
        // The acknowledgement is unchanged and still a command.
        // (needs_steps() on Skip adds the ack after these remedy steps.)
    }
}
