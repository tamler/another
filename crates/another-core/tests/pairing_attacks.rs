//! Q27: pairing proves the joiner holds the node's key. Each test is one attack on pairing
//! (A1 to A16 of the Q27 spec) and asserts both the refusal and that every row of `nodes`,
//! `peers` and `invites` is as it was.
//!
//! This file uses only API that existed before the fix, so it also compiles there: requests
//! and answers are built as JSON, and the signed bytes are built by hand from the documented
//! layout. Every request carries the invite's real `secret` so that it parses on a build
//! that still required that field; this build ignores it.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use another_core::test_support::forge_event;
use another_core::*;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const MIN_MS: i64 = 60 * 1000;

/// Well-formed node ids that no test node has.
const ULID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const ULID_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    /// A node named `name` in the directory `home` (two names that differ only in case need
    /// two directories on a case-insensitive file system).
    fn at(dir: &Path, home: &str, name: &str) -> Node {
        let home = dir.join(home);
        let (clock, _) = Clock::manual(T0);
        let test_support::Node { store, db } = test_support::Node::open(&home, name, clock);
        Node { store, db }
    }

    fn new(dir: &Path, name: &str) -> Node {
        Node::at(dir, name, name)
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn entry(&self) -> NodeEntry {
        self.store.node_entry().unwrap()
    }

    /// This node's private key, as stored. Only ever read from a test's temporary home.
    fn secret(&self) -> String {
        self.sql()
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn agent(&self, name: &str) -> String {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .agent_id
    }

    /// Every row of `nodes`, `peers` and `invites`, without the private key column.
    fn rows(&self) -> Vec<String> {
        let conn = self.sql();
        let mut out = Vec::new();
        for table in ["nodes", "peers", "invites"] {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .unwrap();
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut found = stmt.query([]).unwrap();
            while let Some(row) = found.next().unwrap() {
                let mut line = table.to_string();
                for (i, name) in names.iter().enumerate() {
                    if name == "secret_key" {
                        continue;
                    }
                    let value = match row.get_ref(i).unwrap() {
                        ValueRef::Null => "NULL".to_string(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(n) => n.to_string(),
                        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                        ValueRef::Blob(b) => format!("{b:?}"),
                    };
                    line.push_str(&format!(" {name}={value}"));
                }
                out.push(line);
            }
        }
        out
    }

    fn name_of(&self, node_id: &str) -> Option<String> {
        self.sql()
            .query_row(
                "SELECT display_name FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    fn pinned_key(&self, node_id: &str) -> Option<String> {
        self.sql()
            .query_row(
                "SELECT public_key FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
    }

    fn invite_used(&self, invite: &Value) -> bool {
        self.sql()
            .query_row(
                "SELECT used_at IS NOT NULL FROM invites WHERE invite_id = ?1",
                [text(invite, "invite_id")],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The agents the last message sent on this node went to.
    fn last_recipients(&self) -> String {
        let payload: String = self
            .sql()
            .query_row(
                "SELECT payload FROM events WHERE kind = 'message.sent' ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        payload["recipients"].to_string()
    }

    /// A correctly signed event from this node.
    fn signed_event(&self, kind: &str, payload: Value) -> WireEvent {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        forge_event(
            &self.sql(),
            WireEvent {
                seq: 1_000_000 + n as i64,
                event_id: format!("forged-{kind}-{n}"),
                node_id: self.id(),
                ts: T0 + 1000 + n as i64,
                actor: "forger".into(),
                kind: kind.into(),
                payload: payload.to_string(),
                prev_id: None,
                sig: String::new(),
            },
        )
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap()
}

/// A new invite from `hub`: the code and its decoded JSON.
fn invite(hub: &Node, perms: &[Perm]) -> (String, Value) {
    let code = hub
        .store
        .create_invite("https://hub.example", perms, 3600)
        .unwrap();
    (code.clone(), decode(&code))
}

fn decode(code: &str) -> Value {
    let raw = code.strip_prefix("another-invite:").unwrap();
    serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap()
}

fn encode(invite: &Value) -> String {
    format!(
        "another-invite:{}",
        crypto::b64(invite.to_string().as_bytes())
    )
}

/// Sign `invite` as it now reads with the private key `secret_key`. A hub signs every field
/// of its invites, so an invite that anyone else edited is refused for its signature before
/// any other check; the attacks below that edit an invite are made by whoever holds the
/// invite's key, and sign it again. The bytes are built by hand: `another-invite-v1`, the
/// format number, hub id, hub key, hub name, address, invite id, sha256 hex of the secret,
/// the permission names joined by `,`, the expiry; joined by one newline.
fn resign(invite: &mut Value, secret_key: &str) {
    let perms: Vec<&str> = invite["perms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    let bytes = [
        "another-invite-v1".to_string(),
        invite["v"].to_string(),
        text(&invite["hub"], "node_id").to_string(),
        text(&invite["hub"], "public_key").to_string(),
        text(&invite["hub"], "display_name").to_string(),
        text(invite, "hub_url").to_string(),
        text(invite, "invite_id").to_string(),
        crypto::sha256_hex(text(invite, "secret").as_bytes()),
        perms.join(","),
        invite["expires_at"].to_string(),
    ]
    .join("\n");
    invite["sig"] = json!(crypto::sign(
        &crypto::decode_secret(secret_key).unwrap(),
        bytes.as_bytes()
    ));
}

/// The bytes a pairing request signature covers, built by hand:
/// `another-pair-v1`, hub id, hub key, invite id, sha256 hex of the secret, joiner id,
/// joiner key, joiner name, time; joined by one newline.
fn pair_bytes(invite: &Value, secret_hash: &str, node: &NodeEntry, time_ms: i64) -> Vec<u8> {
    [
        "another-pair-v1",
        text(&invite["hub"], "node_id"),
        text(&invite["hub"], "public_key"),
        text(invite, "invite_id"),
        secret_hash,
        &node.node_id,
        &node.public_key,
        &node.display_name,
        &time_ms.to_string(),
    ]
    .join("\n")
    .into_bytes()
}

/// The bytes a pairing answer signature covers, built by hand.
fn accept_bytes(invite: &Value, joiner: &NodeEntry, hub_name: &str, perms: &[Perm]) -> Vec<u8> {
    let perms: Vec<&str> = perms.iter().map(|p| p.as_str()).collect();
    [
        "another-pair-accept-v1",
        text(invite, "invite_id"),
        &crypto::sha256_hex(text(invite, "secret").as_bytes()),
        &joiner.node_id,
        &joiner.public_key,
        hub_name,
        &perms.join(","),
    ]
    .join("\n")
    .into_bytes()
}

fn request_json(invite: &Value, node: &NodeEntry, time_ms: i64, sig: &str) -> Value {
    json!({
        "invite_id": text(invite, "invite_id"),
        "secret": text(invite, "secret"),
        "node": { "node_id": node.node_id, "display_name": node.display_name, "public_key": node.public_key },
        "time_ms": time_ms,
        "sig": sig,
    })
}

/// A pairing request for `node`, with a valid proof made with the private key `secret_key`.
fn signed_request(invite: &Value, node: &NodeEntry, secret_key: &str, time_ms: i64) -> PairRequest {
    let hash = crypto::sha256_hex(text(invite, "secret").as_bytes());
    let sig = crypto::sign(
        &crypto::decode_secret(secret_key).unwrap(),
        &pair_bytes(invite, &hash, node, time_ms),
    );
    serde_json::from_value(request_json(invite, node, time_ms, &sig)).unwrap()
}

fn entry(node_id: &str, name: &str, public_key: &str) -> NodeEntry {
    NodeEntry {
        node_id: node_id.into(),
        display_name: name.into(),
        public_key: public_key.into(),
    }
}

/// A new key pair: (private, public), both as stored text.
fn new_key() -> (String, String) {
    let key = crypto::generate_key().unwrap();
    (crypto::b64(key.as_bytes()), crypto::public_of(&key))
}

fn pair_with(hub: &Node, spoke: &Node, perms: &[Perm]) {
    test_support::pair(&hub.store, &spoke.store, perms);
}

fn pair(hub: &Node, spoke: &Node) {
    pair_with(hub, spoke, &Perm::ALL);
}

/// Push everything `spoke` authored to the hub, as `another sync` does.
fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    hub.store.ingest(&spoke.id(), &events, &[]).unwrap()
}

/// Pull everything the hub has for `spoke`.
fn pull(hub: &Node, spoke: &Node) -> IngestReport {
    let page = hub.store.pull_for(&spoke.id(), 0, 10_000).unwrap();
    spoke
        .store
        .ingest(&hub.id(), &page.events, &page.nodes)
        .unwrap()
}

fn mail(from: &Node, sender: &str, to: &str) -> Result<Message> {
    from.store.send_message(SendMessage {
        from: sender.into(),
        to: vec![to.into()],
        subject: "hello".into(),
        body: "b".into(),
        ..Default::default()
    })
}

/// A1: with one fresh invite, mallory asks to pair as alpha (alpha's id and public key, a
/// new name), then renames itself `alpha`. Before the fix alpha was renamed, un-revoked and
/// given the invite's permissions, and mail to `bob@@alpha` went to mallory's `bob`.
#[test]
fn a1_an_invite_holder_cannot_re_pair_as_another_node() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let mallory = Node::new(dir.path(), "mallory");
    let other = Node::new(dir.path(), "other");
    pair_with(&hub, &alpha, &[Perm::ReadFeed]);
    pair(&hub, &mallory);
    pair(&hub, &other);
    let alpha_bob = alpha.agent("bob");
    let mallory_bob = mallory.agent("bob");
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert!(push(&hub, &mallory).rejected.is_empty());
    hub.store.revoke_peer("alpha").unwrap();

    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let as_alpha = entry(&alpha.id(), "alpha-gone", &alpha.entry().public_key);
    let req = signed_request(&fresh, &as_alpha, &mallory.secret(), T0);
    let refused = hub.store.accept_pairing(&req);
    assert!(
        refused.is_err(),
        "an invite holder re-paired as alpha: {:#?}",
        hub.rows()
    );
    assert_eq!(hub.rows(), before, "a refused pairing changed the hub");
    assert!(!hub.invite_used(&fresh));

    // The second half: mallory takes the name. Alpha still holds it, so the rename is
    // rejected and the address keeps meaning alpha.
    mallory.store.set_node_name("alpha").unwrap();
    let report = push(&hub, &mallory);
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert_eq!(hub.name_of(&alpha.id()).as_deref(), Some("alpha"));
    assert_eq!(hub.name_of(&mallory.id()).as_deref(), Some("mallory"));

    // On the hub the address still means the revoked alpha: mail to it is refused for that
    // node, not delivered to mallory.
    hub.agent("carol");
    let err = mail(&hub, "carol", "bob@@alpha").expect_err("mail to a revoked node was sent");
    assert!(err.to_string().contains(&alpha.id()), "{err}");
    // With the revoke lifted by hand (the test's own edit, only to follow the address to
    // its agent), the mail goes to alpha's `bob`.
    hub.sql()
        .execute(
            "UPDATE peers SET revoked_at = NULL WHERE node_id = ?1",
            [alpha.id()],
        )
        .unwrap();
    mail(&hub, "carol", "bob@@alpha").unwrap();
    let to = hub.last_recipients();
    assert!(
        to.contains(&alpha_bob) && !to.contains(&mallory_bob),
        "{to}"
    );

    pull(&hub, &other);
    other.agent("dave");
    mail(&other, "dave", "bob@@alpha").unwrap();
    let to = other.last_recipients();
    assert!(
        to.contains(&alpha_bob) && !to.contains(&mallory_bob),
        "{to}"
    );
}

/// A2: a node the hub revoked redeems a fresh invite with its own id and key, with a valid
/// proof. It stays revoked and the invite stays unused.
#[test]
fn a2_a_revoked_node_cannot_un_revoke_itself_with_an_invite() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let mallory = Node::new(dir.path(), "mallory");
    pair(&hub, &mallory);
    hub.store.revoke_peer("mallory").unwrap();

    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let req = signed_request(&fresh, &mallory.entry(), &mallory.secret(), T0);
    let err = hub
        .store
        .accept_pairing(&req)
        .expect_err("a revoked node paired itself again with an invite");
    assert_eq!(err.code(), "forbidden", "{err}");
    assert_eq!(hub.rows(), before);
    assert!(!hub.invite_used(&fresh));
}

/// A3: a node row without a key is never given one by pairing. No production path makes
/// such a row, so the test inserts it.
#[test]
fn a3_a_keyless_node_row_is_never_given_a_key_by_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    hub.sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at)
             VALUES (?1, 'ghost', 'peer', 0, ?2)",
            rusqlite::params![ULID_A, T0],
        )
        .unwrap();
    let (private, public) = new_key();
    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let req = signed_request(&fresh, &entry(ULID_A, "ghost", &public), &private, T0);
    let refused = hub.store.accept_pairing(&req);
    assert!(
        refused.is_err(),
        "pairing pinned a key on a keyless node row"
    );
    assert_eq!(hub.pinned_key(ULID_A), None);
    assert_eq!(hub.rows(), before);
}

/// A4: S is a spoke of H and has revoked it. A pairing request to S under H's id, even one
/// H's own key signed, never rewrites S's `hub` row.
#[test]
fn a4_pairing_never_rewrites_a_hub_row() {
    let dir = tempfile::tempdir().unwrap();
    let h = Node::new(dir.path(), "h");
    let s = Node::new(dir.path(), "s");
    pair_with(&h, &s, &[Perm::ReadFeed]);
    s.store.revoke_peer("h").unwrap();

    let (_, fresh) = invite(&s, &Perm::ALL);
    let before = s.rows();
    let req = signed_request(&fresh, &h.entry(), &h.secret(), T0);
    let refused = s.store.accept_pairing(&req);
    assert!(
        refused.is_err(),
        "pairing rewrote the row of this node's hub: {:#?}",
        s.rows()
    );
    assert_eq!(s.rows(), before);
    assert!(s.store.hub().unwrap().is_none(), "the hub was un-revoked");
}

/// A5: alpha's own request, captured, does not work with another invite.
#[test]
fn a5_a_proof_does_not_carry_over_to_another_invite() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let (_, first) = invite(&hub, &Perm::ALL);
    let hash = crypto::sha256_hex(text(&first, "secret").as_bytes());
    let sig = crypto::sign(
        &crypto::decode_secret(&alpha.secret()).unwrap(),
        &pair_bytes(&first, &hash, &alpha.entry(), T0),
    );
    let honest: PairRequest =
        serde_json::from_value(request_json(&first, &alpha.entry(), T0, &sig)).unwrap();
    hub.store.accept_pairing(&honest).unwrap();

    let (_, second) = invite(&hub, &[Perm::ReadFeed]);
    let before = hub.rows();
    let replay: PairRequest =
        serde_json::from_value(request_json(&second, &alpha.entry(), T0, &sig)).unwrap();
    let refused = hub.store.accept_pairing(&replay);
    assert!(
        refused.is_err(),
        "a captured request was accepted with another invite"
    );
    assert_eq!(hub.rows(), before);
    assert!(!hub.invite_used(&second));
}

/// A6: alpha's own request, captured, does not work on another hub.
#[test]
fn a6_a_proof_does_not_carry_over_to_another_hub() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let second_hub = Node::new(dir.path(), "hub2");
    let alpha = Node::new(dir.path(), "alpha");
    let (_, first) = invite(&hub, &Perm::ALL);
    let hash = crypto::sha256_hex(text(&first, "secret").as_bytes());
    let sig = crypto::sign(
        &crypto::decode_secret(&alpha.secret()).unwrap(),
        &pair_bytes(&first, &hash, &alpha.entry(), T0),
    );
    let honest: PairRequest =
        serde_json::from_value(request_json(&first, &alpha.entry(), T0, &sig)).unwrap();
    hub.store.accept_pairing(&honest).unwrap();

    let (_, elsewhere) = invite(&second_hub, &Perm::ALL);
    let before = second_hub.rows();
    let replay: PairRequest =
        serde_json::from_value(request_json(&elsewhere, &alpha.entry(), T0, &sig)).unwrap();
    let refused = second_hub.store.accept_pairing(&replay);
    assert!(
        refused.is_err(),
        "a request captured on one hub was accepted by another"
    );
    assert_eq!(second_hub.rows(), before);
    assert!(!second_hub.invite_used(&elsewhere));
}

/// A7: a correctly signed request outside the 5 minute window is refused, in both
/// directions, and an extreme time neither panics nor wraps into the window.
#[test]
fn a7_a_stale_pairing_request_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    for time_ms in [T0 - 5 * MIN_MS - 1, T0 + 5 * MIN_MS + 1, i64::MIN, i64::MAX] {
        let req = signed_request(&fresh, &alpha.entry(), &alpha.secret(), time_ms);
        let refused = hub.store.accept_pairing(&req);
        assert!(
            refused.is_err(),
            "a request timed {time_ms} was accepted at {T0}"
        );
        assert_eq!(hub.rows(), before, "time {time_ms}");
    }
    // The edge of the window is inside it, and the refusals above used nothing up.
    let req = signed_request(&fresh, &alpha.entry(), &alpha.secret(), T0 - 5 * MIN_MS);
    hub.store.accept_pairing(&req).unwrap();
}

/// A8: a node id is a ULID. Anything else is refused at pairing and in an invite's hub
/// entry, and skipped in a hub's node list without stopping the page.
#[test]
fn a8_a_node_id_must_be_a_ulid() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let bad_ids = [
        String::new(),
        "x@y".to_string(),
        "a b".to_string(),
        "Z".repeat(100_000),
        "local".to_string(),
        "01ARZ3NDEKTSV4RRFFQ69G5FA\u{202e}".to_string(),
        // 26 characters, but `I`, `L`, `O` and `U` are not in the alphabet, the first
        // character is at most `7`, and lower case is not produced.
        "01ARZ3NDEKTSV4RRFFQ69G5FAI".to_string(),
        "81ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
        "01arz3ndektsv4rrffq69g5fav".to_string(),
        "01ARZ3NDEKTSV4RRFFQ69G5FAV0".to_string(),
    ];

    let (private, public) = new_key();
    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    for id in &bad_ids {
        let req = signed_request(&fresh, &entry(id, "joiner", &public), &private, T0);
        let shown: String = id.chars().take(30).collect();
        let err = match hub.store.accept_pairing(&req) {
            Ok(_) => panic!("paired a node whose id is {shown:?}"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "invalid", "{shown:?}: {err}");
        assert!(
            err.to_string().len() < 300,
            "the refusal quotes the whole id"
        );
        assert_eq!(hub.rows(), before, "{shown:?}");
    }

    // The hub entry of an invite.
    let joiner = Node::new(dir.path(), "joiner");
    for id in &bad_ids {
        let mut forged = fresh.clone();
        forged["hub"]["node_id"] = json!(id);
        resign(&mut forged, &hub.secret());
        let shown: String = id.chars().take(30).collect();
        let err = match joiner.store.prepare_join(&encode(&forged)) {
            Ok(_) => panic!("built a pairing request for a hub whose id is {shown:?}"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "invalid", "{shown:?}: {err}");
    }

    // A hub's node list: the entry is skipped and the rest of the page is applied.
    let (_, good_key) = new_key();
    let mut list: Vec<NodeEntry> = bad_ids
        .iter()
        .map(|id| entry(id, "listed", &public))
        .collect();
    list.push(entry(ULID_B, "good", &good_key));
    let before = spoke.rows().len();
    spoke.store.ingest(&hub.id(), &[], &list).unwrap();
    for id in &bad_ids {
        assert_eq!(spoke.name_of(id), None, "a listed non-id was recorded");
    }
    assert_eq!(spoke.name_of(ULID_B).as_deref(), Some("good"));
    assert_eq!(spoke.rows().len(), before + 1);
}

/// A9: `local` means "not named yet", so a request under that name is refused.
#[test]
fn a9_the_name_local_cannot_pair() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let (private, public) = new_key();
    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let req = signed_request(&fresh, &entry(ULID_A, "local", &public), &private, T0);
    let err = hub
        .store
        .accept_pairing(&req)
        .expect_err("a node named local was paired");
    assert_eq!(err.code(), "invalid", "{err}");
    assert_eq!(hub.rows(), before);
}

/// A10: a signed `node.renamed`, or a second `node.created`, with the name `local` is
/// accepted (an honest node's first event carries it) but names no node.
#[test]
fn a10_no_signed_event_names_a_node_local() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let one = Node::new(dir.path(), "one");
    let two = Node::new(dir.path(), "two");
    pair(&hub, &one);
    pair(&hub, &two);
    for (node, kind) in [(&one, "node.renamed"), (&two, "node.created")] {
        let ev = node.signed_event(
            kind,
            json!({ "node_id": node.id(), "display_name": "local" }),
        );
        let report = hub.store.ingest(&node.id(), &[ev], &[]).unwrap();
        assert!(report.rejected.is_empty(), "{kind}: {report:?}");
        assert_eq!(report.accepted, 1, "{kind}: {report:?}");
    }
    assert_eq!(hub.name_of(&one.id()).as_deref(), Some("one"));
    assert_eq!(hub.name_of(&two.id()).as_deref(), Some("two"));
    let named_local: i64 = hub
        .sql()
        .query_row(
            "SELECT count(*) FROM nodes WHERE display_name = 'local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(named_local, 0);
}

/// A11: two names that differ only in case are one name, on all four paths a name arrives
/// by: pairing, a local rename, a signed rename and a hub's node list.
#[test]
fn a11_names_differing_only_in_case_are_one_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let mallory = Node::new(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);

    // Pairing.
    let upper = Node::at(dir.path(), "upper", "Alpha");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let (_, req) = upper.store.prepare_join(&code).unwrap();
    let err = hub
        .store
        .accept_pairing(&req)
        .expect_err("a node named Alpha paired beside alpha");
    assert_eq!(err.code(), "conflict", "{err}");
    assert_eq!(hub.rows(), before);
    assert!(!hub.invite_used(&fresh));

    // A local rename.
    let err = hub.store.set_node_name("ALPHA").unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");

    // A signed rename.
    let ev = mallory.signed_event(
        "node.renamed",
        json!({ "node_id": mallory.id(), "display_name": "aLPHA" }),
    );
    let report = hub.store.ingest(&mallory.id(), &[ev], &[]).unwrap();
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert_eq!(hub.name_of(&mallory.id()).as_deref(), Some("mallory"));

    // A hub's node list: the second node is recorded under its id.
    let (_, key) = new_key();
    let list = [alpha.entry(), entry(ULID_A, "ALPHA", &key)];
    mallory.store.ingest(&hub.id(), &[], &list).unwrap();
    assert_eq!(mallory.name_of(&alpha.id()).as_deref(), Some("alpha"));
    assert_eq!(mallory.name_of(ULID_A).as_deref(), Some(ULID_A));
}

/// A12: an answer that the hub key in the invite did not sign, for this joiner and with
/// exactly this name and these permissions, is refused and nothing is recorded.
#[test]
fn a12_an_unsigned_or_wrongly_signed_answer_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (code, fresh) = invite(&hub, &[Perm::ReadFeed]);
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let genuine = serde_json::to_value(hub.store.accept_pairing(&req).unwrap()).unwrap();
    let before = spoke.rows();
    let (other_private, other_public) = new_key();
    let other_key = crypto::decode_secret(&other_private).unwrap();
    let hub_key = crypto::decode_secret(&hub.secret()).unwrap();

    let mut unsigned = genuine.clone();
    unsigned.as_object_mut().unwrap().remove("sig");
    let mut by_another_key = genuine.clone();
    by_another_key["sig"] = json!(crypto::sign(
        &other_key,
        &accept_bytes(&fresh, &spoke.entry(), "hub", &[Perm::ReadFeed]),
    ));
    let mut renamed = genuine.clone();
    renamed["hub"]["display_name"] = json!("hubbub");
    let mut more_perms = genuine.clone();
    more_perms["perms"] = json!(["read_feed", "send_mail", "borrow_resource"]);
    let mut for_another_joiner = genuine.clone();
    for_another_joiner["sig"] = json!(crypto::sign(
        &hub_key,
        &accept_bytes(
            &fresh,
            &entry(ULID_A, "spoke", &other_public),
            "hub",
            &[Perm::ReadFeed],
        ),
    ));

    for (what, answer) in [
        ("unsigned", unsigned),
        ("signed by another key", by_another_key),
        ("with a changed hub name", renamed),
        ("with changed permissions", more_perms),
        ("made for another joiner", for_another_joiner),
    ] {
        let answer: PairResponse = serde_json::from_value(answer).unwrap();
        let refused = spoke.store.complete_join(&invite, &answer);
        assert!(refused.is_err(), "an answer {what} was accepted");
        assert_eq!(spoke.rows(), before, "an answer {what} was recorded");
        assert!(spoke.store.peers().unwrap().is_empty());
    }

    let answer: PairResponse = serde_json::from_value(genuine).unwrap();
    spoke.store.complete_join(&invite, &answer).unwrap();
}

/// A13: the hub's name in a correctly signed answer follows the rule for a name in a hub's
/// list. One that is not valid, is `local`, or is another node's is not stored: the hub is
/// recorded under its id.
#[test]
fn a13_the_hubs_name_in_the_answer_follows_the_list_rule() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let hub_key = crypto::decode_secret(&hub.secret()).unwrap();
    for (n, name) in ["a@b", "local", "", "own id"].into_iter().enumerate() {
        let spoke = Node::new(dir.path(), &format!("spoke{n}"));
        let name = if name == "own id" {
            spoke.id()
        } else {
            name.to_string()
        };
        let (code, fresh) = invite(&hub, &Perm::ALL);
        let (invite, req) = spoke.store.prepare_join(&code).unwrap();
        let mut answer = serde_json::to_value(hub.store.accept_pairing(&req).unwrap()).unwrap();
        answer["hub"]["display_name"] = json!(name);
        answer["sig"] = json!(crypto::sign(
            &hub_key,
            &accept_bytes(&fresh, &spoke.entry(), &name, &Perm::ALL),
        ));
        let answer: PairResponse = serde_json::from_value(answer).unwrap();
        spoke.store.complete_join(&invite, &answer).unwrap();
        assert_eq!(
            spoke.name_of(&hub.id()),
            Some(hub.id()),
            "the hub name {name:?} from an answer was stored"
        );
    }
}

/// A14: no error about an invite code quotes anything from the code.
#[test]
fn a14_no_invite_error_quotes_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let secret = text(&fresh, "secret").to_string();

    // The key `secret` renamed to `perms`: the parser meets the secret where it expects a
    // list of permissions.
    let mut misplaced = fresh.clone();
    let object = misplaced.as_object_mut().unwrap();
    object.remove("secret");
    object.insert("perms".into(), json!(secret));
    let raw = code.strip_prefix("another-invite:").unwrap();
    let mut codes = vec![
        encode(&misplaced),
        format!("another-invite:{}", &raw[..raw.len() / 2]),
        format!("another-invite:{}", &raw[..raw.len() - 1]),
        format!("another-invite:!{}", &raw[1..]),
        format!("another-invite:AAAA{}", &raw[4..]),
        "another-invite:".to_string(),
    ];
    codes.push(encode(&json!({ "secret": secret })));
    for code in codes {
        let err = spoke
            .store
            .prepare_join(&code)
            .expect_err("a broken invite code was accepted");
        assert_eq!(err.code(), "invalid", "{err}");
        assert_eq!(err.to_string(), "invalid invite code");
        assert!(!err.to_string().contains(&secret));
    }
}

/// A15: one undecodable key in a hub's node list is skipped; the rest of the page applies.
#[test]
fn a15_an_undecodable_key_in_a_hubs_list_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let (_, good_key) = new_key();
    let list = [
        entry(ULID_A, "broken", "not a key"),
        entry(
            "01CX5ZZKBKACTAV9WEVGEMMVRZ",
            "short",
            &crypto::b64(&[7u8; 5]),
        ),
        entry(ULID_B, "good", &good_key),
    ];
    let applied = spoke.store.ingest(&hub.id(), &[], &list);
    assert!(
        applied.is_ok(),
        "one bad key stopped the whole page: {applied:?}"
    );
    assert_eq!(spoke.name_of(ULID_A), None);
    assert_eq!(spoke.name_of("01CX5ZZKBKACTAV9WEVGEMMVRZ"), None);
    assert_eq!(spoke.name_of(ULID_B).as_deref(), Some("good"));
}

/// A16: the request a joiner sends holds neither the invite's secret nor its hash, so a
/// watcher on the path learns nothing it can redeem. And the `secret` field buys nothing:
/// a request carrying the right secret, with a signature made without its hash, is refused.
#[test]
fn a16_a_pairing_request_does_not_carry_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let secret = text(&fresh, "secret");
    let hash = crypto::sha256_hex(secret.as_bytes());
    let (_, req) = spoke.store.prepare_join(&code).unwrap();
    let sent = serde_json::to_string(&req).unwrap();
    assert!(
        !sent.contains(secret),
        "the request carries the invite secret"
    );
    assert!(!sent.contains(&hash), "the request carries the secret hash");
    assert!(!format!("{req:?}").contains(secret));

    // The watcher's own node, with the secret it could once read off the wire but without
    // a signature over its hash.
    let watcher = Node::new(dir.path(), "watcher");
    let before = hub.rows();
    let guess = crypto::sha256_hex(b"not the secret");
    let sig = crypto::sign(
        &crypto::decode_secret(&watcher.secret()).unwrap(),
        &pair_bytes(&fresh, &guess, &watcher.entry(), T0),
    );
    let req: PairRequest =
        serde_json::from_value(request_json(&fresh, &watcher.entry(), T0, &sig)).unwrap();
    let err = hub
        .store
        .accept_pairing(&req)
        .expect_err("the secret alone redeemed the invite");
    assert_eq!(err.code(), "forbidden", "{err}");
    assert_eq!(hub.rows(), before);
}

/// A17 (review R1, P2-1): the mirror of A4 on the joiner's side. A spoke makes an invite of
/// its own and hands the code to its hub's owner, who runs `another pair join` on it. The
/// spoke answers with its own key, which is the hub key of that invite. Before the fix the
/// hub's `spoke` row took the permissions the answer named and lost its revoke. A join never
/// changes a row whose role is not `hub`: it is refused before anything is sent, and again
/// before anything is written.
#[test]
fn a17_joining_never_rewrites_a_spoke_row() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let mallory = Node::new(dir.path(), "mallory");
    let plain = Node::new(dir.path(), "plain");
    pair(&hub, &mallory);
    pair_with(&hub, &plain, &[Perm::ReadFeed]);
    // Mallory is revoked and its name released, as `another pair release-name` leaves it.
    hub.store.revoke_peer("mallory").unwrap();
    hub.sql()
        .execute(
            "UPDATE nodes SET display_name = node_id WHERE node_id = ?1",
            [mallory.id()],
        )
        .unwrap();

    for (spoke, known_as) in [(&mallory, mallory.id()), (&plain, "plain".to_string())] {
        let (code, fresh) = invite(spoke, &Perm::ALL);
        let before = hub.rows();
        let expected =
            format!("node {known_as} is paired here as a spoke; it cannot also be this node's hub");

        // As the command runs it: refused from the invite alone, so nothing is sent.
        let err = match hub.store.prepare_join(&code) {
            Ok(_) => panic!("built a request to join {known_as}, a spoke of this node"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "conflict", "{err}");
        assert_eq!(err.to_string(), expected);
        assert_eq!(hub.rows(), before);

        // And with an answer in hand, correctly signed by the invite's hub key.
        let made: Invite = serde_json::from_value(fresh.clone()).unwrap();
        let name = spoke.entry().display_name;
        let sig = crypto::sign(
            &crypto::decode_secret(&spoke.secret()).unwrap(),
            &accept_bytes(&fresh, &hub.entry(), &name, &Perm::ALL),
        );
        let answer: PairResponse = serde_json::from_value(json!({
            "hub": spoke.entry(),
            "perms": ["read_feed", "send_mail", "borrow_resource"],
            "sig": sig,
        }))
        .unwrap();
        let err = match hub.store.complete_join(&made, &answer) {
            Ok(peer) => panic!("a join rewrote the row of a spoke: {peer:?}"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "conflict", "{err}");
        assert_eq!(err.to_string(), expected);
        assert_eq!(
            hub.rows(),
            before,
            "a refused join changed {known_as}'s row"
        );
        assert!(!spoke.invite_used(&fresh));
    }
    assert!(hub.store.hub().unwrap().is_none());
}

/// The answer a hostile inviter gives: the invite's hub entry, signed with the key the
/// invite carries, which the inviter holds.
fn answer_signed_by(invite: &Value, joiner: &Node, secret_key: &str) -> PairResponse {
    let name = text(&invite["hub"], "display_name");
    let sig = crypto::sign(
        &crypto::decode_secret(secret_key).unwrap(),
        &accept_bytes(invite, &joiner.entry(), name, &Perm::ALL),
    );
    serde_json::from_value(json!({
        "hub": invite["hub"],
        "perms": ["read_feed", "send_mail", "borrow_resource"],
        "sig": sig,
    }))
    .unwrap()
}

/// A18 (review R2, P2-A): an invite whose hub id is the joining node's own id, with a key
/// the inviter holds. Before the fix the join recorded the node as its own hub at the
/// inviter's URL: the one id whose key the node knows for certain was the one id never
/// compared. And the rule for the whole class: for an id this node already knows, the key
/// in the invite must be the key it has stored, or nothing is sent.
#[test]
fn a18_an_invite_cannot_name_this_node_or_swap_a_known_nodes_key() {
    let dir = tempfile::tempdir().unwrap();
    let solo = Node::new(dir.path(), "solo");
    let mallory = Node::new(dir.path(), "mallory");
    let (_, made) = invite(&mallory, &Perm::ALL);

    // This node's own id.
    let mut own = made.clone();
    own["hub"]["node_id"] = json!(solo.id());
    own["hub"]["display_name"] = json!("evil");
    resign(&mut own, &mallory.secret());
    let before = solo.rows();
    let expected = "this invite names this node itself as its hub; a node cannot join itself";
    let err = match solo.store.prepare_join(&encode(&own)) {
        Ok(_) => panic!("built a request to join this node itself"),
        Err(e) => e,
    };
    assert_eq!(
        (err.code(), err.to_string().as_str()),
        ("conflict", expected)
    );
    let as_invite: Invite = serde_json::from_value(own.clone()).unwrap();
    let answer = answer_signed_by(&own, &solo, &mallory.secret());
    let err = match solo.store.complete_join(&as_invite, &answer) {
        Ok(peer) => panic!("a node became its own hub: {peer:?}"),
        Err(e) => e,
    };
    assert_eq!(
        (err.code(), err.to_string().as_str()),
        ("conflict", expected)
    );
    assert_eq!(solo.rows(), before);
    assert!(solo.store.peers().unwrap().is_empty());

    // A node this one knows, as its hub or only from a list, under another key.
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let listed = Node::new(dir.path(), "listed");
    pair(&hub, &spoke);
    pair(&hub, &listed);
    pull(&hub, &spoke);
    assert_eq!(spoke.name_of(&listed.id()).as_deref(), Some("listed"));
    solo.sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'listed', 'peer', 0, ?2, ?3)",
            rusqlite::params![listed.id(), T0, listed.entry().public_key],
        )
        .unwrap();
    for (joiner, known, name) in [(&spoke, hub.id(), "hub"), (&solo, listed.id(), "listed")] {
        let mut swapped = made.clone();
        swapped["hub"]["node_id"] = json!(known);
        swapped["hub"]["display_name"] = json!(name);
        resign(&mut swapped, &mallory.secret());
        let before = joiner.rows();
        let expected = format!("node {known} presented a different key than before; refusing");
        let err = match joiner.store.prepare_join(&encode(&swapped)) {
            Ok(_) => panic!("built a request to join {name} under a key it does not have"),
            Err(e) => e,
        };
        assert_eq!(
            (err.code(), err.to_string()),
            ("conflict", expected.clone())
        );
        let as_invite: Invite = serde_json::from_value(swapped.clone()).unwrap();
        let answer = answer_signed_by(&swapped, joiner, &mallory.secret());
        let err = match joiner.store.complete_join(&as_invite, &answer) {
            Ok(peer) => panic!("{name} was joined under another key: {peer:?}"),
            Err(e) => e,
        };
        assert_eq!((err.code(), err.to_string()), ("conflict", expected));
        assert_eq!(joiner.rows(), before);
    }
    assert!(!mallory.invite_used(&made));
}
