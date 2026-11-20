//! Federation (v2): nodes exchange their append-only, signed event logs through a hub.
//!
//! - Each node is authoritative for its own agents, holds and resources; foreign events are
//!   projected into local read models but never rewrite another node's ownership.
//! - Spokes dial out to a self-hostable hub: they **push** events they authored and **pull**
//!   a filtered stream of everyone else's. The hub relays; spokes never talk to each other.
//! - Every event is signed by its origin node, so relayed events can be checked end to end.
//! - What a spoke may do is scoped by the permissions the hub granted at pairing:
//!   `read_feed`, `send_mail`, `borrow_resource`. There is no remote reap permission.

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::crypto;
use crate::error::{Error, Result};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts, later_secs};

/// What a paired node may do through the hub.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Perm {
    /// Receive the activity feed of other nodes.
    ReadFeed,
    /// Mail agents on other nodes.
    SendMail,
    /// Borrow resources owned by other nodes.
    BorrowResource,
}

impl Perm {
    pub const ALL: [Perm; 3] = [Perm::ReadFeed, Perm::SendMail, Perm::BorrowResource];

    pub fn as_str(self) -> &'static str {
        match self {
            Perm::ReadFeed => "read_feed",
            Perm::SendMail => "send_mail",
            Perm::BorrowResource => "borrow_resource",
        }
    }
}

impl std::str::FromStr for Perm {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Perm::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown permission {s:?}; expected read_feed, send_mail or borrow_resource \
                     (there is no remote reap permission)"
                ))
            })
    }
}

/// One event as it travels between nodes. `payload` is the exact signed text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    /// The sender's local sequence number (informational).
    pub seq: i64,
    pub event_id: String,
    pub node_id: String,
    pub ts: i64,
    pub actor: String,
    pub kind: String,
    pub payload: String,
    pub prev_id: Option<String>,
    pub sig: String,
}

/// Largest single event this build writes, in bytes of its wire JSON (a [`WireEvent`],
/// escaping included, with the sequence number counted as one digit). A local write over it
/// is refused (D99). It is a rule for local writes only: an event already in a log, or one
/// a peer sends, is not judged by it, and travels if it fits the transport.
pub const MAX_EVENT_WIRE_BYTES: usize = 2 * 1024 * 1024;
/// A push batch is closed before its request body would pass this; it always holds one event.
pub const PUSH_BATCH_BUDGET_BYTES: usize = 4 * 1024 * 1024;
/// Largest `/fed/push` request body a hub reads.
pub const PUSH_BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;
/// A pull page is closed before its response body, the node directory included, would pass
/// this; it always holds one event.
pub const PULL_PAGE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Largest `/fed/pull` response body a spoke reads.
pub const PULL_BODY_LIMIT_BYTES: usize = 32 * 1024 * 1024;
/// Bytes of a push request body around its events: `{"events":[]}`.
pub const PUSH_ENVELOPE_BYTES: usize = 13;
/// The most a real sequence number adds to the one digit [`MAX_EVENT_WIRE_BYTES`] counts.
const SEQ_EXTRA_BYTES: usize = 19;

// One event at the cap fits a batch and a page, and each budget fits under its limit. An
// event that arrived by push is at most the push limit, so a pull answer that carries it
// alone has at least as much room again for the node directory: whatever a hub accepted,
// it can relay. The build fails when a change breaks one of these.
const _: () = {
    let one_event = MAX_EVENT_WIRE_BYTES + SEQ_EXTRA_BYTES + PUSH_ENVELOPE_BYTES;
    assert!(one_event <= PUSH_BATCH_BUDGET_BYTES);
    assert!(one_event <= PULL_PAGE_BUDGET_BYTES);
    assert!(PUSH_BATCH_BUDGET_BYTES <= PUSH_BODY_LIMIT_BYTES);
    assert!(PULL_PAGE_BUDGET_BYTES <= PULL_BODY_LIMIT_BYTES);
    assert!(2 * PUSH_BODY_LIMIT_BYTES <= PULL_BODY_LIMIT_BYTES);
};

/// Counts the bytes written to it.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &impl Serialize) -> usize {
    let mut count = ByteCount(0);
    // Writing to the counter cannot fail, and these types always serialise.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

impl WireEvent {
    /// The bytes of this event's JSON as sync sends it.
    pub fn wire_bytes(&self) -> usize {
        json_bytes(self)
    }

    /// [`WireEvent::wire_bytes`] with the sequence number counted as one digit: the size
    /// [`MAX_EVENT_WIRE_BYTES`] is compared with.
    pub fn capped_bytes(&self) -> usize {
        self.wire_bytes() + 1 - self.seq.to_string().len()
    }
}

/// The fields of an event about to be written, in [`WireEvent`]'s order, for measuring it
/// before it has a sequence number.
#[derive(Serialize)]
pub(crate) struct NewEvent<'a> {
    pub seq: i64,
    pub event_id: &'a str,
    pub node_id: &'a str,
    pub ts: i64,
    pub actor: &'a str,
    pub kind: &'a str,
    pub payload: &'a str,
    pub prev_id: Option<&'a str>,
    pub sig: &'a str,
}

/// Refuse a local event over [`MAX_EVENT_WIRE_BYTES`] before it is written. Each field that
/// can grow has its own smaller rule; this is the backstop for what they do not cover, such
/// as a message to thousands of recipients.
pub(crate) fn check_new_event(event: &NewEvent<'_>) -> Result<()> {
    let bytes = json_bytes(event);
    if bytes > MAX_EVENT_WIRE_BYTES {
        return Err(Error::Invalid(format!(
            "this {} event would be {bytes} bytes when sent to another node; the limit for \
             one event is {MAX_EVENT_WIRE_BYTES}",
            event.kind
        )));
    }
    Ok(())
}

/// An event already in a log, written by an earlier build, that does not fit the transport
/// even alone: a spoke's own event that makes a push over [`PUSH_BODY_LIMIT_BYTES`], or an
/// event on a hub that makes a pull answer over [`PULL_BODY_LIMIT_BYTES`]. Every smaller
/// event is sent, alone in its batch or page if need be, whatever its size.
///
/// UNDECIDED, THE OWNER'S CHOICE (D99): this is the one place that says what happens to
/// such an event. As built it blocks: the sync fails with this message and the events after
/// it wait, which is what a build before D99 did (at 2 MiB and 10 MiB) without naming the
/// event. The other choice is to skip it: the callers would step past it instead of
/// returning this error, and peers would then never see that event.
fn over_limit_in_log(e: &WireEvent, whose: &str, reader: &str, limit: usize) -> Error {
    Error::Conflict(format!(
        "event {} ({}, {} bytes on the wire) in {whose} log is larger than {reader} \
         ({limit} bytes), so it cannot be sent and the events after it wait behind it; \
         retrying will not help; the owner must decide whether it is skipped or keeps \
         blocking (docs/FEDERATION.md, \"If a sync reports an event that cannot be sent\")",
        crate::mail::single_line(&e.event_id, 40),
        crate::mail::single_line(&e.kind, 80),
        e.wire_bytes(),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEntry {
    pub node_id: String,
    pub display_name: String,
    pub public_key: String,
}

/// Contents of an invite code (`another pair invite`). `sig` is the hub's signature over
/// [`crypto::invite_message`], which covers every other field; a joiner checks it against
/// the hub key the invite names before it sends anything. An invite from a build that did
/// not sign has no `sig` and is refused.
#[derive(Clone, Serialize, Deserialize)]
pub struct Invite {
    pub v: u8,
    pub hub_url: String,
    pub hub: NodeEntry,
    pub invite_id: String,
    pub secret: String,
    pub perms: Vec<Perm>,
    pub expires_at: i64,
    #[serde(default)]
    pub sig: String,
}

/// What is wrong with an invite's signature.
enum InviteSignature {
    Missing,
    /// A signed field holds a line break, so the signed bytes would be ambiguous.
    LineBreak,
    Wrong,
}

impl Invite {
    /// The bytes the hub signed for this invite.
    fn signed_message(&self) -> Vec<u8> {
        crypto::invite_message(
            self.v,
            &self.hub.node_id,
            &self.hub.public_key,
            &self.hub.display_name,
            &self.hub_url,
            &self.invite_id,
            &crypto::sha256_hex(self.secret.as_bytes()),
            &perms_text(&self.perms),
            self.expires_at,
        )
    }

    /// Check `sig` against the hub key this invite names. A key that does not decode, or a
    /// weak one, fails as a wrong signature.
    fn check_signature(&self) -> std::result::Result<(), InviteSignature> {
        if self.sig.is_empty() {
            return Err(InviteSignature::Missing);
        }
        let texts = [
            &self.hub.node_id,
            &self.hub.public_key,
            &self.hub.display_name,
            &self.hub_url,
            &self.invite_id,
        ];
        if texts.iter().any(|t| t.contains(['\n', '\r'])) {
            return Err(InviteSignature::LineBreak);
        }
        if crypto::verify_strict(&self.hub.public_key, &self.signed_message(), &self.sig) {
            Ok(())
        } else {
            Err(InviteSignature::Wrong)
        }
    }
}

/// Whether `url` can be a hub's address: an `http://` or `https://` URL with a host. The
/// hub applies it to the address it signs into an invite and the joiner to the address it
/// is about to dial, so both sides share one rule. It checks the form only.
pub fn hub_url_is_http(url: &str) -> bool {
    url.parse::<http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some_and(|h| !h.is_empty())
    })
}

/// A node that a hub's signed list names with another key than the one this node pinned
/// for it. Both keys are shown as fingerprints, computed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyConflict {
    pub node_id: String,
    pub listed_fingerprint: String,
    pub pinned_fingerprint: String,
}

impl KeyConflict {
    /// The audit detail for this refusal.
    pub fn detail(&self) -> String {
        format!(
            "node list names {} with key {}; pinned {}",
            self.node_id, self.listed_fingerprint, self.pinned_fingerprint
        )
    }

    /// The error a sync fails with for this conflict.
    pub fn refusal(&self) -> Error {
        Error::Conflict(format!(
            "the hub's signed node list gives node {} the key {}, but this node pinned the \
             key {} for it earlier; the answer is refused and this node cannot sync. A \
             node's key never changes, so one of the two is wrong. Compare both with the \
             fingerprint that `another node show` prints on that node. If the pinned key is \
             the wrong one, this home must be replaced; if the hub's is, the hub's home \
             must be: see \"A pinned key that is wrong\" in docs/FEDERATION.md",
            self.node_id, self.listed_fingerprint, self.pinned_fingerprint
        ))
    }
}

/// `listed` against the key pinned here for its id: the conflict when a different key is
/// stored. This node's own id, and an id with no key stored, never conflict.
fn key_conflict(
    conn: &Connection,
    own_id: &str,
    listed: &NodeEntry,
) -> Result<Option<KeyConflict>> {
    if listed.node_id == own_id {
        return Ok(None);
    }
    let pinned: Option<String> = conn
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [&listed.node_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(pinned
        .filter(|key| *key != listed.public_key)
        .map(|key| KeyConflict {
            node_id: listed.node_id.clone(),
            listed_fingerprint: crypto::fingerprint(&listed.public_key),
            pinned_fingerprint: crypto::fingerprint(&key),
        }))
}

/// The pairing secret lets a node join until the invite expires, so `Debug` never prints
/// it. `Serialize` keeps it: the invite code is built from it.
impl std::fmt::Debug for Invite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invite")
            .field("v", &self.v)
            .field("hub_url", &self.hub_url)
            .field("hub", &self.hub)
            .field("invite_id", &self.invite_id)
            .field("secret", &"<redacted>")
            .field("perms", &self.perms)
            .field("expires_at", &self.expires_at)
            .field("sig", &self.sig)
            .finish()
    }
}

/// What a joining node sends to `/fed/pair`. It holds no secret: the invite's secret never
/// leaves the joiner. `sig` is the joiner's signature over [`crypto::pair_message`], which
/// includes the hash of the secret, so only a holder of the invite and of the node key can
/// make it. A request from a build before signed pairing has no `sig` and is refused; its
/// `secret` field is not a field here, so it is never read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub invite_id: String,
    pub node: NodeEntry,
    #[serde(default)]
    pub time_ms: i64,
    #[serde(default)]
    pub sig: String,
}

/// The hub's answer to a pairing request. `sig` is the hub's signature over
/// [`crypto::pair_accept_message`]; the joiner checks it against the hub key in the invite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub hub: NodeEntry,
    pub perms: Vec<Perm>,
    #[serde(default)]
    pub sig: String,
}

/// Why a pairing request failed the invite check. The caller is told one text for all four;
/// the hub's audit row records which one it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairRefusal {
    UnknownInvite,
    /// `used_by` is the node id stored with the invite, or empty.
    UsedInvite {
        used_by: String,
    },
    ExpiredInvite,
    BadSignature,
}

impl PairRefusal {
    /// The audit detail for this cause.
    pub fn detail(&self) -> String {
        match self {
            PairRefusal::UnknownInvite => "unknown invite".into(),
            PairRefusal::UsedInvite { used_by } => format!("used invite (by {used_by})"),
            PairRefusal::ExpiredInvite => "expired invite".into(),
            PairRefusal::BadSignature => "bad signature".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub events: Vec<WireEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rejected {
    pub event_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IngestReport {
    pub accepted: usize,
    pub duplicates: usize,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub events: Vec<WireEvent>,
    /// Hub sequence to pass as `after` next time.
    pub cursor: i64,
    pub more: bool,
    /// Every node the hub knows (including itself), for addressing and signature checks.
    pub nodes: Vec<NodeEntry>,
    /// The hub's log no longer contains what this spoke already received: the hub was
    /// restored from an older copy, or replaced. No events are sent; the spoke rewinds both
    /// cursors to zero and syncs again (ingest skips duplicates). Absent from older hubs.
    #[serde(default)]
    pub reset: bool,
}

/// What a spoke tells its hub on a pull so the hub can tell whether it was restored from an
/// older copy (see [`Store::pull_for_from`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullAnchors {
    /// The last event the hub sent this spoke.
    pub received: Option<String>,
    /// The hub's `seq` for `received`, as sent. A restored hub can hold that event again,
    /// pushed back by its author, but then at another `seq`.
    pub received_seq: Option<i64>,
    /// The last event of this spoke's own that the hub accepted. Only this spoke pushes its
    /// own events, so no other spoke can put it back on a restored hub.
    pub pushed: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Peer {
    pub node_id: String,
    pub display_name: String,
    /// `hub` (this node dials it) or `spoke` (it dials this node).
    pub role: String,
    pub url: Option<String>,
    /// For a spoke: what it may do here. For the hub: what it granted this node.
    pub perms: Vec<Perm>,
    pub fingerprint: String,
    pub paired_at: String,
    pub last_sync_at: Option<String>,
    pub revoked_at: Option<String>,
    pub push_cursor: i64,
    pub pull_cursor: i64,
}

/// How an event may travel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Share {
    /// Stays on its node (security records, pairing, local-only mail).
    Never,
    /// Identity: node, project and agent registrations — sent to every peer.
    Directory,
    /// Activity: sent to peers with `read_feed`, plus these nodes regardless.
    Feed(Vec<String>),
    /// Mail: only to the nodes that host a recipient (or the message's origin).
    Mail(Vec<String>),
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn classify(kind: &str, payload: &Value, origin: &str) -> Share {
    let not_origin =
        |nodes: Vec<String>| -> Vec<String> { nodes.into_iter().filter(|n| n != origin).collect() };
    match kind.split('.').next().unwrap_or_default() {
        // The queue is local to its node: no queue event is ever shared.
        "token" | "peer" | "oauth" | "queue" => return Share::Never,
        _ => {}
    }
    match kind {
        "node.created" | "node.renamed" | "project.ensured" | "agent.registered" => {
            Share::Directory
        }
        "message.sent" => {
            let nodes = not_origin(strings(payload, "recipient_nodes"));
            if nodes.is_empty() {
                Share::Never
            } else {
                Share::Mail(nodes)
            }
        }
        "delivery.read" | "delivery.acked" => {
            match payload.get("message_node").and_then(Value::as_str) {
                Some(n) if n != origin => Share::Mail(vec![n.to_string()]),
                _ => Share::Never,
            }
        }
        _ => {
            let mut also = strings(payload, "resource_nodes");
            if let Some(n) = payload.get("resource_node").and_then(Value::as_str) {
                also.push(n.to_string());
            }
            Share::Feed(not_origin(also))
        }
    }
}

fn perms_of(text: &str) -> Vec<Perm> {
    serde_json::from_str(text).unwrap_or_default()
}

/// A node id is a ULID as `ulid::Ulid::new().to_string()` writes it, which is how every
/// build has made node ids: exactly 26 ASCII characters from `0123456789ABCDEFGHJKMNPQRSTVWXYZ`,
/// the first one `0` to `7`. Anything else is not an id, whoever sends it.
pub fn validate_node_id(node_id: &str) -> Result<()> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = node_id.as_bytes();
    let ok = bytes.len() == 26
        && (b'0'..=b'7').contains(&bytes[0])
        && bytes.iter().all(|b| ALPHABET.contains(b));
    if ok {
        Ok(())
    } else {
        let shown: String = node_id.chars().take(40).collect();
        Err(Error::Invalid(format!(
            "invalid node id {:?}: a node id is 26 characters from \
             0123456789ABCDEFGHJKMNPQRSTVWXYZ, the first one 0 to 7",
            crate::mail::single_line(&shown, usize::MAX)
        )))
    }
}

/// [`crypto::decode_peer_public`], and the text must be the one form `crypto::b64` writes:
/// decoding trims, so without this two texts would name one key.
fn canonical_peer_public(text: &str) -> Result<()> {
    let key = crypto::decode_peer_public(text)?;
    if crypto::b64(key.as_bytes()) != text {
        return Err(Error::Invalid(
            "node public key is not in its canonical text form".into(),
        ));
    }
    Ok(())
}

/// The name rule, and not the `local` placeholder: what a node must be called to pair.
fn validate_pairing_name(name: &str) -> Result<()> {
    validate_node_name(name)?;
    if name == "local" {
        return Err(Error::Invalid(
            "a node named `local` is not named yet and cannot pair; name it first: \
             `another node name <name>`"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_node_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid node name {name:?}: use 1-40 ASCII letters, digits, '-', '_' or '.'"
        )))
    }
}

fn local_entry(conn: &Connection) -> Result<NodeEntry> {
    Ok(conn.query_row(
        "SELECT node_id, display_name, public_key FROM nodes WHERE is_local = 1",
        [],
        |r| {
            Ok(NodeEntry {
                node_id: r.get(0)?,
                display_name: r.get(1)?,
                public_key: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )?)
}

fn require_named(entry: &NodeEntry) -> Result<()> {
    if entry.display_name == "local" {
        return Err(Error::Invalid(
            "give this node a unique name before pairing: `another node name <name>`".into(),
        ));
    }
    // A home made by an older build with an unchecked ANOTHER_NODE_NAME can hold a name a
    // rename would refuse. The name is not quoted: it is exactly the text that is not safe.
    if validate_node_name(&entry.display_name).is_err() {
        return Err(Error::Invalid(
            "this node's name is not valid (use 1-40 ASCII letters, digits, '-', '_' or '.'); \
             rename it with `another node name <name>`"
                .into(),
        ));
    }
    Ok(())
}

/// Whether `name` already names a node other than `node_id`. A node is addressed by its
/// display name or its id (`identity.rs` `resolve_remote`), so another node's id is taken
/// too: otherwise `name@project@<node id>`, the form that resolves an ambiguity, is ambiguous.
/// Case is ignored: `Alpha` and `alpha` are one name, since a person reads them as one.
fn node_name_taken(conn: &Connection, name: &str, node_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM nodes
             WHERE (display_name = ?1 COLLATE NOCASE OR node_id = ?1 COLLATE NOCASE)
               AND node_id != ?2",
            params![name, node_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Whether `node_id` is already another node's display name. Checked before a node is first
/// recorded, the inverse of [`node_name_taken`]: a new node whose id is someone's name would
/// make that identifier match two nodes.
fn node_id_is_a_name(conn: &Connection, node_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM nodes WHERE display_name = ?1 COLLATE NOCASE AND node_id != ?1",
            [node_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The refusal for a node that is new here and whose id is already another node's name.
fn id_is_a_name(node_id: &str) -> Error {
    Error::Conflict(format!(
        "node {node_id}'s id is already another node's name; rename that node first"
    ))
}

/// Permissions as a pairing answer's signature covers them: each name, joined by `,`.
fn perms_text(perms: &[Perm]) -> String {
    perms
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// `listed` as a peer's node list may apply it, or `None` to skip it. The name is used only
/// when a local rename would accept it: valid, not the `local` placeholder, and not another
/// node's. Otherwise a known node keeps its name, and an unknown one is recorded under its
/// id, so its key is still learned and its signed events still verify. An unknown node whose
/// id is already another node's name is skipped: recording it would make that identifier
/// match two nodes, and skipping one entry keeps the rest of the sync going. An entry whose
/// id is not a node id is skipped the same way.
fn listed_entry(tx: &Tx<'_>, listed: &NodeEntry) -> Result<Option<NodeEntry>> {
    if validate_node_id(&listed.node_id).is_err() {
        return Ok(None);
    }
    let known = tx
        .query_row(
            "SELECT display_name FROM nodes WHERE node_id = ?1",
            [&listed.node_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if known.is_none() && node_id_is_a_name(tx, &listed.node_id)? {
        return Ok(None);
    }
    let usable = validate_node_name(&listed.display_name).is_ok()
        && listed.display_name != "local"
        && !node_name_taken(tx, &listed.display_name, &listed.node_id)?;
    let display_name = if usable {
        listed.display_name.clone()
    } else {
        known.unwrap_or_else(|| listed.node_id.clone())
    };
    Ok(Some(NodeEntry {
        display_name,
        ..listed.clone()
    }))
}

/// Record (or confirm) another node from a hub's list or a hub's signed answer to a join.
/// Keys are pinned on first sight. The hub side of pairing does not come through here: it
/// never writes a key on a node it already knows (see [`Store::accept_pairing`]).
fn upsert_node(tx: &Tx<'_>, node: &NodeEntry) -> Result<()> {
    if node.node_id == tx.node_id {
        return Ok(());
    }
    crypto::decode_peer_public(&node.public_key)?;
    // The name prefixes every feed line from that node and is listed by `another node`.
    // Callers pass it through `listed_entry` first; this keeps it one safe line regardless.
    let display_name = crate::mail::single_line(&node.display_name, usize::MAX);
    let known: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT display_name, public_key FROM nodes WHERE node_id = ?1",
            [&node.node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match known {
        Some((_, Some(key))) if key != node.public_key => Err(Error::Conflict(format!(
            "node {} presented a different key than before; refusing",
            node.node_id
        ))),
        Some((name, _)) => {
            if name != display_name {
                tx.execute(
                    "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                    params![node.node_id, display_name],
                )?;
            }
            tx.execute(
                "UPDATE nodes SET public_key = ?2 WHERE node_id = ?1 AND public_key IS NULL",
                params![node.node_id, node.public_key],
            )?;
            Ok(())
        }
        None => {
            if node_id_is_a_name(tx, &node.node_id)? {
                return Err(id_is_a_name(&node.node_id));
            }
            tx.execute(
                "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
                 VALUES (?1, ?2, 'peer', 0, ?3, ?4)",
                params![node.node_id, display_name, tx.now, node.public_key],
            )?;
            Ok(())
        }
    }
}

fn wire_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WireEvent> {
    Ok(WireEvent {
        seq: r.get(0)?,
        event_id: r.get(1)?,
        node_id: r.get(2)?,
        ts: r.get(3)?,
        actor: r.get(4)?,
        kind: r.get(5)?,
        payload: r.get(6)?,
        prev_id: r.get(7)?,
        sig: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
    })
}

const WIRE_SELECT: &str =
    "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig FROM events";

/// The anchor for a spoke's next pull and the hub `seq` it was sent at: the newest stored
/// foreign event (no `seq` known), or the `anchor` and `seq` of a newer local
/// `peer.anchored` / `peer.resynced` record. See [`Store::last_relayed_event_id`].
fn relayed_anchor(conn: &Connection, local: &str) -> Result<Option<(String, Option<i64>)>> {
    let newest: Option<(String, String, String)> = conn
        .query_row(
            "SELECT kind, event_id, payload FROM events
             WHERE node_id != ?1
                OR (node_id = ?1 AND kind IN ('peer.resynced', 'peer.anchored'))
             ORDER BY seq DESC LIMIT 1",
            [local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match newest {
        Some((kind, _, payload)) if kind == "peer.resynced" || kind == "peer.anchored" => {
            let p = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            p.get("anchor")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), p.get("seq").and_then(Value::as_i64)))
        }
        Some((_, event_id, _)) => Some((event_id, None)),
        None => None,
    })
}

/// The newest of this spoke's own events its hub accepted: the `anchor` of the newest local
/// `peer.pushed` or `peer.resynced` record. A resync records it again, or `null` when the
/// restored hub accepted nothing, so an event the hub later refuses cannot stay the anchor.
fn pushed_anchor(conn: &Connection, local: &str) -> Result<Option<String>> {
    let newest: Option<(String, String)> = conn
        .query_row(
            "SELECT kind, payload FROM events
             WHERE node_id = ?1 AND kind IN ('peer.pushed', 'peer.resynced')
             ORDER BY seq DESC LIMIT 1",
            [local],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(newest.and_then(|(kind, payload)| {
        let p = serde_json::from_str::<Value>(&payload).ok()?;
        let key = if kind == "peer.pushed" {
            "anchor"
        } else {
            "pushed"
        };
        p.get(key)?.as_str().map(str::to_string)
    }))
}

/// Every node with a key, for a pull response's addressing and signature checks.
fn known_nodes(conn: &Connection) -> Result<Vec<NodeEntry>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, display_name, public_key FROM nodes WHERE public_key IS NOT NULL",
    )?;
    let nodes = stmt
        .query_map([], |r| {
            Ok(NodeEntry {
                node_id: r.get(0)?,
                display_name: r.get(1)?,
                public_key: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(nodes)
}

/// (role, perms, revoked_at)
type PeerRow = (String, Vec<Perm>, Option<i64>);

/// A `peers` row for this node itself is never acted on. No command of this build writes
/// one; a build before the joiner checked the hub id could, from an invite that named the
/// node's own id. Every reader that decides something from a `peers` row (this node's hub,
/// who is a paired spoke, whose signed requests are accepted) carries this condition, so
/// such a row is inert: it is only shown, marked, by `another peers` and `another doctor`.
const NOT_SELF: &str = "node_id NOT IN (SELECT node_id FROM nodes WHERE is_local = 1)";

fn peer_row(conn: &Connection, node_id: &str) -> Result<Option<PeerRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT role, perms, revoked_at FROM peers WHERE node_id = ?1 AND {NOT_SELF}"),
            [node_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?)),
        )
        .optional()?
        .map(|(role, perms, revoked)| (role, perms_of(&perms), revoked)))
}

/// Refuses to join `node_id` as a hub when this node already has it paired in another role.
/// A join writes only a `hub` row: otherwise an invite made by one of this node's spokes
/// would give that spoke the permissions it named and clear its revoke.
fn not_paired_as_spoke(conn: &Connection, node_id: &str) -> Result<()> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT p.role, n.display_name FROM peers p JOIN nodes n ON n.node_id = p.node_id
             WHERE p.node_id = ?1",
            [node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((role, name)) if role != "hub" => Err(Error::Conflict(format!(
            "node {name} is paired here as a spoke; it cannot also be this node's hub"
        ))),
        _ => Ok(()),
    }
}

/// The refusal for a join while this node syncs with another hub.
fn second_hub() -> Error {
    Error::Conflict(
        "this node already syncs with another hub; revoke it first: `another pair revoke <hub>`"
            .into(),
    )
}

/// The joiner's identity rule, for the node an invite names as the hub. Everything here is
/// decided by what this node already stores, so it is checked before anything is sent and
/// again before anything is written:
///
/// - the id is not this node's own;
/// - a row stored for the id has a key (a keyless row is never given one by a join, as on
///   the hub's side), and the invite carries that key;
/// - the id is not paired here in a role other than `hub`;
/// - this node does not sync with a different hub.
fn may_join_as_hub(conn: &Connection, own_id: &str, hub: &NodeEntry) -> Result<()> {
    if hub.node_id == own_id {
        return Err(Error::Conflict(
            "this invite names this node itself as its hub; a node cannot join itself".into(),
        ));
    }
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [&hub.node_id],
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        Some(None) => {
            return Err(Error::Conflict(format!(
                "node {} is known here without a key; it cannot be joined",
                hub.node_id
            )));
        }
        Some(Some(key)) if key != hub.public_key => {
            return Err(Error::Conflict(format!(
                "node {} presented a different key than before; refusing",
                hub.node_id
            )));
        }
        _ => {}
    }
    not_paired_as_spoke(conn, &hub.node_id)?;
    match hub_of(conn)? {
        Some((existing, _, _)) if existing != hub.node_id => Err(second_hub()),
        _ => Ok(()),
    }
}

/// An invite id is a ULID, as the hub makes it (the same form as a node id). It goes into
/// both signed messages, so it is checked before either is built. The refusal is the fixed
/// text for an invite that does not decode: the id is part of the code.
fn validate_invite_id(invite_id: &str) -> Result<()> {
    validate_node_id(invite_id).map_err(|_| Error::Invalid("invalid invite code".into()))
}

/// The refusal of `pair revoke`, `pair readmit` and `pair release-name` for this node's own
/// id. No command makes a pairing row for the node itself; a home that has one from an
/// older build must not end its own leases or rename itself through these commands.
fn not_this_node(tx: &Tx<'_>, node_id: &str) -> Result<()> {
    if node_id == tx.node_id {
        return Err(Error::Invalid(format!(
            "node {node_id} is this node itself; `another pair revoke`, `readmit` and \
             `release-name` act on other nodes only"
        )));
    }
    Ok(())
}

/// The hub this node syncs with, if it is a spoke: (node_id, url, granted perms).
pub(crate) fn hub_of(conn: &Connection) -> Result<Option<(String, String, Vec<Perm>)>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT node_id, url, perms FROM peers
                 WHERE role = 'hub' AND revoked_at IS NULL AND {NOT_SELF} LIMIT 1"
            ),
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(n, u, p)| (n, u, perms_of(&p))))
}

/// Whether this node may act on `perm` towards `node` (another node).
pub(crate) fn allowed_towards(conn: &Connection, node: &str, perm: Perm) -> Result<()> {
    if let Some((_, _, granted)) = hub_of(conn)? {
        if granted.contains(&perm) {
            return Ok(());
        }
        return Err(Error::Forbidden(format!(
            "the hub did not grant this node {}",
            perm.as_str()
        )));
    }
    // A hub may act towards any node paired with it.
    match peer_row(conn, node)? {
        Some((role, _, None)) if role == "spoke" => Ok(()),
        _ => Err(Error::Forbidden(format!(
            "node {node} is not paired with this node"
        ))),
    }
}

/// TCP reachability of a foreign resource from this machine.
pub fn reachable(endpoint: Option<&str>, port: Option<u16>) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let host = endpoint
        .map(|e| e.split_once("://").map_or(e, |(_, rest)| rest))
        .map(|e| e.split('/').next().unwrap_or_default().to_string());
    let (host, port) = match host {
        Some(h) if h.starts_with('[') => {
            let (addr, rest) = h.split_once(']').unwrap_or((&h, ""));
            let p = rest.trim_start_matches(':').parse().ok().or(port);
            (addr.trim_start_matches('[').to_string(), p)
        }
        Some(h) => match h.rsplit_once(':') {
            Some((host, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (host.to_string(), p.parse().ok().or(port))
            }
            _ => (h, port),
        },
        None => return false,
    };
    let Some(port) = port else { return false };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, std::time::Duration::from_millis(500)).is_ok())
}

fn verify_wire(conn: &Connection, ev: &WireEvent) -> Result<()> {
    let key: Option<String> = conn
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [&ev.node_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(key) = key else {
        return Err(Error::Forbidden(format!(
            "unknown origin node {}",
            ev.node_id
        )));
    };
    let msg = crypto::event_message(
        &ev.event_id,
        &ev.node_id,
        ev.ts,
        &ev.actor,
        &ev.kind,
        &ev.payload,
    );
    // Strict: a weak key pinned before keys were checked cannot verify the degenerate
    // signature (FINDINGS-main-weak-keys).
    if crypto::verify_strict(&key, &msg, &ev.sig) {
        Ok(())
    } else {
        Err(Error::Forbidden("bad event signature".into()))
    }
}

/// Permission checks the hub applies to events a spoke pushes.
fn spoke_may_publish(
    conn: &Connection,
    perms: &[Perm],
    from: &str,
    ev: &WireEvent,
    payload: &Value,
) -> Result<()> {
    if ev.node_id != from {
        return Err(Error::Forbidden(
            "spokes may only push events they authored".into(),
        ));
    }
    match classify(&ev.kind, payload, &ev.node_id) {
        Share::Never => {
            return Err(Error::Forbidden(format!(
                "{} events are not shared",
                ev.kind
            )))
        }
        Share::Mail(_) if ev.kind == "message.sent" && !perms.contains(&Perm::SendMail) => {
            return Err(Error::Forbidden(
                "this node may not send mail to other nodes".into(),
            ))
        }
        _ => {}
    }
    // Whether a borrow reaches another node's resource is decided by the resource this node
    // knows, never by the routing hint (`resource_node`) the event's author chose. A
    // resource this node does not know cannot be shown to be the spoke's own.
    if ev.kind == "resource.borrowed" && !perms.contains(&Perm::BorrowResource) {
        let rid = payload.get("resource_id").and_then(Value::as_str);
        let owner = match rid {
            Some(id) => owner_of(conn, "resources", "resource_id", id)?,
            None => None,
        };
        if owner.as_deref() != Some(ev.node_id.as_str()) {
            return Err(Error::Forbidden(
                "this node may not borrow resources of other nodes".into(),
            ));
        }
    }
    Ok(())
}

/// The node that owns the row `id` in `table`, if this node knows it.
fn owner_of(conn: &Connection, table: &str, column: &str, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            &format!("SELECT node_id FROM {table} WHERE {column} = ?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// A signature proves only who sent an event. Before a foreign event is stored, check that
/// every agent, project, message, hold and resource it would create or change belongs to
/// its origin node. An object this node does not know yet cannot be rewritten, so it
/// passes; the projection then records it under the origin. Mail may name recipients and
/// a borrow may name a resource on any node: those are the cross-node operations
/// federation exists for, and the permission checks in `spoke_may_publish` cover them.
fn foreign_authority(conn: &Connection, origin: &str, kind: &str, p: &Value) -> Result<()> {
    let owned = |what: &str, table: &str, column: &str, key: &str| -> Result<()> {
        let Some(id) = p.get(key).and_then(Value::as_str) else {
            return Ok(());
        };
        match owner_of(conn, table, column, id)? {
            Some(node) if node != origin => Err(Error::Forbidden(format!(
                "{kind} names {what} {id}, which belongs to another node"
            ))),
            _ => Ok(()),
        }
    };
    let agent = |key: &str| owned("agent", "agents", "agent_id", key);
    let project = || owned("project", "projects", "project_id", "project_id");
    let resource = || owned("resource", "resources", "resource_id", "resource_id");
    match kind {
        // Pairing refuses a name another node has; a later rename must not take one either,
        // or two nodes' agents would share `name@@node` addresses.
        "node.created" | "node.renamed" => {
            // `local` is the placeholder every node starts with (its first `node.created`
            // carries it) and that pairing refuses, so it names no one.
            let Some(name) = p
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|n| *n != "local")
            else {
                return Ok(());
            };
            // The local rename rule. `set_node_name` always applies it; a `node.created`
            // whose name came unchecked from ANOTHER_NODE_NAME is refused once, and pairing
            // needs a valid rename first anyway.
            validate_node_name(name)?;
            if node_name_taken(conn, name, origin)? {
                return Err(Error::Forbidden(format!(
                    "{kind} names {name:?}, which another node already has"
                )));
            }
            Ok(())
        }
        "agent.registered" => {
            agent("agent_id")?;
            // The local name rule, so a peer without it (a changed build or a forged event)
            // cannot add an agent whose name is oversized, look-alike or not addressable.
            // A missing name counts as empty and is refused.
            crate::identity::validate_name(p.get("name").and_then(Value::as_str).unwrap_or(""))?;
            project()
        }
        "session.started" | "session.ended" | "delivery.read" | "delivery.acked" => {
            agent("agent_id")
        }
        "message.sent" => {
            agent("sender_id")?;
            // The same body limit as a local send, so a peer without it (a changed build or
            // a forged event) cannot store or relay an oversized body. A body that is not
            // text is refused too: it would skip the limit and still be kept in the event.
            // The subject is not checked here: the projection cuts it and stores it (D75),
            // so an over-long subject never costs the recipient the message.
            match p.get("body") {
                None | Some(Value::Null) => {}
                Some(Value::String(body)) => crate::mail::check_body(body)?,
                Some(_) => return Err(Error::Invalid("message body must be text".into())),
            }
            owned("message", "messages", "message_id", "message_id")
        }
        "hold.acquired" | "hold.updated" => {
            agent("agent_id")?;
            project()?;
            owned("hold", "holds", "hold_id", "hold_id")
        }
        "resource.registered" => {
            agent("owner_id")?;
            project()?;
            // The local resource-name rule. A missing name counts as empty and is refused.
            crate::resources::validate_name(p.get("name").and_then(Value::as_str).unwrap_or(""))?;
            // The local size limits. Control characters in these two are cleaned by the
            // projection (D97); a size over the limit cannot be cleaned, so it is refused.
            crate::resources::check_text_sizes(
                p.get("endpoint").and_then(Value::as_str),
                p.get("purpose").and_then(Value::as_str),
            )?;
            // The same metadata rule as a local registration: an object of at most 64 KiB.
            // An event without metadata is stored with `{}`.
            if let Some(metadata) = p.get("metadata") {
                crate::resources::check_metadata(metadata)?;
            }
            // A local registration types these as u16 / u32; refuse what no build sends.
            for (field, max) in [("port", u64::from(u16::MAX)), ("pid", u64::from(u32::MAX))] {
                if let Some(v) = p.get(field).filter(|v| !v.is_null()) {
                    if v.as_u64().is_none_or(|n| n > max) {
                        return Err(Error::Invalid(format!(
                            "resource {field} {v} is not a valid {field}"
                        )));
                    }
                }
            }
            resource()
        }
        "resource.adopted" => {
            agent("agent_id")?;
            resource()
        }
        "resource.borrowed" => agent("agent_id"),
        "resource.transferred" => {
            resource()?;
            // The lease's resource decides, whatever resource_id the payload claims.
            let lease_resource: Option<String> = match p.get("lease_id").and_then(Value::as_str) {
                Some(lease) => conn
                    .query_row(
                        "SELECT r.node_id FROM resource_leases l
                         JOIN resources r ON r.resource_id = l.resource_id WHERE l.lease_id = ?1",
                        [lease],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            };
            match lease_resource {
                Some(node) if node != origin => Err(Error::Forbidden(format!(
                    "{kind} names a lease on a resource that belongs to another node"
                ))),
                _ => Ok(()),
            }
        }
        "resource.stopped" | "resource.reaped" => resource(),
        _ => Ok(()),
    }
}

fn agent_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM agents WHERE agent_id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn project_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM projects WHERE project_id = ?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// Fold a foreign event into local read models. Idempotent; unknown kinds are only stored.
///
/// The origin chose the event's time and every TTL in it. Times are therefore taken no
/// later than this node's clock, and TTLs are clamped to the limits a local caller gets, so
/// a skewed clock or a huge TTL cannot keep a foreign hold or lease alive here (or put
/// mail, presence or receipts in the future).
fn project_foreign(tx: &Tx<'_>, ev: &WireEvent, p: &Value) -> Result<()> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    // A peer's free text skips the local checks: hold reasons, programs, models,
    // descriptions and a resource's endpoint and purpose are stored as one safe line, like a
    // subject (D75); the signed event keeps the original. The name of an agent or a resource
    // is not cleaned: `foreign_authority` holds it to the local name rule, so it is stored
    // as sent. Held patterns, project paths and slugs name real files and are matched
    // across nodes, so they are kept as sent.
    let line = |k: &str| s(k).map(|t| crate::mail::single_line(&t, usize::MAX));
    let ts = ev.ts.min(tx.now);
    let origin = ev.node_id.as_str();
    match ev.kind.as_str() {
        "node.created" | "node.renamed" => {
            // `local` means "not named yet" and names no one, so it is never applied: the
            // event is stored and relayed (an honest node's first `node.created` carries
            // it), and the node keeps the name pairing or a hub's list gave it.
            if let Some(name) = s("display_name").filter(|n| n != "local") {
                tx.execute(
                    "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                    params![origin, name],
                )?;
            }
        }
        "project.ensured" => {
            tx.execute(
                "INSERT OR IGNORE INTO projects (project_id, node_id, abs_path, slug, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![s("project_id"), origin, s("abs_path"), s("slug"), ts, ev.event_id],
            )?;
        }
        "agent.registered" => {
            let Some(agent_id) = s("agent_id") else {
                return Ok(());
            };
            let name = s("name").unwrap_or_default();
            let project_id = s("project_id").filter(|id| project_exists(tx, id).unwrap_or(false));
            let slug: Option<String> = match &project_id {
                Some(id) => tx
                    .query_row(
                        "SELECT slug FROM projects WHERE project_id = ?1",
                        [id],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            };
            let node_name: String = tx.query_row(
                "SELECT display_name FROM nodes WHERE node_id = ?1",
                [origin],
                |r| r.get(0),
            )?;
            // The stored slug stays as sent, since it is matched against local slugs; in an
            // address, which is printed and typed, it is one safe line.
            let address = format!(
                "{name}@{}@{node_name}",
                crate::mail::single_line(&slug.unwrap_or_default(), usize::MAX)
            );
            tx.execute(
                "INSERT INTO agents (agent_id, node_id, project_id, name, program, model, description,
                   status, created_at, updated_at, last_seen_at, event_id, address)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?8, ?8, ?9, ?10)
                 ON CONFLICT(agent_id) DO UPDATE SET program = excluded.program,
                   model = excluded.model, description = excluded.description,
                   updated_at = excluded.updated_at, last_seen_at = excluded.last_seen_at,
                   status = 'active', address = excluded.address",
                params![
                    agent_id,
                    origin,
                    project_id,
                    name,
                    line("program").unwrap_or_else(|| "other".into()),
                    line("model"),
                    line("description"),
                    ts,
                    ev.event_id,
                    address
                ],
            )?;
        }
        "session.started" | "session.ended" => {
            let status = if ev.kind == "session.started" {
                "active"
            } else {
                "offline"
            };
            tx.execute(
                "UPDATE agents SET status = ?2, last_seen_at = ?3 WHERE agent_id = ?1",
                params![s("agent_id"), status, ts],
            )?;
        }
        "message.sent" => {
            let (Some(message_id), Some(sender)) = (s("message_id"), s("sender_id")) else {
                return Ok(());
            };
            if !agent_exists(tx, &sender)? {
                return Err(Error::NotFound(format!("unknown sender agent {sender}")));
            }
            let reply_to = s("reply_to").filter(|id| {
                tx.query_row("SELECT 1 FROM messages WHERE message_id = ?1", [id], |_| {
                    Ok(())
                })
                .optional()
                .ok()
                .flatten()
                .is_some()
            });
            tx.execute(
                "INSERT OR IGNORE INTO messages (message_id, node_id, thread_id, reply_to, sender_id,
                   subject, body, importance, ack_required, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    message_id,
                    origin,
                    s("thread_id").unwrap_or_else(|| message_id.clone()),
                    reply_to,
                    sender,
                    // A peer's subject skips the sender-side check (`check_text`): store it
                    // as one safe line of at most the local length, so a wake or the CLI
                    // never types or prints its control characters. The signed event keeps
                    // the original text.
                    crate::mail::single_line(
                        &s("subject").unwrap_or_default(),
                        crate::mail::MAX_SUBJECT_CHARS
                    ),
                    s("body").unwrap_or_default(),
                    s("importance").unwrap_or_else(|| "normal".into()),
                    p.get("ack_required").and_then(Value::as_bool).unwrap_or(false),
                    ts,
                    ev.event_id
                ],
            )?;
            for r in p
                .get("recipients")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let agent = r
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !agent_exists(tx, agent)? {
                    continue;
                }
                tx.execute(
                    "INSERT OR IGNORE INTO deliveries (message_id, agent_id, node_id, role)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        message_id,
                        agent,
                        r.get("node_id").and_then(Value::as_str).unwrap_or(origin),
                        r.get("role").and_then(Value::as_str).unwrap_or("to")
                    ],
                )?;
            }
        }
        "delivery.read" | "delivery.acked" => {
            let acked = ev.kind == "delivery.acked";
            tx.execute(
                "UPDATE deliveries SET read_at = coalesce(read_at, ?3),
                   acked_at = CASE WHEN ?4 THEN coalesce(acked_at, ?3) ELSE acked_at END
                 WHERE message_id = ?1 AND agent_id = ?2",
                params![s("message_id"), s("agent_id"), ts, acked],
            )?;
        }
        "hold.acquired" | "hold.updated" => {
            let (Some(hold_id), Some(project), Some(agent)) =
                (s("hold_id"), s("project_id"), s("agent_id"))
            else {
                return Ok(());
            };
            if !project_exists(tx, &project)? || !agent_exists(tx, &agent)? {
                return Ok(());
            }
            let ttl = p
                .get("ttl_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(crate::holds::DEFAULT_HOLD_TTL_SECONDS)
                .clamp(
                    crate::holds::MIN_HOLD_TTL_SECONDS,
                    crate::holds::MAX_HOLD_TTL_SECONDS,
                );
            // No later than the bounded TTL from the (bounded) event time allows.
            let latest = ts + ttl * 1000;
            let expires = p
                .get("expires_at")
                .and_then(Value::as_i64)
                .map_or(latest, |e| e.min(latest));
            let exclusive = p.get("exclusive").and_then(Value::as_bool).unwrap_or(true);
            tx.execute(
                "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive, reason,
                   ttl_seconds, created_at, renewed_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
                 ON CONFLICT(hold_id) DO UPDATE SET exclusive = excluded.exclusive,
                   reason = excluded.reason, ttl_seconds = excluded.ttl_seconds,
                   renewed_at = excluded.renewed_at, expires_at = excluded.expires_at
                 WHERE holds.node_id = excluded.node_id",
                params![hold_id, origin, project, agent, s("pattern"), exclusive, line("reason"), ttl, ts, expires],
            )?;
        }
        "hold.renewed" => {
            for id in strings(p, "hold_ids") {
                tx.execute(
                    "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ttl_seconds * 1000
                     WHERE hold_id = ?1 AND node_id = ?3 AND released_at IS NULL",
                    params![id, ts, origin],
                )?;
            }
        }
        "hold.released" => {
            for id in strings(p, "hold_ids") {
                tx.execute(
                    "UPDATE holds SET released_at = ?2, release_reason = ?3
                     WHERE hold_id = ?1 AND node_id = ?4 AND released_at IS NULL",
                    params![id, ts, s("reason"), origin],
                )?;
            }
        }
        "hold.expired" => {
            tx.execute(
                "UPDATE holds SET released_at = expires_at, release_reason = 'expired'
                 WHERE hold_id = ?1 AND node_id = ?2 AND released_at IS NULL",
                params![s("hold_id"), origin],
            )?;
        }
        "resource.registered" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            let project = s("project_id").filter(|id| project_exists(tx, id).unwrap_or(false));
            tx.execute(
                "INSERT OR IGNORE INTO resources (resource_id, node_id, kind, name, endpoint, port, pid,
                   project_id, purpose, mode, metadata, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    rid,
                    origin,
                    s("kind"),
                    s("name"),
                    line("endpoint"),
                    p.get("port").and_then(Value::as_i64),
                    p.get("pid").and_then(Value::as_i64),
                    project,
                    line("purpose"),
                    s("mode").unwrap_or_else(|| "shared".into()),
                    p.get("metadata").cloned().unwrap_or(json!({})).to_string(),
                    ts,
                    ev.event_id
                ],
            )?;
            foreign_lease(
                tx,
                &rid,
                s("lease_id"),
                s("owner_id"),
                "owner",
                p,
                ts,
                origin,
            )?;
        }
        "resource.adopted" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            foreign_lease(
                tx,
                &rid,
                s("lease_id"),
                s("agent_id"),
                "owner",
                p,
                ts,
                origin,
            )?;
        }
        "resource.borrowed" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            // The owning node decides: no borrowers on exclusive or released resources.
            let ok: Option<(String, Option<i64>)> = tx
                .query_row(
                    "SELECT mode, released_at FROM resources WHERE resource_id = ?1",
                    [&rid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if matches!(ok, Some((ref mode, None)) if mode == "shared") {
                foreign_lease(
                    tx,
                    &rid,
                    s("lease_id"),
                    s("agent_id"),
                    "borrower",
                    p,
                    ts,
                    origin,
                )?;
            }
        }
        "resource.renewed" => {
            for id in strings(p, "lease_ids") {
                tx.execute(
                    "UPDATE resource_leases SET renewed_at = ?2,
                       ttl_seconds = coalesce(?4, ttl_seconds),
                       expires_at = ?2 + coalesce(?4, ttl_seconds) * 1000
                     WHERE lease_id = ?1 AND node_id = ?3 AND released_at IS NULL",
                    params![id, ts, origin, lease_ttl(p)],
                )?;
            }
        }
        // A lease ends by its holder's word, or by the word of the node that owns the
        // resource (a hub ending a revoked spoke's leases on its own resources).
        "resource.released" => {
            for id in strings(p, "lease_ids") {
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = ?3
                     WHERE lease_id = ?1 AND released_at IS NULL
                       AND (node_id = ?4 OR EXISTS (SELECT 1 FROM resources r
                            WHERE r.resource_id = resource_leases.resource_id AND r.node_id = ?4))",
                    params![
                        id,
                        ts,
                        s("reason").unwrap_or_else(|| "released".into()),
                        origin
                    ],
                )?;
            }
        }
        "resource.lease_expired" => {
            tx.execute(
                "UPDATE resource_leases SET released_at = ?2, release_reason = 'expired'
                 WHERE lease_id = ?1 AND released_at IS NULL
                   AND (node_id = ?3 OR EXISTS (SELECT 1 FROM resources r
                        WHERE r.resource_id = resource_leases.resource_id AND r.node_id = ?3))",
                params![s("lease_id"), ts, origin],
            )?;
        }
        "resource.transferred" => {
            tx.execute(
                "UPDATE resource_leases SET role = 'owner' WHERE lease_id = ?1",
                params![s("lease_id")],
            )?;
        }
        "resource.stopped" | "resource.reaped" => {
            // Only the owning node's word ends a resource.
            if let Some(rid) = s("resource_id") {
                tx.execute(
                    "UPDATE resources SET released_at = ?2, release_reason = ?3
                     WHERE resource_id = ?1 AND node_id = ?4 AND released_at IS NULL",
                    params![rid, ts, ev.kind.trim_start_matches("resource."), origin],
                )?;
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = 'stopped'
                     WHERE resource_id = ?1 AND released_at IS NULL
                       AND EXISTS (SELECT 1 FROM resources WHERE resource_id = ?1 AND node_id = ?3)",
                    params![rid, ts, origin],
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// A foreign lease TTL from the payload, clamped to the limits a local borrower gets.
fn lease_ttl(p: &Value) -> Option<i64> {
    p.get("ttl_seconds").and_then(Value::as_i64).map(|t| {
        t.clamp(
            crate::resources::MIN_LEASE_TTL_SECONDS,
            crate::resources::MAX_LEASE_TTL_SECONDS,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn foreign_lease(
    tx: &Tx<'_>,
    resource_id: &str,
    lease_id: Option<String>,
    agent_id: Option<String>,
    role: &str,
    p: &Value,
    ts: i64,
    origin: &str,
) -> Result<()> {
    let (Some(lease_id), Some(agent_id)) = (lease_id, agent_id) else {
        return Ok(());
    };
    if !agent_exists(tx, &agent_id)? {
        return Ok(());
    }
    let exists = tx
        .query_row(
            "SELECT 1 FROM resources WHERE resource_id = ?1",
            [resource_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(());
    }
    let ttl = lease_ttl(p).unwrap_or(crate::resources::DEFAULT_LEASE_TTL_SECONDS);
    tx.execute(
        "INSERT OR IGNORE INTO resource_leases (lease_id, resource_id, node_id, agent_id, role,
           ttl_seconds, created_at, renewed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            lease_id,
            resource_id,
            origin,
            agent_id,
            role,
            ttl,
            ts,
            ts + ttl * 1000
        ],
    )?;
    tx.execute(
        "UPDATE resources SET lapsed_at = NULL WHERE resource_id = ?1",
        [resource_id],
    )?;
    Ok(())
}

fn peer_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Peer> {
    let perms: String = r.get(4)?;
    let key: Option<String> = r.get(5)?;
    Ok(Peer {
        node_id: r.get(0)?,
        display_name: r.get(1)?,
        role: r.get(2)?,
        url: r.get(3)?,
        perms: perms_of(&perms),
        fingerprint: crypto::fingerprint(&key.unwrap_or_default()),
        paired_at: fmt_ts(r.get(6)?),
        last_sync_at: fmt_opt(r.get(7)?),
        revoked_at: fmt_opt(r.get(8)?),
        push_cursor: r.get(9)?,
        pull_cursor: r.get(10)?,
    })
}

const PEER_SELECT: &str = "SELECT p.node_id, n.display_name, p.role, p.url, p.perms, n.public_key,
       p.paired_at, p.last_sync_at, p.revoked_at, p.push_cursor, p.pull_cursor
     FROM peers p JOIN nodes n ON n.node_id = p.node_id";

/// A paired node by id or name, with the lookup `another pair revoke` uses.
fn find_peer(conn: &Connection, node: &str) -> Result<Peer> {
    conn.query_row(
        &format!("{PEER_SELECT} WHERE p.node_id = ?1 OR n.display_name = ?1"),
        [node],
        peer_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("no paired node {node:?}")))
}

impl Store {
    /// Rename this node (do it before pairing: other nodes address agents by node name).
    pub fn set_node_name(&self, name: &str) -> Result<NodeEntry> {
        validate_node_name(name)?;
        if name == "local" {
            return Err(Error::Invalid(
                "`local` means a node that is not named yet; choose another name".into(),
            ));
        }
        self.write(|tx| {
            if node_name_taken(tx, name, tx.node_id)? {
                return Err(Error::Conflict(format!(
                    "another known node is already named {name:?}"
                )));
            }
            tx.execute(
                "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                params![tx.node_id, name],
            )?;
            tx.emit(
                "system",
                "node.renamed",
                json!({ "node_id": tx.node_id, "display_name": name }),
            )?;
            local_entry(tx)
        })
    }

    pub fn node_entry(&self) -> Result<NodeEntry> {
        self.read(local_entry)
    }

    /// Sign a node-to-node HTTP request.
    pub fn sign_request(
        &self,
        method: &str,
        path_and_query: &str,
        time_ms: i64,
        body: &[u8],
    ) -> String {
        crypto::sign(
            self.signing_key(),
            &crypto::request_message(method, path_and_query, time_ms, body),
        )
    }

    /// Hub side: whether `node_id` names a spoke that is paired and not revoked. A hub asks
    /// this of a request's node header before it reads the body, so a caller that names no
    /// paired node makes it buffer nothing; the signature is still checked after the body.
    pub fn is_paired_spoke(&self, node_id: &str) -> Result<bool> {
        self.read(|conn| {
            Ok(matches!(
                peer_row(conn, node_id)?,
                Some((role, _, None)) if role == "spoke"
            ))
        })
    }

    /// Hub side: check a signed request from a paired node. Returns its granted perms.
    pub fn verify_peer_request(
        &self,
        node_id: &str,
        method: &str,
        path_and_query: &str,
        time_ms: i64,
        body: &[u8],
        signature: &str,
    ) -> Result<Vec<Perm>> {
        let denied = || Error::Forbidden("request not signed by a paired node".into());
        // `time_ms` comes from an unauthenticated header: compare without overflow, so an
        // extreme value can neither panic nor wrap past the window.
        if self.now().abs_diff(time_ms) > 5 * 60 * 1000 {
            return Err(Error::Forbidden(
                "request time is more than 5 minutes off; check the clocks".into(),
            ));
        }
        self.read(|conn| {
            let row: Option<(String, Option<i64>, Option<String>)> = conn
                .query_row(
                    "SELECT p.perms, p.revoked_at, n.public_key FROM peers p
                     JOIN nodes n ON n.node_id = p.node_id
                     WHERE p.node_id = ?1 AND p.role = 'spoke' AND n.is_local = 0",
                    [node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((perms, None, Some(key))) = row else {
                return Err(denied());
            };
            let msg = crypto::request_message(method, path_and_query, time_ms, body);
            if crypto::verify_strict(&key, &msg, signature) {
                Ok(perms_of(&perms))
            } else {
                Err(denied())
            }
        })
    }

    /// Hub side: create a single-use invite code for a spoke, signed with this node's key
    /// over all of its fields, the address included.
    pub fn create_invite(
        &self,
        hub_url: &str,
        perms: &[Perm],
        expires_in_seconds: i64,
    ) -> Result<String> {
        self.create_invite_as(hub_url, perms, expires_in_seconds, true)
    }

    /// [`Store::create_invite`] as a build before invites were signed made it: no `sig`.
    #[cfg(feature = "test-support")]
    pub fn create_invite_before_signed_invites(
        &self,
        hub_url: &str,
        perms: &[Perm],
        expires_in_seconds: i64,
    ) -> Result<String> {
        self.create_invite_as(hub_url, perms, expires_in_seconds, false)
    }

    fn create_invite_as(
        &self,
        hub_url: &str,
        perms: &[Perm],
        expires_in_seconds: i64,
        signed: bool,
    ) -> Result<String> {
        // The address is signed into the invite as text and dialled by the joiner: it must
        // be one the joiner accepts, and hold nothing that makes the signed bytes ambiguous.
        let hub_url = hub_url.trim_end_matches('/');
        if !hub_url_is_http(hub_url)
            || hub_url.contains(|c: char| c.is_whitespace() || c.is_control())
        {
            return Err(Error::Invalid(
                "hub url must be an http:// or https:// URL with a host".into(),
            ));
        }
        if expires_in_seconds <= 0 {
            return Err(Error::Invalid("invite expiry must be positive".into()));
        }
        let mut secret = [0u8; 24];
        getrandom::fill(&mut secret)
            .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
        let secret = crypto::b64(&secret);
        let perms: Vec<Perm> = perms
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.write(|tx| {
            let hub = local_entry(tx)?;
            require_named(&hub)?;
            let invite_id = new_id();
            let expires_at = later_secs(tx.now, expires_in_seconds, "invite expiry")?;
            tx.execute(
                "INSERT INTO invites (invite_id, secret_hash, perms, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    invite_id,
                    crypto::sha256_hex(secret.as_bytes()),
                    serde_json::to_string(&perms)?,
                    tx.now,
                    expires_at
                ],
            )?;
            let mut invite = Invite {
                v: 2,
                hub_url: hub_url.to_string(),
                hub,
                invite_id,
                secret,
                perms,
                expires_at,
                sig: String::new(),
            };
            if signed {
                invite.sig = crypto::sign(self.signing_key(), &invite.signed_message());
            }
            Ok(format!(
                "another-invite:{}",
                crypto::b64(serde_json::to_string(&invite)?.as_bytes())
            ))
        })
    }

    /// Spoke side: decode an invite and build the signed pairing request for it. Everything
    /// that can be refused here is, so that nothing is sent and the invite is not used up.
    ///
    /// The hub's signature on the invite is checked first, right after the code decodes,
    /// against the hub key the invite names: an invite without one, or one whose contents
    /// were changed after the hub made it, is refused before any other field is judged.
    pub fn prepare_join(&self, code: &str) -> Result<(Invite, PairRequest)> {
        self.prepare_join_as(code, true)
    }

    /// [`Store::prepare_join`] as a build before invites were signed did it: `sig` is not
    /// a field that build knows, so it is neither checked nor kept.
    #[cfg(feature = "test-support")]
    pub fn prepare_join_before_signed_invites(&self, code: &str) -> Result<(Invite, PairRequest)> {
        self.prepare_join_as(code, false)
    }

    fn prepare_join_as(&self, code: &str, signed: bool) -> Result<(Invite, PairRequest)> {
        let raw = code.trim().strip_prefix("another-invite:").ok_or_else(|| {
            Error::Invalid("not an Another invite code (expected another-invite:...)".into())
        })?;
        // One fixed text for every decode failure: the base64 and JSON errors can quote
        // the code, and the code holds the secret.
        let invalid_code = || Error::Invalid("invalid invite code".into());
        let mut invite: Invite = crypto::unb64(raw)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(invalid_code)?;
        if signed {
            match invite.check_signature() {
                Ok(()) => {}
                Err(InviteSignature::Missing) => {
                    return Err(Error::Invalid(
                        "this invite is not signed by its hub: an older build made it, or its \
                         signature was removed; not joined. Make a new invite on a hub that \
                         runs this build or a newer one"
                            .into(),
                    ))
                }
                Err(InviteSignature::LineBreak) => return Err(invalid_code()),
                Err(InviteSignature::Wrong) => {
                    return Err(Error::Forbidden(
                        "this invite's signature does not match its contents: the code was \
                         changed after the hub made it, or a newer build made it in a form \
                         this build cannot check; not joined. Ask the hub's owner for a new \
                         invite over a channel you trust, and update this node if the hub \
                         runs a newer build"
                            .into(),
                    ))
                }
            }
        } else {
            invite.sig.clear();
        }
        if invite.v != 2 {
            return Err(Error::Invalid(format!(
                "this invite is signed but in a format this build does not know (v {}); not \
                 joined. Update this node to the hub's build",
                invite.v
            )));
        }
        if invite.expires_at <= self.now() {
            return Err(Error::Invalid(
                "this invite has expired; ask for a new one".into(),
            ));
        }
        validate_invite_id(&invite.invite_id)?;
        // Only the shape of the hub entry is checked here. Whether its name is taken on
        // this node is decided when the hub is recorded (`complete_join`).
        validate_node_id(&invite.hub.node_id)?;
        canonical_peer_public(&invite.hub.public_key)?;
        validate_pairing_name(&invite.hub.display_name)?;
        let me = self.node_entry()?;
        require_named(&me)?;
        if me.display_name == invite.hub.display_name {
            return Err(Error::Conflict(format!(
                "the hub is also named {:?}; rename this node first",
                me.display_name
            )));
        }
        self.read(|conn| {
            let known = conn
                .query_row(
                    "SELECT 1 FROM nodes WHERE node_id = ?1",
                    [&invite.hub.node_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !known && node_id_is_a_name(conn, &invite.hub.node_id)? {
                return Err(id_is_a_name(&invite.hub.node_id));
            }
            // The invite and this node's own rows show all of this, so it is refused before
            // anything is sent, and a refused join does not use up an honest hub's invite.
            may_join_as_hub(conn, &me.node_id, &invite.hub)
        })?;
        let time_ms = self.now();
        let sig = crypto::sign(
            self.signing_key(),
            &crypto::pair_message(
                &invite.hub.node_id,
                &invite.hub.public_key,
                &invite.invite_id,
                &crypto::sha256_hex(invite.secret.as_bytes()),
                &me.node_id,
                &me.public_key,
                &me.display_name,
                time_ms,
            ),
        );
        let req = PairRequest {
            invite_id: invite.invite_id.clone(),
            node: me,
            time_ms,
            sig,
        };
        Ok((invite, req))
    }

    /// Hub side: accept a signed pairing request for a valid, unused invite.
    pub fn accept_pairing(&self, req: &PairRequest) -> Result<PairResponse> {
        self.accept_pairing_with_cause(req).0
    }

    /// [`Store::accept_pairing`], and beside the result the cause when the request failed
    /// the invite check. The error text for those four causes is one and the same, so a
    /// caller learns nothing about an invite it does not hold; the cause is for this hub's
    /// audit log only.
    ///
    /// Everything is checked before anything is written, and the writes share one
    /// transaction, so a refused request never uses up the invite.
    pub fn accept_pairing_with_cause(
        &self,
        req: &PairRequest,
    ) -> (Result<PairResponse>, Option<PairRefusal>) {
        let cause = std::cell::RefCell::new(None);
        let result = self.accept_pairing_inner(req, &cause);
        (result, cause.into_inner())
    }

    fn accept_pairing_inner(
        &self,
        req: &PairRequest,
        cause: &std::cell::RefCell<Option<PairRefusal>>,
    ) -> Result<PairResponse> {
        let node = &req.node;
        validate_node_id(&node.node_id)?;
        validate_pairing_name(&node.display_name)?;
        canonical_peer_public(&node.public_key)?;
        if req.sig.is_empty() {
            return Err(Error::Forbidden(
                "this hub accepts only signed pairing requests; upgrade the joining node; \
                 an invite made before the hub was upgraded must be made again"
                    .into(),
            ));
        }
        // `time_ms` comes from the request: compare without overflow, as for a signed
        // request between paired nodes.
        if self.now().abs_diff(req.time_ms) > 5 * 60 * 1000 {
            return Err(Error::Forbidden(
                "pairing request time is more than 5 minutes off; check the clocks".into(),
            ));
        }
        self.write(|tx| {
            let denied = |why: PairRefusal| {
                *cause.borrow_mut() = Some(why);
                Error::Forbidden(
                    "invalid, used or expired invite, or the request is not signed for it".into(),
                )
            };
            type InviteRow = (String, String, i64, Option<i64>, Option<String>);
            let row: Option<InviteRow> = tx
                .query_row(
                    "SELECT secret_hash, perms, expires_at, used_at, used_by FROM invites
                     WHERE invite_id = ?1",
                    [&req.invite_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((secret_hash, perms, expires_at, used_at, used_by)) = row else {
                return Err(denied(PairRefusal::UnknownInvite));
            };
            if used_at.is_some() {
                // An older build stored whatever id the request carried.
                let used_by = used_by
                    .filter(|id| validate_node_id(id).is_ok())
                    .unwrap_or_default();
                return Err(denied(PairRefusal::UsedInvite { used_by }));
            }
            if expires_at <= tx.now {
                return Err(denied(PairRefusal::ExpiredInvite));
            }
            let hub = local_entry(tx)?;
            // Built from this hub's own id and key and the stored hash, never from the
            // request: the signature is the proof of the node key, of the invite and of
            // the hub it was meant for.
            let message = crypto::pair_message(
                &hub.node_id,
                &hub.public_key,
                &req.invite_id,
                &secret_hash,
                &node.node_id,
                &node.public_key,
                &node.display_name,
                req.time_ms,
            );
            if !crypto::verify_strict(&node.public_key, &message, &req.sig) {
                return Err(denied(PairRefusal::BadSignature));
            }

            // The identity rules. The request is from the holder of `node.public_key`.
            let name_taken = || {
                Error::Conflict(format!(
                    "a node named {:?} is already known; rename the joining node",
                    node.display_name
                ))
            };
            if node.node_id == hub.node_id {
                return Err(name_taken());
            }
            let known: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT display_name, public_key FROM nodes WHERE node_id = ?1",
                    [&node.node_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let peer: Option<(String, Option<i64>, String)> = tx
                .query_row(
                    "SELECT role, revoked_at, perms FROM peers WHERE node_id = ?1",
                    [&node.node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let mut previous = None;
            match &known {
                None => {
                    if node_id_is_a_name(tx, &node.node_id)? {
                        return Err(id_is_a_name(&node.node_id));
                    }
                    if node_name_taken(tx, &node.display_name, &node.node_id)? {
                        return Err(name_taken());
                    }
                    tx.execute(
                        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
                         VALUES (?1, ?2, 'peer', 0, ?3, ?4)",
                        params![node.node_id, node.display_name, tx.now, node.public_key],
                    )?;
                }
                Some((_, None)) => {
                    return Err(Error::Conflict(format!(
                        "node {} is known here without a key; it cannot be paired",
                        node.node_id
                    )));
                }
                Some((_, Some(key))) if *key != node.public_key => {
                    return Err(Error::Conflict(format!(
                        "node {} presented a different key than before; refusing",
                        node.node_id
                    )));
                }
                Some((name, Some(_))) => {
                    match &peer {
                        Some((role, _, _)) if role == "hub" => {
                            return Err(Error::Conflict(format!(
                                "this node syncs with {name} as its hub; it cannot also be its hub"
                            )));
                        }
                        Some((_, Some(_), _)) => {
                            return Err(Error::Forbidden(format!(
                                "node {name} was revoked on this hub; the hub's owner can let \
                                 it back in with `another pair readmit {name}`"
                            )));
                        }
                        Some((_, None, old_perms)) => {
                            previous = Some((name.clone(), perms_of(old_perms)));
                        }
                        None => {}
                    }
                    // The rename rule, only when the name changes: a node keeps a name
                    // that predates a rule.
                    if *name != node.display_name {
                        if node_name_taken(tx, &node.display_name, &node.node_id)? {
                            return Err(name_taken());
                        }
                        tx.execute(
                            "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                            params![node.node_id, node.display_name],
                        )?;
                    }
                }
            }
            // Pairing never writes `revoked_at`: a revoked row was refused above.
            tx.execute(
                "INSERT INTO peers (node_id, role, perms, paired_at) VALUES (?1, 'spoke', ?2, ?3)
                 ON CONFLICT(node_id) DO UPDATE SET perms = excluded.perms,
                   paired_at = excluded.paired_at",
                params![node.node_id, perms, tx.now],
            )?;
            tx.execute(
                "UPDATE invites SET used_at = ?2, used_by = ?3 WHERE invite_id = ?1",
                params![req.invite_id, tx.now, node.node_id],
            )?;
            let perms = perms_of(&perms);
            let mut record = json!({
                "node_id": node.node_id,
                "display_name": node.display_name,
                "role": "spoke",
                "perms": perms,
                "again": peer.is_some(),
                "invite_id": req.invite_id,
                "fingerprint": crypto::fingerprint(&node.public_key),
            });
            if let Some((previous_name, previous_perms)) = previous {
                record["previous_name"] = json!(previous_name);
                record["previous_perms"] = json!(previous_perms);
            }
            tx.emit("human (another pair)", "peer.paired", record)?;
            let sig = crypto::sign(
                tx.key,
                &crypto::pair_accept_message(
                    &req.invite_id,
                    &secret_hash,
                    &node.node_id,
                    &node.public_key,
                    &hub.display_name,
                    &perms_text(&perms),
                ),
            );
            Ok(PairResponse { hub, perms, sig })
        })
    }

    /// Spoke side: record the hub after it accepted the pairing. The answer must be signed
    /// by the hub key the invite carries, for this node, with the name and permissions it
    /// states; otherwise nothing is recorded.
    pub fn complete_join(&self, invite: &Invite, resp: &PairResponse) -> Result<Peer> {
        self.complete_join_reporting(invite, resp)
            .map(|(peer, _)| peer)
    }

    /// [`Store::complete_join`], and beside the peer the name this node had stored for the
    /// hub when the join lifted this node's own revoke of it (`None` for any other join).
    /// A join to a hub this node revoked makes it sync with that hub again, whatever name
    /// the invite gives it, so the caller says so.
    pub fn complete_join_reporting(
        &self,
        invite: &Invite,
        resp: &PairResponse,
    ) -> Result<(Peer, Option<String>)> {
        if resp.hub.node_id != invite.hub.node_id || resp.hub.public_key != invite.hub.public_key {
            return Err(Error::Forbidden(
                "the hub answered with a different identity than the invite".into(),
            ));
        }
        validate_invite_id(&invite.invite_id)?;
        // The fields of the signed bytes are joined by line breaks, so none may hold one
        // when the signature is checked. The invite id was checked above; the other fields
        // are this node's own id and key, a hash and the permission names; the hub's name
        // is the one that arrives with the answer. Whether it is a usable name is decided
        // after the check, by the rule for a hub's list.
        if resp.hub.display_name.contains(['\n', '\r']) {
            return Err(Error::Invalid(
                "the hub's answer carries a name with a line break; not paired".into(),
            ));
        }
        let me = self.node_entry()?;
        let message = crypto::pair_accept_message(
            &invite.invite_id,
            &crypto::sha256_hex(invite.secret.as_bytes()),
            &me.node_id,
            &me.public_key,
            &resp.hub.display_name,
            &perms_text(&resp.perms),
        );
        if !crypto::verify_strict(&invite.hub.public_key, &message, &resp.sig) {
            return Err(Error::Forbidden(
                "the hub's answer is not signed with the hub key in the invite; not paired".into(),
            ));
        }
        let invite_signed = invite.check_signature().is_ok();
        self.write(|tx| {
            // A join writes the `hub` row of another node and nothing else: never a row for
            // this node itself, never the row of a node paired here as a spoke, never under
            // a key other than the one stored for the id. The same rule ran before the
            // request was sent; the rows may have changed since.
            may_join_as_hub(tx, tx.node_id, &resp.hub)?;
            // The hub's name is applied by the rule for a name in a hub's list: used when
            // valid, not `local` and not another node's, otherwise the known name or the id.
            let Some(hub) = listed_entry(tx, &resp.hub)? else {
                return Err(id_is_a_name(&resp.hub.node_id));
            };
            let before = peer_row(tx, &hub.node_id)?;
            let again = before.is_some();
            // This node's own revoke of the hub is lifted by this join: remember the name
            // it was stored under, which the invite may not repeat.
            let lifted: Option<String> = match before {
                Some((_, _, Some(_))) => tx
                    .query_row(
                        "SELECT display_name FROM nodes WHERE node_id = ?1",
                        [&hub.node_id],
                        |r| r.get(0),
                    )
                    .optional()?,
                _ => None,
            };
            upsert_node(tx, &hub)?;
            tx.execute(
                "INSERT INTO peers (node_id, role, url, perms, paired_at) VALUES (?1, 'hub', ?2, ?3, ?4)
                 ON CONFLICT(node_id) DO UPDATE SET url = excluded.url, perms = excluded.perms,
                   revoked_at = NULL
                 WHERE peers.role = 'hub'",
                params![hub.node_id, invite.hub_url, serde_json::to_string(&resp.perms)?, tx.now],
            )?;
            let mut record = json!({
                "node_id": hub.node_id,
                "display_name": hub.display_name,
                "role": "hub",
                "perms": resp.perms,
                "again": again,
                "invite_id": invite.invite_id,
                "fingerprint": crypto::fingerprint(&hub.public_key),
            });
            // The stored address is one the hub's key signed. A join without this mark is
            // one an older build made, on an invite whose address nothing checked.
            if invite_signed {
                record["invite_signed"] = json!(true);
            }
            if let Some(previous_name) = &lifted {
                record["revoke_lifted"] = json!(true);
                record["previous_name"] = json!(previous_name);
            }
            tx.emit("human (another pair)", "peer.paired", record)?;
            let peer = tx.query_row(
                &format!("{PEER_SELECT} WHERE p.node_id = ?1"),
                [&hub.node_id],
                peer_from_row,
            )?;
            Ok((peer, lifted))
        })
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!("{PEER_SELECT} ORDER BY p.paired_at"))?;
            let rows = stmt.query_map([], peer_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The hub this node syncs with: (node_id, url).
    pub fn hub(&self) -> Result<Option<(String, String)>> {
        self.read(|conn| Ok(hub_of(conn)?.map(|(n, u, _)| (n, u))))
    }

    /// Stop accepting requests from (or syncing with) a peer.
    pub fn revoke_peer(&self, node: &str) -> Result<Peer> {
        self.write(|tx| {
            let id: String = tx
                .query_row(
                    "SELECT p.node_id FROM peers p JOIN nodes n ON n.node_id = p.node_id
                     WHERE p.node_id = ?1 OR n.display_name = ?1",
                    [node],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no paired node {node:?}")))?;
            not_this_node(tx, &id)?;
            tx.execute(
                "UPDATE peers SET revoked_at = ?2 WHERE node_id = ?1",
                params![id, tx.now],
            )?;
            tx.emit(
                "human (another pair revoke)",
                "peer.revoked",
                json!({ "node_id": id }),
            )?;
            // The revoked node can no longer end its own leases here, so this node ends
            // the ones on its own resources rather than let one be promoted to owner.
            crate::resources::release_node_leases_tx(tx, &id, "peer_revoked")?;
            Ok(tx.query_row(
                &format!("{PEER_SELECT} WHERE p.node_id = ?1"),
                [&id],
                peer_from_row,
            )?)
        })
    }

    /// Hub side: let a spoke this hub revoked sync again, with the permissions it had.
    /// This is the only way a revoked node returns; an invite never does it. Leases the
    /// revoke ended are not restored.
    pub fn readmit_peer(&self, node: &str) -> Result<Peer> {
        self.write(|tx| {
            let peer = find_peer(tx, node)?;
            not_this_node(tx, &peer.node_id)?;
            if peer.role == "hub" {
                return Err(Error::Invalid(format!(
                    "{} is this node's hub; join it again with a fresh invite",
                    peer.display_name
                )));
            }
            if peer.revoked_at.is_none() {
                return Err(Error::Conflict(format!(
                    "node {} is not revoked",
                    peer.display_name
                )));
            }
            tx.execute(
                "UPDATE peers SET revoked_at = NULL WHERE node_id = ?1",
                [&peer.node_id],
            )?;
            tx.emit(
                "human (another pair readmit)",
                "peer.readmitted",
                json!({
                    "node_id": peer.node_id,
                    "display_name": peer.display_name,
                    "perms": peer.perms,
                    "fingerprint": peer.fingerprint,
                }),
            )?;
            find_peer(tx, &peer.node_id)
        })
    }

    /// Free the name a revoked node holds: the node is listed by its id from now on. Its
    /// row, key, revoked state and agents stay. Returns the peer and the name it had. The
    /// name is free for any node at once.
    pub fn release_peer_name(&self, node: &str) -> Result<(Peer, String)> {
        self.write(|tx| {
            let peer = find_peer(tx, node)?;
            not_this_node(tx, &peer.node_id)?;
            if peer.revoked_at.is_none() {
                return Err(Error::Conflict(format!(
                    "node {} is not revoked; revoke it first",
                    peer.display_name
                )));
            }
            if peer.display_name == peer.node_id {
                return Err(Error::Conflict(format!(
                    "node {} has no name to release",
                    peer.node_id
                )));
            }
            tx.execute(
                "UPDATE nodes SET display_name = node_id WHERE node_id = ?1",
                [&peer.node_id],
            )?;
            tx.emit(
                "human (another pair release-name)",
                "peer.name_released",
                json!({
                    "node_id": peer.node_id,
                    "previous_name": peer.display_name,
                    "fingerprint": peer.fingerprint,
                }),
            )?;
            Ok((find_peer(tx, &peer.node_id)?, peer.display_name))
        })
    }

    /// Spoke side: events this node authored that may leave it, after `after_seq`.
    /// Returns them and the highest sequence scanned (to advance the cursor past skips).
    pub fn outbound_events(&self, after_seq: i64, limit: u32) -> Result<(Vec<WireEvent>, i64)> {
        self.outbound_events_within(after_seq, limit, PUSH_BATCH_BUDGET_BYTES)
    }

    /// [`Store::outbound_events`] with the byte budget given: the batch is closed before
    /// the push request body carrying it would pass `budget` bytes, and always holds one
    /// event when there is one to send, so an event larger than the budget travels alone.
    /// `usize::MAX` bounds it by count only, as builds before D99 did. Fails when the next
    /// event to send would alone make a push over [`PUSH_BODY_LIMIT_BYTES`].
    pub fn outbound_events_within(
        &self,
        after_seq: i64,
        limit: u32,
        budget: usize,
    ) -> Result<(Vec<WireEvent>, i64)> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{WIRE_SELECT} WHERE node_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
            ))?;
            let mut rows = stmt.query(params![self.node_id(), after_seq, limit])?;
            let mut high = after_seq;
            let mut out: Vec<WireEvent> = Vec::new();
            // The body is the envelope, the events and a comma between each two.
            let mut bytes = PUSH_ENVELOPE_BYTES - 1;
            while let Some(row) = rows.next()? {
                let e = wire_from_row(row)?;
                let seq = e.seq;
                let p: Value = serde_json::from_str(&e.payload).unwrap_or(Value::Null);
                if classify(&e.kind, &p, &e.node_id) != Share::Never {
                    let len = e.wire_bytes() + 1;
                    // Alone it would still be over what a hub reads.
                    if PUSH_ENVELOPE_BYTES - 1 + len > PUSH_BODY_LIMIT_BYTES {
                        if high == after_seq {
                            return Err(over_limit_in_log(
                                &e,
                                "this node's",
                                "a hub reads in one push",
                                PUSH_BODY_LIMIT_BYTES,
                            ));
                        }
                        break;
                    }
                    if !out.is_empty() && bytes.saturating_add(len) > budget {
                        break;
                    }
                    bytes += len;
                    out.push(e);
                }
                high = seq;
            }
            Ok((out, high))
        })
    }

    /// Spoke side: after a push batch to `peer`, commit the push cursor and, in the same
    /// transaction, the pushed anchor: `last_accepted`, the batch's last event the hub
    /// accepted or already held. A node-local `peer.pushed` records it when it changes.
    pub fn advance_push(&self, peer: &str, seq: i64, last_accepted: Option<&str>) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET push_cursor = max(push_cursor, ?2) WHERE node_id = ?1",
                params![peer, seq],
            )?;
            if let Some(id) = last_accepted {
                if pushed_anchor(tx, tx.node_id)?.as_deref() != Some(id) {
                    tx.emit(
                        "system",
                        "peer.pushed",
                        json!({ "peer": peer, "anchor": id }),
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Spoke side: the newest of this node's own events its hub accepted, the pushed anchor
    /// for [`Store::pull_for_from`].
    pub fn last_pushed_event_id(&self) -> Result<Option<String>> {
        self.read(|conn| pushed_anchor(conn, self.node_id()))
    }

    pub fn push_cursor(&self, peer: &str) -> Result<(i64, i64)> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT push_cursor, pull_cursor FROM peers WHERE node_id = ?1",
                [peer],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    /// Hub side: what a spoke receives after `after_seq` (its own events excluded).
    pub fn pull_for(&self, peer: &str, after_seq: i64, limit: u32) -> Result<PullResponse> {
        self.pull_for_from(peer, after_seq, &PullAnchors::default(), limit)
    }

    /// [`Store::pull_for`] with the spoke's [`PullAnchors`]. The response is `reset`, with no
    /// events, when this hub's log no longer reaches `after_seq`, no longer holds the received
    /// anchor at the `seq` it was sent at, or no longer holds the spoke's last pushed event.
    /// Each means the hub was restored from an older copy or replaced, so its sequence
    /// numbers no longer match the spoke's cursors. The received anchor is checked only when
    /// `after_seq > 0`.
    pub fn pull_for_from(
        &self,
        peer: &str,
        after_seq: i64,
        anchors: &PullAnchors,
        limit: u32,
    ) -> Result<PullResponse> {
        self.pull_page(peer, after_seq, anchors, limit, PULL_PAGE_BUDGET_BYTES)
    }

    /// [`Store::pull_for_from`] with the byte budget given: the page is closed, with `more`
    /// set, before its response body (the node directory included) would pass `budget`
    /// bytes, and always holds one event when there is one to send, so an event larger than
    /// the budget travels alone. `usize::MAX` bounds it by count only, as builds before D99
    /// did. Fails when the next event to send would alone make an answer over
    /// [`PULL_BODY_LIMIT_BYTES`].
    pub fn pull_page(
        &self,
        peer: &str,
        after_seq: i64,
        anchors: &PullAnchors,
        limit: u32,
        budget: usize,
    ) -> Result<PullResponse> {
        self.read(|conn| {
            let Some((role, perms, None)) = peer_row(conn, peer)? else {
                return Err(Error::Forbidden("not a paired node".into()));
            };
            if role != "spoke" {
                return Err(Error::Forbidden("only spokes pull from a hub".into()));
            }
            let high: i64 =
                conn.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?;
            let seq_of = |id: &str| -> Result<Option<i64>> {
                Ok(conn
                    .query_row("SELECT seq FROM events WHERE event_id = ?1", [id], |r| {
                        r.get(0)
                    })
                    .optional()?)
            };
            let received_lost = match anchors.received.as_deref().filter(|_| after_seq > 0) {
                Some(id) => match (seq_of(id)?, anchors.received_seq) {
                    (None, _) => true,
                    (Some(at), Some(sent_at)) => at != sent_at,
                    (Some(_), None) => false,
                },
                None => false,
            };
            let pushed_lost = match anchors.pushed.as_deref() {
                Some(id) => seq_of(id)?.is_none(),
                None => false,
            };
            if after_seq > high || received_lost || pushed_lost {
                return Ok(PullResponse {
                    events: Vec::new(),
                    cursor: 0,
                    more: false,
                    nodes: known_nodes(conn)?,
                    reset: true,
                });
            }
            let scan = limit.saturating_mul(4).max(limit);
            let mut stmt = conn.prepare(&format!(
                "{WIRE_SELECT} WHERE seq > ?1 AND node_id != ?2 ORDER BY seq LIMIT ?3"
            ))?;
            let mut rows = stmt.query(params![after_seq, peer, scan])?;
            let mut page = PullResponse {
                events: Vec::new(),
                cursor: i64::MAX,
                more: false,
                nodes: known_nodes(conn)?,
                reset: false,
            };
            // The body with no events and the widest cursor, then the events and a comma
            // between each two.
            let envelope = json_bytes(&page) - 1;
            let mut bytes = envelope;
            let mut scanned = 0u32;
            // The page was closed with an event still to send.
            let mut closed = false;
            let mut cursor = after_seq;
            while let Some(row) = rows.next()? {
                let e = wire_from_row(row)?;
                scanned += 1;
                let p: Value = serde_json::from_str(&e.payload).unwrap_or(Value::Null);
                let send = match classify(&e.kind, &p, &e.node_id) {
                    Share::Never => false,
                    Share::Directory => true,
                    Share::Feed(also) => {
                        perms.contains(&Perm::ReadFeed) || also.iter().any(|n| n == peer)
                    }
                    Share::Mail(nodes) => nodes.iter().any(|n| n == peer),
                };
                if !send {
                    cursor = e.seq;
                    continue;
                }
                let len = e.wire_bytes() + 1;
                // Alone it would still be over what a spoke reads.
                if envelope + len > PULL_BODY_LIMIT_BYTES {
                    if cursor == after_seq {
                        return Err(over_limit_in_log(
                            &e,
                            "the hub's",
                            "a spoke reads in one pull answer",
                            PULL_BODY_LIMIT_BYTES,
                        ));
                    }
                    closed = true;
                    break;
                }
                if !page.events.is_empty() && bytes.saturating_add(len) > budget {
                    closed = true;
                    break;
                }
                bytes += len;
                cursor = e.seq;
                page.events.push(e);
                if page.events.len() as u32 >= limit {
                    break;
                }
            }
            page.more = closed || scanned == scan || page.events.len() as u32 >= limit;
            page.cursor = cursor;
            Ok(page)
        })
    }

    /// Spoke side: the last event this node received from its hub, the anchor for
    /// [`Store::pull_for_from`]. Events from other nodes reach a spoke only through its hub
    /// and are stored in the order the hub sent them. When the last event the hub sent was
    /// not stored (a duplicate or a rejected event), or after a resync, the newest record
    /// is a local `peer.anchored` or `peer.resynced` event naming it (`anchor`).
    pub fn last_relayed_event_id(&self) -> Result<Option<String>> {
        Ok(self
            .read(|conn| relayed_anchor(conn, self.node_id()))?
            .map(|(id, _)| id))
    }

    /// Spoke side: every anchor for the next pull from the hub (see [`PullAnchors`]).
    pub fn pull_anchors(&self) -> Result<PullAnchors> {
        self.read(|conn| {
            let local = self.node_id();
            let (received, received_seq) = relayed_anchor(conn, local)?.unzip();
            Ok(PullAnchors {
                received,
                received_seq: received_seq.flatten(),
                pushed: pushed_anchor(conn, local)?,
            })
        })
    }

    /// Spoke side: after a pull page from `peer` is ingested, commit its cursor and the
    /// anchor that goes with it **in one transaction**, so a failure or crash can never
    /// leave an advanced cursor beside an older anchor (REVIEW-R2). `last_received` is the
    /// last event the page carried, with the hub `seq` it was sent at. A node-local
    /// `peer.anchored` records both whenever they change. A page with no events moves the
    /// cursor and keeps the anchor.
    pub fn advance_pull(
        &self,
        peer: &str,
        seq: i64,
        last_received: Option<(&str, i64)>,
    ) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET pull_cursor = max(pull_cursor, ?2) WHERE node_id = ?1",
                params![peer, seq],
            )?;
            if let Some((id, at)) = last_received {
                if relayed_anchor(tx, tx.node_id)? != Some((id.to_string(), Some(at))) {
                    tx.emit(
                        "system",
                        "peer.anchored",
                        json!({ "peer": peer, "anchor": id, "seq": at }),
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Spoke side: a sync with `hub` ran to its end. This is the only write of the hub's
    /// `last_sync_at`: a push batch or a pull page by itself does not count, so a sync that
    /// keeps failing part way shows as stale.
    pub fn record_sync_complete(&self, hub: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET last_sync_at = ?2 WHERE node_id = ?1 AND role = 'hub'",
                params![hub, tx.now],
            )?;
            Ok(())
        })
    }

    /// Hub side: a push or a pull of `spoke` was answered with a signed `200`. Writes that
    /// spoke's `last_sync_at`, so the hub can tell a spoke that stopped syncing.
    pub fn record_peer_contact(&self, spoke: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET last_sync_at = ?2 WHERE node_id = ?1 AND role = 'spoke'",
                params![spoke, tx.now],
            )?;
            Ok(())
        })
    }

    /// Hub side: sign a `200` answer to a push or a pull of `spoke_node_id`. `nonce` and
    /// `request_signature` are the header texts of the request being answered, and `body`
    /// the exact bytes that are sent.
    pub fn sign_answer(
        &self,
        spoke_node_id: &str,
        nonce: &str,
        request_signature: &str,
        body: &[u8],
    ) -> String {
        crypto::sign(
            self.signing_key(),
            &crypto::answer_message(
                self.node_id(),
                spoke_node_id,
                nonce,
                request_signature,
                body,
            ),
        )
    }

    /// Spoke side: check the hub's signature on an answer to a push or a pull, under the
    /// key pinned for `hub_id` at pairing. `nonce` and `request_signature` are what this
    /// node sent with the request, and `body` the answer body as received, after content
    /// decoding. Nothing of the body may be read before this returns `Ok`.
    pub fn verify_hub_answer(
        &self,
        hub_id: &str,
        nonce: &str,
        request_signature: &str,
        body: &[u8],
        signature: &str,
    ) -> Result<()> {
        let key: Option<String> = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT public_key FROM nodes WHERE node_id = ?1 AND is_local = 0",
                    [hub_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        })?;
        let message =
            crypto::answer_message(hub_id, self.node_id(), nonce, request_signature, body);
        match key {
            Some(key) if crypto::verify_strict(&key, &message, signature) => Ok(()),
            _ => Err(Error::Forbidden(
                "answer signature is not the hub's for this request".into(),
            )),
        }
    }

    /// Spoke side: the first entry of a hub's list that names a node with another key than
    /// the one pinned here, as [`Store::ingest`] refuses it. Read after such a refusal, to
    /// record it: the refusal itself rolled back with its transaction.
    pub fn listed_key_conflict(&self, nodes: &[NodeEntry]) -> Result<Option<KeyConflict>> {
        self.read(|conn| {
            for n in nodes {
                if crypto::decode_peer_public(&n.public_key).is_err()
                    || validate_node_id(&n.node_id).is_err()
                {
                    continue;
                }
                if let Some(conflict) = key_conflict(conn, self.node_id(), n)? {
                    return Ok(Some(conflict));
                }
            }
            Ok(None)
        })
    }

    /// Spoke side: this node's hub, when the newest local `peer.paired` record of it does
    /// not say the invite was signed. Its stored address is then one nothing checked: the
    /// join was made by a build before invites were signed. `None` for a hub joined on a
    /// signed invite, and for a node with no hub.
    pub fn hub_joined_before_signed_invites(&self) -> Result<Option<String>> {
        self.read(|conn| {
            let Some((hub, _, _)) = hub_of(conn)? else {
                return Ok(None);
            };
            let newest: Option<rusqlite::types::Value> = conn
                .query_row(
                    "SELECT json_extract(payload, '$.invite_signed') FROM events
                     WHERE node_id = ?1 AND kind = 'peer.paired'
                       AND json_extract(payload, '$.node_id') = ?2
                       AND json_extract(payload, '$.role') = 'hub'
                     ORDER BY seq DESC LIMIT 1",
                    params![self.node_id(), hub],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(match newest {
                Some(rusqlite::types::Value::Integer(1)) | None => None,
                Some(_) => Some(hub),
            })
        })
    }

    /// Spoke side: start over with `peer` from zero in both directions, after the hub
    /// reported `reset`. Duplicates are skipped on both sides, so this is safe to repeat.
    pub fn reset_sync_cursors(&self, peer: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET push_cursor = 0, pull_cursor = 0 WHERE node_id = ?1",
                [peer],
            )?;
            Ok(())
        })
    }

    /// Spoke side: record a completed resync with `peer`: the last event it sent and the
    /// `seq` it was sent at (the received anchor), and the last of this node's events it
    /// accepted (the pushed anchor), each `null` when there was none. A `peer.*` event never
    /// leaves this node.
    pub fn record_resync(
        &self,
        peer: &str,
        anchor: Option<(&str, i64)>,
        pushed: Option<&str>,
    ) -> Result<()> {
        let (id, seq) = anchor.unzip();
        self.write(|tx| {
            tx.emit(
                "system",
                "peer.resynced",
                json!({ "peer": peer, "anchor": id, "seq": seq, "pushed": pushed }),
            )?;
            Ok(())
        })
    }

    /// Store and project events received from `from` (a spoke pushing, or the hub relaying).
    /// Each event is verified against its origin node's key; duplicates are skipped.
    pub fn ingest(
        &self,
        from: &str,
        events: &[WireEvent],
        nodes: &[NodeEntry],
    ) -> Result<IngestReport> {
        self.write(|tx| {
            let Some((role, perms, None)) = peer_row(tx, from)? else {
                return Err(Error::Forbidden("not a paired node".into()));
            };
            for n in nodes {
                // A key in the directory that is weak (a hub may have pinned one before keys
                // were checked) or does not decode is never pinned here, but it does not
                // stop the rest of the page from syncing: events from that node then fail
                // as from an unknown origin (FINDINGS-197 adv197).
                if crypto::decode_peer_public(&n.public_key).is_err() {
                    continue;
                }
                // A spoke speaks only for itself (public keys prove nothing about the
                // sender); for its spokes, the hub's list is the directory.
                if role == "spoke" && n.node_id != from {
                    continue;
                }
                if let Some(entry) = listed_entry(tx, n)? {
                    // A hub's list reaches here only from an answer the hub signed. A key
                    // in it that differs from the pinned one is refused with both
                    // fingerprints: one of the two is wrong, and the owner must see which.
                    if role == "hub" {
                        if let Some(conflict) = key_conflict(tx, tx.node_id, &entry)? {
                            return Err(conflict.refusal());
                        }
                    }
                    upsert_node(tx, &entry)?;
                }
            }
            let mut report = IngestReport::default();
            for ev in events {
                let reject = |reason: String| Rejected {
                    event_id: ev.event_id.clone(),
                    reason,
                };
                if ev.node_id == tx.node_id {
                    report.rejected.push(reject("events about this node come only from this node".into()));
                    continue;
                }
                let payload: Value = match serde_json::from_str(&ev.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        report.rejected.push(reject(format!("invalid payload: {e}")));
                        continue;
                    }
                };
                if role == "spoke" {
                    if let Err(e) = spoke_may_publish(tx, &perms, from, ev, &payload) {
                        report.rejected.push(reject(e.to_string()));
                        continue;
                    }
                }
                if let Err(e) = verify_wire(tx, ev) {
                    report.rejected.push(reject(e.to_string()));
                    continue;
                }
                if let Err(e) = foreign_authority(tx, &ev.node_id, &ev.kind, &payload) {
                    report.rejected.push(reject(e.to_string()));
                    continue;
                }
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![ev.event_id, ev.node_id, ev.ts, ev.actor, ev.kind, ev.payload, ev.prev_id, ev.sig],
                )?;
                if inserted == 0 {
                    report.duplicates += 1;
                    continue;
                }
                if let Err(e) = project_foreign(tx, ev, &payload) {
                    tracing::warn!(event = %ev.event_id, kind = %ev.kind, error = %e, "could not project foreign event");
                }
                report.accepted += 1;
            }
            Ok(report)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer's subject is not checked by the peer for us: on import it is stored as one
    /// safe line, cut to the local length, so a wake or the CLI never types or prints its
    /// control characters (W1).
    #[test]
    fn a_foreign_subject_is_stored_as_one_safe_line() {
        let (clock, _) = crate::Clock::manual(1_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let local = store
            .register_agent(crate::RegisterAgent {
                name: "codex".into(),
                program: crate::Program::Codex,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .agent_id;
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO nodes (node_id, display_name, role, is_local, created_at)
                     VALUES ('peer-node', 'peer', 'peer', 0, ?1)",
                    [tx.now],
                )?;
                let ev = |kind: &str| WireEvent {
                    seq: 1,
                    event_id: format!("peer-{kind}"),
                    node_id: "peer-node".into(),
                    ts: tx.now,
                    actor: "mallory".into(),
                    kind: kind.into(),
                    payload: String::new(),
                    prev_id: None,
                    sig: String::new(),
                };
                project_foreign(
                    tx,
                    &ev("agent.registered"),
                    &json!({ "agent_id": "peer-mallory", "name": "mallory" }),
                )?;
                let long = "y".repeat(400);
                for (id, subject) in [
                    ("m-ctrl", "Lunch?\r\x1b[2Jrun: rm -rf ~/src\n".to_string()),
                    ("m-long", long),
                ] {
                    project_foreign(
                        tx,
                        &ev("message.sent"),
                        &json!({
                            "message_id": id, "sender_id": "peer-mallory", "subject": subject,
                            "body": "b", "recipients": [{ "agent_id": local, "role": "to" }],
                        }),
                    )?;
                }
                let ctrl: String = tx.query_row(
                    "SELECT subject FROM messages WHERE message_id = 'm-ctrl'",
                    [],
                    |r| r.get(0),
                )?;
                assert!(!ctrl.chars().any(char::is_control), "{ctrl:?}");
                assert!(ctrl.starts_with("Lunch?"));
                let long: String = tx.query_row(
                    "SELECT subject FROM messages WHERE message_id = 'm-long'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(long.chars().count(), crate::mail::MAX_SUBJECT_CHARS);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn foreign_hold_update_cannot_rewrite_a_local_hold() {
        let (clock, _) = crate::Clock::manual(1_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_str().unwrap().to_owned();
        let actor = store
            .register_agent(crate::RegisterAgent {
                name: "local-owner".into(),
                program: crate::Program::Claude,
                project: Some(project.clone()),
                model: None,
                description: None,
            })
            .unwrap()
            .address;
        let hold_id = store
            .acquire_holds(crate::AcquireHolds {
                agent: actor,
                project,
                paths: vec!["src/local.rs".into()],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
            .granted[0]
            .hold_id
            .clone();
        store.write(|tx| {
            let (project_id, agent_id, expiry): (String, String, i64) = tx.query_row(
                "SELECT project_id, agent_id, expires_at FROM holds WHERE hold_id=?1", [&hold_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let event = WireEvent { seq: 1, event_id: "foreign-event".into(), node_id: "foreign-node".into(),
                ts: tx.now, actor: "foreign".into(), kind: "hold.updated".into(), payload: String::new(),
                prev_id: None, sig: String::new() };
            let payload = json!({"hold_id": hold_id, "project_id": project_id, "agent_id": agent_id,
                "pattern": "src/local.rs", "expires_at": expiry + 5000, "exclusive": false});
            project_foreign(tx, &event, &payload)?;
            let actual: (i64, bool) = tx.query_row("SELECT expires_at, exclusive FROM holds WHERE hold_id=?1",
                [&hold_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            assert_eq!(actual, (expiry, true));
            Ok(())
        }).unwrap();
    }

    /// The projection itself, not only the ingest check, keeps a foreign stop away from this
    /// node's leases.
    #[test]
    fn foreign_stop_cannot_end_local_leases() {
        let (clock, _) = crate::Clock::manual(1_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        store
            .register_agent(crate::RegisterAgent {
                name: "owner".into(),
                program: crate::Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
        let rid = store
            .register_resource(crate::RegisterResource {
                agent: "owner".into(),
                kind: crate::ResourceKind::Port,
                name: "tcp:127.0.0.1:1".into(),
                endpoint: None,
                port: Some(1),
                pid: None,
                project: None,
                purpose: None,
                mode: crate::LeaseMode::Shared,
                ttl_seconds: None,
                metadata: None,
            })
            .unwrap()
            .resource
            .resource_id;
        store
            .write(|tx| {
                let event = WireEvent {
                    seq: 1,
                    event_id: "foreign-stop".into(),
                    node_id: "foreign-node".into(),
                    ts: tx.now,
                    actor: "foreign".into(),
                    kind: "resource.stopped".into(),
                    payload: String::new(),
                    prev_id: None,
                    sig: String::new(),
                };
                project_foreign(tx, &event, &json!({ "resource_id": rid }))?;
                let live: i64 = tx.query_row(
                    "SELECT count(*) FROM resource_leases WHERE resource_id = ?1 AND released_at IS NULL",
                    [&rid],
                    |r| r.get(0),
                )?;
                assert_eq!(live, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn signed_peer_collision_cannot_rewrite_local_hold() {
        let (clock, store_now) = crate::Clock::manual(1_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let (peer_clock, peer_now) = crate::Clock::manual(1_000_000);
        let peer = Store::open_in_memory(peer_clock).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_str().unwrap().to_owned();
        let actor = store
            .register_agent(crate::RegisterAgent {
                name: "local-owner".into(),
                program: crate::Program::Claude,
                project: Some(project.clone()),
                model: None,
                description: None,
            })
            .unwrap()
            .address;
        let id = store
            .acquire_holds(crate::AcquireHolds {
                agent: actor,
                project,
                paths: vec!["src/local.rs".into()],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: Some("local reason".into()),
                repo_wide: false,
            })
            .unwrap()
            .granted[0]
            .hold_id
            .clone();
        let (project_id, agent_id, expiry): (String, String, i64) = store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT project_id, agent_id, expires_at FROM holds WHERE hold_id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap();
        let peer_id = peer.node_id().to_owned();
        store
            .write(|tx| {
                upsert_node(
                    tx,
                    &NodeEntry {
                        node_id: peer_id.clone(),
                        display_name: "peer".into(),
                        public_key: crypto::public_of(peer.signing_key()),
                    },
                )?;
                tx.execute(
                    "INSERT INTO peers(node_id,role,perms,paired_at) VALUES (?1,'spoke','[]',?2)",
                    params![peer_id, tx.now],
                )?;
                Ok(())
            })
            .unwrap();
        let payload = json!({"hold_id": id, "project_id": project_id, "agent_id": agent_id,
            "pattern": "src/local.rs", "expires_at": expiry + 5000, "exclusive": false,
            "ttl_seconds": 42, "reason": "foreign reason"})
        .to_string();
        let mut event = WireEvent {
            seq: 1,
            event_id: "signed-peer-event".into(),
            node_id: peer_id.clone(),
            ts: 1_000_100,
            actor: "peer".into(),
            kind: "hold.updated".into(),
            payload,
            prev_id: None,
            sig: String::new(),
        };
        event.sig = crypto::sign(
            peer.signing_key(),
            &crypto::event_message(
                &event.event_id,
                &event.node_id,
                event.ts,
                &event.actor,
                &event.kind,
                &event.payload,
            ),
        );
        let report = store.ingest(&peer_id, &[event], &[]).unwrap();
        // The colliding event is now refused outright (D69), not stored and ignored.
        assert_eq!(report.accepted, 0);
        assert_eq!(report.rejected.len(), 1);
        let actual: (i64, bool, i64, Option<String>, i64) = store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT expires_at, exclusive, ttl_seconds, reason, renewed_at FROM holds WHERE hold_id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?)
            })
            .unwrap();
        assert_eq!(
            actual,
            (expiry, true, 1800, Some("local reason".into()), 1_000_000)
        );

        // A real peer-owned project, agent and hold still synchronize normally.
        let peer_project = dir.path().join("peer-project");
        std::fs::create_dir_all(&peer_project).unwrap();
        let peer_root = peer_project.to_str().unwrap().to_owned();
        let peer_actor = peer
            .register_agent(crate::RegisterAgent {
                name: "peer-owner".into(),
                program: crate::Program::Claude,
                project: Some(peer_root.clone()),
                model: None,
                description: None,
            })
            .unwrap()
            .address;
        let peer_hold = peer
            .acquire_holds(crate::AcquireHolds {
                agent: peer_actor.clone(),
                project: peer_root.clone(),
                paths: vec!["src/peer.rs".into()],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
            .granted[0]
            .hold_id
            .clone();
        let (events, _) = peer.outbound_events(0, 100).unwrap();
        let report = store.ingest(&peer_id, &events, &[]).unwrap();
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        // Both clocks move: a receiver never takes a foreign time past its own clock (D72).
        peer_now.store(1_001_000, std::sync::atomic::Ordering::SeqCst);
        store_now.store(1_001_000, std::sync::atomic::Ordering::SeqCst);
        let updated = peer
            .acquire_holds(crate::AcquireHolds {
                agent: peer_actor,
                project: peer_root,
                paths: vec!["src/peer.rs".into()],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: Some("peer update".into()),
                repo_wide: false,
            })
            .unwrap();
        assert_eq!(updated.granted[0].hold_id, peer_hold);
        let (events, _) = peer.outbound_events(0, 100).unwrap();
        let report = store.ingest(&peer_id, &events, &[]).unwrap();
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let peer_expiry: i64 = store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT expires_at FROM holds WHERE hold_id=?1",
                    [&peer_hold],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(peer_expiry, 2_801_000);
    }

    #[test]
    fn extreme_request_times_are_refused_without_overflow() {
        // `x-another-time` is read from an unauthenticated header before any signature
        // check; found by the peer_request fuzz harness.
        let (clock, _) = crate::Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        // The last value made the old wrapping difference exactly i64::MIN, whose wrapping
        // `abs` stays negative: it slipped past the window in a release build.
        let wrap_to_min = 1_800_000_000_000i64.wrapping_add(i64::MIN);
        for time in [i64::MIN, i64::MIN + 1, i64::MAX, wrap_to_min] {
            let err = store
                .verify_peer_request("node", "GET", "/fed/pull", time, b"", "sig")
                .unwrap_err();
            assert!(
                err.to_string().contains("more than 5 minutes off"),
                "{time}: {err}"
            );
        }
    }

    #[test]
    fn classification() {
        let o = "A";
        assert_eq!(classify("token.created", &json!({}), o), Share::Never);
        assert_eq!(classify("oauth.approved", &json!({}), o), Share::Never);
        assert_eq!(
            classify("agent.registered", &json!({}), o),
            Share::Directory
        );
        assert_eq!(
            classify("message.sent", &json!({ "recipient_nodes": ["A"] }), o),
            Share::Never,
            "local-only mail stays local"
        );
        assert_eq!(
            classify("message.sent", &json!({ "recipient_nodes": ["A", "B"] }), o),
            Share::Mail(vec!["B".into()])
        );
        assert_eq!(
            classify("delivery.acked", &json!({ "message_node": "C" }), o),
            Share::Mail(vec!["C".into()])
        );
        assert_eq!(
            classify("hold.acquired", &json!({}), o),
            Share::Feed(vec![])
        );
        assert_eq!(
            classify("resource.borrowed", &json!({ "resource_node": "B" }), o),
            Share::Feed(vec!["B".into()])
        );
    }

    #[test]
    fn reachability() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(reachable(Some(&format!("http://127.0.0.1:{port}/x")), None));
        assert!(reachable(Some("127.0.0.1"), Some(port)));
        drop(l);
        assert!(!reachable(None, Some(port)));
        assert!(!reachable(Some("http://127.0.0.1:1"), None));
    }
}
