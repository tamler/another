//! Q27: the rules of pairing that are not an attack: what a first pairing and a second one
//! record, what each refusal says, `pair readmit`, `pair release-name`, the recoveries they
//! serve, and what stays as it was for nodes that are already paired.
//!
//! The attacks themselves are in `pairing_attacks.rs`, which also compiles on the build
//! before the fix. This file uses the new API and cannot.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const ULID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const ONE_TEXT: &str = "invalid, used or expired invite, or the request is not signed for it";

struct Node {
    store: Store,
    home: std::path::PathBuf,
    clock: Arc<AtomicI64>,
}

impl Node {
    /// A node named `name` in the directory `home`.
    fn at(dir: &Path, home: &str, name: &str) -> Node {
        let home = dir.join(home);
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            home,
            clock: handle,
        }
    }

    fn new(dir: &Path, name: &str) -> Node {
        Node::at(dir, name, name)
    }

    fn sql(&self) -> Connection {
        Connection::open(self.home.join(DB_FILE)).unwrap()
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

    fn peer(&self, node_id: &str) -> Peer {
        self.store
            .peers()
            .unwrap()
            .into_iter()
            .find(|p| p.node_id == node_id)
            .unwrap()
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

    /// The payload of this node's last event of `kind`.
    fn last_event(&self, kind: &str) -> Value {
        let payload: String = self
            .sql()
            .query_row(
                "SELECT payload FROM events WHERE kind = ?1 ORDER BY seq DESC LIMIT 1",
                [kind],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }

    fn last_actor(&self, kind: &str) -> String {
        self.sql()
            .query_row(
                "SELECT actor FROM events WHERE kind = ?1 ORDER BY seq DESC LIMIT 1",
                [kind],
                |r| r.get(0),
            )
            .unwrap()
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap()
}

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

fn entry(node_id: &str, name: &str, public_key: &str) -> NodeEntry {
    NodeEntry {
        node_id: node_id.into(),
        display_name: name.into(),
        public_key: public_key.into(),
    }
}

/// A pairing request for `node` with a valid proof made with the private key `secret_key`.
fn signed_request(invite: &Value, node: &NodeEntry, secret_key: &str, time_ms: i64) -> PairRequest {
    let sig = crypto::sign(
        &crypto::decode_secret(secret_key).unwrap(),
        &crypto::pair_message(
            text(&invite["hub"], "node_id"),
            text(&invite["hub"], "public_key"),
            text(invite, "invite_id"),
            &crypto::sha256_hex(text(invite, "secret").as_bytes()),
            &node.node_id,
            &node.public_key,
            &node.display_name,
            time_ms,
        ),
    );
    PairRequest {
        invite_id: text(invite, "invite_id").into(),
        node: node.clone(),
        time_ms,
        sig,
    }
}

fn new_key() -> (String, String) {
    let key = crypto::generate_key().unwrap();
    (crypto::b64(key.as_bytes()), crypto::public_of(&key))
}

fn pair_with(hub: &Node, spoke: &Node, perms: &[Perm]) -> Value {
    let (code, decoded) = invite(hub, perms);
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
    decoded
}

fn pair(hub: &Node, spoke: &Node) -> Value {
    pair_with(hub, spoke, &Perm::ALL)
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

/// Whether the hub accepts a request `spoke` signs now, as `/fed/push` checks it.
fn signed_push_accepted(hub: &Node, spoke: &Node) -> bool {
    let sig = spoke.store.sign_request("POST", "/fed/push", T0, b"{}");
    hub.store
        .verify_peer_request(&spoke.id(), "POST", "/fed/push", T0, b"{}", &sig)
        .is_ok()
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

fn recipients(node: &Node) -> String {
    node.last_event("message.sent")["recipients"].to_string()
}

#[test]
fn first_pairing_works_and_records_the_proof() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let used = pair_with(&hub, &spoke, &[Perm::ReadFeed, Perm::SendMail]);

    let paired = hub.last_event("peer.paired");
    assert_eq!(paired["node_id"], json!(spoke.id()));
    assert_eq!(paired["display_name"], json!("spoke"));
    assert_eq!(paired["role"], json!("spoke"));
    assert_eq!(paired["perms"], json!(["read_feed", "send_mail"]));
    assert_eq!(paired["again"], json!(false));
    assert_eq!(paired["invite_id"], used["invite_id"]);
    assert_eq!(
        paired["fingerprint"],
        json!(crypto::fingerprint(&spoke.entry().public_key))
    );
    assert!(paired.get("previous_name").is_none());
    assert!(paired.get("previous_perms").is_none());
    assert!(hub.invite_used(&used));
    let used_by: String = hub
        .sql()
        .query_row("SELECT used_by FROM invites", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used_by, spoke.id());

    // The spoke's record of the same pairing.
    let joined = spoke.last_event("peer.paired");
    assert_eq!(joined["role"], json!("hub"));
    assert_eq!(joined["again"], json!(false));
    assert_eq!(joined["invite_id"], used["invite_id"]);
    assert_eq!(
        joined["fingerprint"],
        json!(crypto::fingerprint(&hub.entry().public_key))
    );

    assert!(signed_push_accepted(&hub, &spoke));
    spoke.agent("bob");
    assert!(push(&hub, &spoke).rejected.is_empty());
    pull(&hub, &spoke);
}

/// A joiner from before signed pairing sends `{invite_id, secret, node}`. It is told to
/// upgrade, and the invite is not used.
#[test]
fn unsigned_pairing_is_refused_with_the_upgrade_message() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (_, fresh) = invite(&hub, &Perm::ALL);
    let before = hub.rows();
    let old: PairRequest = serde_json::from_value(json!({
        "invite_id": text(&fresh, "invite_id"),
        "secret": text(&fresh, "secret"),
        "node": spoke.entry(),
    }))
    .unwrap();
    let (result, cause) = hub.store.accept_pairing_with_cause(&old);
    let err = result.unwrap_err();
    assert_eq!(err.code(), "forbidden");
    assert_eq!(
        err.to_string(),
        "this hub accepts only signed pairing requests; upgrade the joining node; \
         an invite made before the hub was upgraded must be made again"
    );
    assert_eq!(cause, None);
    assert_eq!(hub.rows(), before);
    assert!(!hub.invite_used(&fresh));
}

#[test]
fn a_request_signed_by_another_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let (other_private, _) = new_key();
    let before = hub.rows();

    let by_another_key = signed_request(&fresh, &spoke.entry(), &other_private, T0);
    let (_, honest) = spoke.store.prepare_join(&code).unwrap();
    let mut renamed = honest.clone();
    renamed.node.display_name = "spoke2".into();
    let mut other_id = honest.clone();
    other_id.node.node_id = ULID_A.into();
    let mut other_time = honest.clone();
    other_time.time_ms += 1;
    let mut cut = honest.clone();
    cut.sig.pop();

    for (what, req) in [
        ("signed by another key", by_another_key),
        ("with the name changed after signing", renamed),
        ("with the id changed after signing", other_id),
        ("with the time changed after signing", other_time),
        ("with a cut signature", cut),
    ] {
        let (result, cause) = hub.store.accept_pairing_with_cause(&req);
        let err = match result {
            Ok(_) => panic!("a request {what} was accepted"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "forbidden", "{what}: {err}");
        assert_eq!(err.to_string(), ONE_TEXT, "{what}");
        assert_eq!(cause, Some(PairRefusal::BadSignature), "{what}");
        assert_eq!(hub.rows(), before, "{what}");
    }
    hub.store.accept_pairing(&honest).unwrap();
}

/// A known id with another key is refused even with a valid proof under the new key, and
/// whatever the state of the known node. There is no override.
#[test]
fn a_known_id_with_a_different_key_is_always_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair(&hub, &alpha);
    let (private, public) = new_key();
    for revoked in [false, true] {
        if revoked {
            hub.store.revoke_peer("alpha").unwrap();
        }
        let (_, fresh) = invite(&hub, &Perm::ALL);
        let before = hub.rows();
        for name in ["alpha", "alpha2"] {
            let req = signed_request(&fresh, &entry(&alpha.id(), name, &public), &private, T0);
            let err = hub.store.accept_pairing(&req).unwrap_err();
            assert_eq!(err.code(), "conflict", "{err}");
            assert_eq!(
                err.to_string(),
                format!(
                    "node {} presented a different key than before; refusing",
                    alpha.id()
                )
            );
            assert_eq!(hub.rows(), before);
        }
    }
}

#[test]
fn a_paired_node_re_pairs_with_its_own_key() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let beta = Node::new(dir.path(), "beta");
    pair(&hub, &alpha);
    pair(&hub, &beta);
    hub.sql()
        .execute(
            "UPDATE peers SET push_cursor = 7, pull_cursor = 9, last_sync_at = 5 WHERE node_id = ?1",
            [alpha.id()],
        )
        .unwrap();
    let before = hub.rows();
    hub.clock.fetch_add(1000, Ordering::SeqCst);
    alpha.clock.fetch_add(1000, Ordering::SeqCst);

    let used = pair_with(&hub, &alpha, &[Perm::ReadFeed]);
    let peer = hub.peer(&alpha.id());
    assert_eq!(peer.perms, [Perm::ReadFeed]);
    assert_eq!(peer.display_name, "alpha");
    assert_eq!(peer.revoked_at, None);
    assert_eq!((peer.push_cursor, peer.pull_cursor), (7, 9));
    assert_eq!(alpha.peer(&hub.id()).perms, [Perm::ReadFeed]);

    // Only alpha's `peers` row (permissions and `paired_at`) and the new invite differ.
    let after = hub.rows();
    let changed: Vec<&String> = after.iter().filter(|l| !before.contains(l)).collect();
    assert_eq!(changed.len(), 2, "{changed:#?}");
    assert!(changed[0].starts_with(&format!("peers node_id={} role=spoke", alpha.id())));
    assert!(changed[0].contains(&format!("paired_at={}", T0 + 1000)));
    assert!(changed[0].contains("last_sync_at=5 revoked_at=NULL"));
    assert!(changed[1].starts_with("invites "));

    let paired = hub.last_event("peer.paired");
    assert_eq!(paired["again"], json!(true));
    assert_eq!(paired["invite_id"], used["invite_id"]);
    assert_eq!(paired["perms"], json!(["read_feed"]));
    assert_eq!(paired["previous_name"], json!("alpha"));
    assert_eq!(
        paired["previous_perms"],
        json!(["read_feed", "send_mail", "borrow_resource"])
    );
    assert_eq!(spoke_again(&alpha), json!(true));
    assert!(signed_push_accepted(&hub, &alpha));
}

fn spoke_again(spoke: &Node) -> Value {
    spoke.last_event("peer.paired")["again"].clone()
}

#[test]
fn a_re_pair_may_rename_by_the_rename_rule_only() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let gamma = Node::new(dir.path(), "gamma");
    pair(&hub, &alpha);
    pair(&hub, &gamma);

    // A name the rename rule refuses: the whole request is refused.
    let (_, fresh) = invite(&hub, &[Perm::ReadFeed]);
    let before = hub.rows();
    for (name, code) in [
        ("gamma", "conflict"),
        ("GAMMA", "conflict"),
        ("hub", "conflict"),
        ("local", "invalid"),
        ("a b", "invalid"),
        ("", "invalid"),
    ] {
        let req = signed_request(
            &fresh,
            &entry(&alpha.id(), name, &alpha.entry().public_key),
            &alpha.secret(),
            T0,
        );
        let err = match hub.store.accept_pairing(&req) {
            Ok(_) => panic!("a second pairing renamed alpha to {name:?}"),
            Err(e) => e,
        };
        assert_eq!(err.code(), code, "{name:?}: {err}");
        if code == "conflict" {
            assert_eq!(
                err.to_string(),
                format!("a node named {name:?} is already known; rename the joining node")
            );
        }
        assert_eq!(hub.rows(), before, "{name:?}");
    }

    // A free, valid name is applied, and the record says what it was.
    alpha.store.set_node_name("beta").unwrap();
    let (code, _) = invite(&hub, &[Perm::SendMail]);
    let (_, req) = alpha.store.prepare_join(&code).unwrap();
    hub.store.accept_pairing(&req).unwrap();
    assert_eq!(hub.name_of(&alpha.id()).as_deref(), Some("beta"));
    let paired = hub.last_event("peer.paired");
    assert_eq!(paired["display_name"], json!("beta"));
    assert_eq!(paired["previous_name"], json!("alpha"));
    assert_eq!(paired["again"], json!(true));
    assert_eq!(hub.peer(&alpha.id()).perms, [Perm::SendMail]);
}

/// The hub knows a node's id and key from a list but never paired with it. Its direct join
/// is a first pairing, except that the node row exists.
#[test]
fn a_known_node_without_a_peers_row_pairs_as_new() {
    let dir = tempfile::tempdir().unwrap();
    let top = Node::new(dir.path(), "top");
    let mid = Node::new(dir.path(), "mid");
    let alpha = Node::at(dir.path(), "alpha", "alpha");
    let beta = Node::at(dir.path(), "beta", "beta");
    pair(&top, &mid);
    pair(&top, &alpha);
    pair(&top, &beta);
    pull(&top, &mid);
    assert_eq!(mid.name_of(&alpha.id()).as_deref(), Some("alpha"));
    assert!(mid.store.peers().unwrap().iter().all(|p| p.role == "hub"));

    // Under the name `mid` has for it.
    let (_, fresh) = invite(&mid, &[Perm::ReadFeed]);
    let req = signed_request(&fresh, &alpha.entry(), &alpha.secret(), T0);
    mid.store.accept_pairing(&req).unwrap();
    let peer = mid.peer(&alpha.id());
    assert_eq!(
        (peer.role.as_str(), &peer.perms[..]),
        ("spoke", &[Perm::ReadFeed][..])
    );
    let paired = mid.last_event("peer.paired");
    assert_eq!(paired["again"], json!(false));
    assert!(paired.get("previous_name").is_none());

    // Under a new name: the rename rule decides. A taken one is refused whole.
    let (_, fresh) = invite(&mid, &[Perm::ReadFeed]);
    let before = mid.rows();
    let taken = entry(&beta.id(), "alpha", &beta.entry().public_key);
    let err = mid
        .store
        .accept_pairing(&signed_request(&fresh, &taken, &beta.secret(), T0))
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert_eq!(mid.rows(), before);
    let renamed = entry(&beta.id(), "beta2", &beta.entry().public_key);
    mid.store
        .accept_pairing(&signed_request(&fresh, &renamed, &beta.secret(), T0))
        .unwrap();
    assert_eq!(mid.name_of(&beta.id()).as_deref(), Some("beta2"));

    // A different key for an id known only from a list is refused like any other.
    let (private, public) = new_key();
    let (_, fresh) = invite(&mid, &Perm::ALL);
    let err = mid
        .store
        .accept_pairing(&signed_request(
            &fresh,
            &entry(&top.id(), "imposter", &public),
            &private,
            T0,
        ))
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
}

/// Two valid joiners redeem one invite at the same moment through two handles on one
/// database file. Exactly one is paired.
#[test]
fn two_requests_race_for_one_invite() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let second_handle = Store::open_with_clock(&hub.home, Clock::manual(T0).0).unwrap();
    let one = Node::new(dir.path(), "one");
    let two = Node::new(dir.path(), "two");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let (_, first) = one.store.prepare_join(&code).unwrap();
    let (_, second) = two.store.prepare_join(&code).unwrap();

    let start = Arc::new(std::sync::Barrier::new(2));
    let run = |store: Store, req: PairRequest| {
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            store.accept_pairing_with_cause(&req)
        })
    };
    let a = run(hub.store.clone(), first);
    let b = run(second_handle, second);
    let results = [a.join().unwrap(), b.join().unwrap()];

    let won = results.iter().filter(|(r, _)| r.is_ok()).count();
    assert_eq!(won, 1, "{results:?}");
    let (lost, cause) = results.iter().find(|(r, _)| r.is_err()).unwrap();
    assert_eq!(lost.as_ref().unwrap_err().to_string(), ONE_TEXT);
    let peers = hub.store.peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(
        cause,
        &Some(PairRefusal::UsedInvite {
            used_by: peers[0].node_id.clone()
        })
    );
    assert!(hub.invite_used(&fresh));
}

#[test]
fn a_spoke_that_revoked_its_hub_joins_again() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    spoke.store.revoke_peer("hub").unwrap();
    assert!(spoke.store.hub().unwrap().is_none());

    pair_with(&hub, &spoke, &[Perm::ReadFeed, Perm::SendMail]);
    assert_eq!(spoke.store.hub().unwrap().map(|(id, _)| id), Some(hub.id()));
    assert_eq!(spoke.peer(&hub.id()).revoked_at, None);
    assert_eq!(hub.last_event("peer.paired")["again"], json!(true));
    assert_eq!(spoke_again(&spoke), json!(true));
    spoke.agent("bob");
    assert!(push(&hub, &spoke).rejected.is_empty());
    pull(&hub, &spoke);
    assert!(signed_push_accepted(&hub, &spoke));
}

#[test]
fn readmit_lets_a_revoked_node_sync_again() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair_with(&hub, &alpha, &[Perm::ReadFeed, Perm::SendMail]);

    // The three errors.
    let err = hub.store.readmit_peer("nobody").unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    let err = hub.store.readmit_peer("alpha").unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(err.to_string(), "node alpha is not revoked");
    let err = alpha.store.readmit_peer("hub").unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert_eq!(
        err.to_string(),
        "hub is this node's hub; join it again with a fresh invite"
    );
    alpha.store.revoke_peer("hub").unwrap();
    assert_eq!(
        alpha.store.readmit_peer("hub").unwrap_err().code(),
        "invalid"
    );

    hub.store.revoke_peer("alpha").unwrap();
    assert!(!signed_push_accepted(&hub, &alpha));
    let before = hub.rows();
    let peer = hub.store.readmit_peer(&alpha.id()).unwrap();
    assert_eq!(peer.revoked_at, None);
    assert_eq!(peer.perms, [Perm::ReadFeed, Perm::SendMail]);
    assert_eq!(peer.display_name, "alpha");
    let fingerprint = crypto::fingerprint(&alpha.entry().public_key);
    assert_eq!(peer.fingerprint, fingerprint);

    // Only `revoked_at` on alpha's row changed.
    let after = hub.rows();
    let changed: Vec<&String> = after.iter().filter(|l| !before.contains(l)).collect();
    assert_eq!(changed.len(), 1, "{changed:#?}");
    assert!(changed[0].starts_with(&format!("peers node_id={}", alpha.id())));
    assert!(changed[0].ends_with("revoked_at=NULL"));

    let event = hub.last_event("peer.readmitted");
    assert_eq!(
        event,
        json!({
            "node_id": alpha.id(),
            "display_name": "alpha",
            "perms": ["read_feed", "send_mail"],
            "fingerprint": fingerprint,
        })
    );
    assert_eq!(
        hub.last_actor("peer.readmitted"),
        "human (another pair readmit)"
    );

    assert!(signed_push_accepted(&hub, &alpha));
    alpha.agent("bob");
    assert!(push(&hub, &alpha).rejected.is_empty());
    // By name as well as by id.
    hub.store.revoke_peer("alpha").unwrap();
    hub.store.readmit_peer("alpha").unwrap();
}

#[test]
fn release_name_frees_only_a_revoked_nodes_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let beta = Node::new(dir.path(), "beta");
    pair(&hub, &alpha);
    pair(&hub, &beta);
    alpha.agent("bob");
    assert!(push(&hub, &alpha).rejected.is_empty());

    let err = hub.store.release_peer_name("nobody").unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    let err = hub.store.release_peer_name("alpha").unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(
        err.to_string(),
        "node alpha is not revoked; revoke it first"
    );

    hub.store.revoke_peer("alpha").unwrap();
    let before = hub.rows();
    let agents_before: i64 = hub
        .sql()
        .query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    let (peer, previous) = hub.store.release_peer_name("alpha").unwrap();
    assert_eq!(previous, "alpha");
    assert_eq!(peer.display_name, alpha.id());
    assert_eq!(peer.node_id, alpha.id());
    assert!(peer.revoked_at.is_some());
    assert_eq!(hub.name_of(&alpha.id()), Some(alpha.id()));

    // Only that node's name changed.
    let after = hub.rows();
    let changed: Vec<&String> = after.iter().filter(|l| !before.contains(l)).collect();
    assert_eq!(changed.len(), 1, "{changed:#?}");
    assert!(changed[0].starts_with(&format!("nodes node_id={0} display_name={0} ", alpha.id())));
    let agents_after: i64 = hub
        .sql()
        .query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(agents_after, agents_before);

    let fingerprint = crypto::fingerprint(&alpha.entry().public_key);
    assert_eq!(
        hub.last_event("peer.name_released"),
        json!({ "node_id": alpha.id(), "previous_name": "alpha", "fingerprint": fingerprint })
    );
    assert_eq!(
        hub.last_actor("peer.name_released"),
        "human (another pair release-name)"
    );

    let err = hub.store.release_peer_name(&alpha.id()).unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(
        err.to_string(),
        format!("node {} has no name to release", alpha.id())
    );

    // The name is free: another node takes it with a signed rename.
    beta.store.set_node_name("alpha").unwrap();
    assert!(push(&hub, &beta).rejected.is_empty());
    assert_eq!(hub.name_of(&beta.id()).as_deref(), Some("alpha"));
}

/// The state the old attack left, built by SQL because the attack can no longer make it:
/// on the hub the victim is named `alpha-gone` and the attacker holds `alpha`. The
/// documented steps, with commands only, give the name back.
#[test]
fn recovery_frees_a_taken_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let mallory = Node::new(dir.path(), "mallory");
    let other = Node::new(dir.path(), "other");
    pair(&hub, &alpha);
    pair(&hub, &mallory);
    pair(&hub, &other);
    let alpha_bob = alpha.agent("bob");
    let mallory_bob = mallory.agent("bob");
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert!(push(&hub, &mallory).rejected.is_empty());
    hub.sql()
        .execute(
            "UPDATE nodes SET display_name = 'alpha-gone' WHERE node_id = ?1",
            [alpha.id()],
        )
        .unwrap();
    mallory.store.set_node_name("alpha").unwrap();
    assert!(push(&hub, &mallory).rejected.is_empty());
    pull(&hub, &other);
    assert_eq!(hub.name_of(&mallory.id()).as_deref(), Some("alpha"));
    assert_eq!(other.name_of(&mallory.id()).as_deref(), Some("alpha"));
    assert_eq!(other.name_of(&alpha.id()).as_deref(), Some("alpha-gone"));
    hub.agent("carol");
    other.agent("dave");
    mail(&hub, "carol", "bob@@alpha").unwrap();
    assert!(
        recipients(&hub).contains(&mallory_bob),
        "not the attacked state"
    );

    // 1 and 2, on the hub, by id.
    hub.store.revoke_peer(&mallory.id()).unwrap();
    hub.store.release_peer_name(&mallory.id()).unwrap();
    // 3, on the victim.
    pull(&hub, &alpha);
    alpha.store.set_node_name("alpha").unwrap();
    assert!(push(&hub, &alpha).rejected.is_empty());
    pull(&hub, &alpha);
    // 5, on the other spokes, twice.
    pull(&hub, &other);
    pull(&hub, &other);

    for node in [&hub, &other] {
        assert_eq!(node.name_of(&alpha.id()).as_deref(), Some("alpha"));
        assert_eq!(node.name_of(&mallory.id()), Some(mallory.id()));
    }
    mail(&hub, "carol", "bob@@alpha").unwrap();
    let to = recipients(&hub);
    assert!(
        to.contains(&alpha_bob) && !to.contains(&mallory_bob),
        "{to}"
    );
    mail(&other, "dave", "bob@@alpha").unwrap();
    let to = recipients(&other);
    assert!(
        to.contains(&alpha_bob) && !to.contains(&mallory_bob),
        "{to}"
    );

    // A spoke that pulls from zero gets the list before the events, so the attacker's old
    // rename to `alpha` finds the name taken. The attacker's own first name, from its
    // earlier signed event, is applied on that first pull; the list of the next pull puts
    // the node back under its id.
    let late = Node::new(dir.path(), "late");
    pair(&hub, &late);
    let first = pull(&hub, &late);
    assert_eq!(first.rejected.len(), 1, "{first:?}");
    assert!(first.rejected[0].reason.contains("alpha"), "{first:?}");
    assert_eq!(late.name_of(&alpha.id()).as_deref(), Some("alpha"));
    assert_eq!(late.name_of(&mallory.id()).as_deref(), Some("mallory"));
    pull(&hub, &late);
    assert_eq!(late.name_of(&alpha.id()).as_deref(), Some("alpha"));
    assert_eq!(late.name_of(&mallory.id()), Some(mallory.id()));
    late.agent("erin");
    mail(&late, "erin", "bob@@alpha").unwrap();
    let to = recipients(&late);
    assert!(
        to.contains(&alpha_bob) && !to.contains(&mallory_bob),
        "{to}"
    );
}

#[test]
fn a_lost_home_pairs_as_a_new_node() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let laptop = Node::new(dir.path(), "laptop");
    pair(&hub, &laptop);
    hub.store.revoke_peer("laptop").unwrap();

    // Under a new name it simply pairs.
    let renamed = Node::at(dir.path(), "home2", "laptop2");
    pair(&hub, &renamed);
    assert_eq!(hub.last_event("peer.paired")["again"], json!(false));

    // Under the old name it is refused while the revoked node holds the name.
    let replacement = Node::at(dir.path(), "home3", "laptop");
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let (invite, req) = replacement.store.prepare_join(&code).unwrap();
    let before = hub.rows();
    let err = hub.store.accept_pairing(&req).unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert_eq!(hub.rows(), before);
    assert!(!hub.invite_used(&fresh));

    hub.store.release_peer_name("laptop").unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    replacement.store.complete_join(&invite, &resp).unwrap();
    assert_eq!(hub.name_of(&replacement.id()).as_deref(), Some("laptop"));
    assert_eq!(hub.name_of(&laptop.id()), Some(laptop.id()));
    assert!(hub.peer(&laptop.id()).revoked_at.is_some());
}

#[test]
fn a_rebuilt_hub_under_the_same_name_can_be_joined() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::at(dir.path(), "hub-old", "hub");
    let new = Node::at(dir.path(), "hub-new", "hub");
    let early = Node::new(dir.path(), "early");
    let tidy = Node::new(dir.path(), "tidy");
    let unaware = Node::new(dir.path(), "unaware");
    pair(&old, &early);
    pair(&old, &tidy);

    // Joined while the old hub's row still holds the name: the new hub is recorded under
    // its id.
    early.store.revoke_peer(&old.id()).unwrap();
    pair(&new, &early);
    assert_eq!(early.name_of(&new.id()), Some(new.id()));
    assert_eq!(early.peer(&new.id()).display_name, new.id());
    assert_eq!(early.name_of(&old.id()).as_deref(), Some("hub"));
    // After the release, the next sync applies the name from the new hub's list.
    early.store.release_peer_name(&old.id()).unwrap();
    pull(&new, &early);
    assert_eq!(early.name_of(&new.id()).as_deref(), Some("hub"));
    assert_eq!(early.name_of(&old.id()), Some(old.id()));

    // With the release before the join, the name is there at once.
    tidy.store.revoke_peer("hub").unwrap();
    tidy.store.release_peer_name(&old.id()).unwrap();
    pair(&new, &tidy);
    assert_eq!(tidy.name_of(&new.id()).as_deref(), Some("hub"));

    // A spoke that never knew the old hub sees the name at once.
    pair(&new, &unaware);
    assert_eq!(unaware.name_of(&new.id()).as_deref(), Some("hub"));
}

#[test]
fn an_old_invite_is_refused_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let (_, fresh) = invite(&hub, &Perm::ALL);
    assert_eq!(fresh["v"], json!(2));
    for v in [1, 0, 3] {
        let mut old = fresh.clone();
        old["v"] = json!(v);
        let err = spoke.store.prepare_join(&encode(&old)).unwrap_err();
        assert_eq!(err.code(), "invalid");
        assert_eq!(
            err.to_string(),
            "this invite was made by an older build; make a new invite on an upgraded hub"
        );
    }
    assert!(!hub.invite_used(&fresh));
    assert!(spoke.store.peers().unwrap().is_empty());
}

/// S is a spoke of H and a hub to T. S revokes H and releases its name. T follows at its
/// next pull; nothing travels up to H.
#[test]
fn a_release_on_a_node_that_is_also_a_hub_reaches_its_spokes() {
    let dir = tempfile::tempdir().unwrap();
    let h = Node::new(dir.path(), "h");
    let s = Node::new(dir.path(), "s");
    let t = Node::new(dir.path(), "t");
    pair(&h, &s);
    pair(&s, &t);
    pull(&s, &t);
    assert_eq!(t.name_of(&h.id()).as_deref(), Some("h"));

    let h_before = h.rows();
    let s_before = s.rows();
    s.store.revoke_peer("h").unwrap();
    s.store.release_peer_name(&h.id()).unwrap();

    pull(&s, &t);
    assert_eq!(t.name_of(&h.id()), Some(h.id()));
    assert_eq!(t.name_of(&s.id()).as_deref(), Some("s"));

    assert_eq!(h.rows(), h_before, "a release travelled up to the hub");
    assert_eq!(h.name_of(&h.id()).as_deref(), Some("h"));
    // On S only H's two rows differ.
    let s_after = s.rows();
    assert_eq!(s_after.len(), s_before.len());
    for (before, after) in s_before.iter().zip(&s_after) {
        if before != after {
            assert!(
                after.starts_with(&format!("nodes node_id={}", h.id()))
                    || after.starts_with(&format!("peers node_id={}", h.id())),
                "{after}"
            );
        }
    }
}

#[test]
fn an_invite_whose_hub_id_is_a_known_name_is_refused_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    // The spoke knows another node whose name is the hub's id.
    let (_, key) = new_key();
    spoke
        .sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, ?2, 'peer', 0, ?3, ?4)",
            rusqlite::params![ULID_A, hub.id(), T0, key],
        )
        .unwrap();
    let (code, fresh) = invite(&hub, &Perm::ALL);
    let before = spoke.rows();
    let err = spoke.store.prepare_join(&code).unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert_eq!(
        err.to_string(),
        format!(
            "node {}'s id is already another node's name; rename that node first",
            hub.id()
        )
    );
    assert_eq!(spoke.rows(), before);
    assert!(!hub.invite_used(&fresh));
}

#[test]
fn a_second_hub_is_refused_with_the_revoke_command() {
    let dir = tempfile::tempdir().unwrap();
    let first = Node::new(dir.path(), "first");
    let second = Node::new(dir.path(), "second");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&first, &spoke);
    let (code, _) = invite(&second, &Perm::ALL);
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = second.store.accept_pairing(&req).unwrap();
    let before = spoke.rows();
    let err = spoke.store.complete_join(&invite, &resp).unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(
        err.to_string(),
        "this node already syncs with another hub; revoke it first: `another pair revoke <hub>`"
    );
    assert_eq!(spoke.rows(), before);
}

/// Each failure of the invite check answers the one text and hands the hub its own cause.
#[test]
fn a_refused_pairing_records_its_cause() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let beta = Node::new(dir.path(), "beta");

    let refused = |req: &PairRequest| -> PairRefusal {
        let before = hub.rows();
        let (result, cause) = hub.store.accept_pairing_with_cause(req);
        let err = result.unwrap_err();
        assert_eq!(err.code(), "forbidden");
        assert_eq!(err.to_string(), ONE_TEXT);
        assert_eq!(hub.rows(), before);
        cause.unwrap()
    };

    let (_, fresh) = invite(&hub, &Perm::ALL);
    let mut unknown = fresh.clone();
    unknown["invite_id"] = json!(ULID_A);
    let cause = refused(&signed_request(
        &unknown,
        &alpha.entry(),
        &alpha.secret(),
        T0,
    ));
    assert_eq!(cause, PairRefusal::UnknownInvite);
    assert_eq!(cause.detail(), "unknown invite");

    let (other_private, _) = new_key();
    let cause = refused(&signed_request(&fresh, &alpha.entry(), &other_private, T0));
    assert_eq!(cause, PairRefusal::BadSignature);
    assert_eq!(cause.detail(), "bad signature");

    hub.store
        .accept_pairing(&signed_request(&fresh, &alpha.entry(), &alpha.secret(), T0))
        .unwrap();
    let cause = refused(&signed_request(&fresh, &beta.entry(), &beta.secret(), T0));
    assert_eq!(
        cause,
        PairRefusal::UsedInvite {
            used_by: alpha.id()
        }
    );
    assert_eq!(cause.detail(), format!("used invite (by {})", alpha.id()));

    // A `used_by` an older build stored unchecked is not repeated.
    hub.sql()
        .execute("UPDATE invites SET used_by = 'x@y\nforged'", [])
        .unwrap();
    let cause = refused(&signed_request(&fresh, &beta.entry(), &beta.secret(), T0));
    assert_eq!(cause.detail(), "used invite (by )");

    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 60)
        .unwrap();
    let short = decode(&code);
    hub.clock.fetch_add(60_000, Ordering::SeqCst);
    let cause = refused(&signed_request(&short, &beta.entry(), &beta.secret(), T0));
    assert_eq!(cause, PairRefusal::ExpiredInvite);
    assert_eq!(cause.detail(), "expired invite");
}

#[test]
fn activity_lines_for_pairing_again_readmit_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair(&hub, &alpha);
    let lines = |hub: &Node| -> Vec<String> {
        hub.store
            .activity(ActivityQuery::default())
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.summary)
            .collect()
    };
    assert!(lines(&hub).contains(&"paired with node alpha (spoke)".to_string()));
    assert!(!lines(&hub).iter().any(|l| l.contains("paired again")));

    pair(&hub, &alpha);
    hub.store.revoke_peer("alpha").unwrap();
    hub.store.readmit_peer("alpha").unwrap();
    hub.store.revoke_peer("alpha").unwrap();
    hub.store.release_peer_name("alpha").unwrap();
    let lines = lines(&hub);
    for expected in [
        "paired again with node alpha (spoke)".to_string(),
        "re-admitted node alpha".to_string(),
        format!("released the name alpha of node {}", alpha.id()),
    ] {
        assert!(lines.contains(&expected), "{expected:?} not in {lines:#?}");
    }
}

#[test]
fn local_is_refused_by_node_name() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(dir.path(), "named");
    let err = node.store.set_node_name("local").unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert_eq!(node.entry().display_name, "named");
    let renames: i64 = node
        .sql()
        .query_row(
            "SELECT count(*) FROM events WHERE kind = 'node.renamed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(renames, 1);
}

/// A node that an older build let be named `local` keeps that name where it is recorded
/// and keeps syncing. It cannot pair again until it is renamed, and a node pulling from
/// zero records it under its id.
#[test]
fn a_node_already_named_local_keeps_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    // A home that was never named, paired as an older hub accepted it from a hand-made
    // request: the rows are written here because no build makes them any more.
    let home = dir.path().join("odd");
    let (clock, handle) = Clock::manual(T0);
    let odd = Node {
        store: Store::open_with_clock(&home, clock).unwrap(),
        home,
        clock: handle,
    };
    assert_eq!(odd.entry().display_name, "local");
    let perms = r#"["read_feed","send_mail","borrow_resource"]"#;
    hub.sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'local', 'peer', 0, ?2, ?3)",
            rusqlite::params![odd.id(), T0, odd.entry().public_key],
        )
        .unwrap();
    hub.sql()
        .execute(
            "INSERT INTO peers (node_id, role, perms, paired_at) VALUES (?1, 'spoke', ?2, ?3)",
            rusqlite::params![odd.id(), perms, T0],
        )
        .unwrap();
    odd.sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'hub', 'peer', 0, ?2, ?3)",
            rusqlite::params![hub.id(), T0, hub.entry().public_key],
        )
        .unwrap();
    odd.sql()
        .execute(
            "INSERT INTO peers (node_id, role, url, perms, paired_at)
             VALUES (?1, 'hub', 'https://hub.example', ?2, ?3)",
            rusqlite::params![hub.id(), perms, T0],
        )
        .unwrap();

    odd.agent("bob");
    assert!(push(&hub, &odd).rejected.is_empty());
    pull(&hub, &odd);
    assert!(signed_push_accepted(&hub, &odd));
    assert_eq!(hub.name_of(&odd.id()).as_deref(), Some("local"));

    // It cannot make a request, and a hand-made one is refused.
    let (code, fresh) = invite(&hub, &Perm::ALL);
    assert_eq!(odd.store.prepare_join(&code).unwrap_err().code(), "invalid");
    let before = hub.rows();
    let req = signed_request(&fresh, &odd.entry(), &odd.secret(), T0);
    assert_eq!(req.node.display_name, "local");
    assert_eq!(
        hub.store.accept_pairing(&req).unwrap_err().code(),
        "invalid"
    );
    assert_eq!(hub.rows(), before);

    // A node pulling from zero records it under its id.
    let late = Node::new(dir.path(), "late");
    pair(&hub, &late);
    pull(&hub, &late);
    assert_eq!(late.name_of(&odd.id()), Some(odd.id()));

    // Renamed, it pairs again.
    odd.store.set_node_name("odd").unwrap();
    let (_, req) = odd.store.prepare_join(&code).unwrap();
    hub.store.accept_pairing(&req).unwrap();
    assert_eq!(hub.name_of(&odd.id()).as_deref(), Some("odd"));
}

/// Two nodes whose names an older build let differ only in case both keep syncing and both
/// pair again under their unchanged names.
#[test]
fn existing_case_only_names_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let lower = Node::at(dir.path(), "lower", "alpha");
    let upper = Node::at(dir.path(), "upper", "alphb");
    pair(&hub, &lower);
    pair(&hub, &upper);
    assert!(push(&hub, &lower).rejected.is_empty());
    assert!(push(&hub, &upper).rejected.is_empty());
    for node in [&hub, &upper] {
        node.sql()
            .execute(
                "UPDATE nodes SET display_name = 'Alpha' WHERE node_id = ?1",
                [upper.id()],
            )
            .unwrap();
    }
    assert_eq!(upper.entry().display_name, "Alpha");

    for node in [&lower, &upper] {
        node.agent("bob");
        let (events, _) = node.store.outbound_events(0, 10_000).unwrap();
        let fresh: Vec<WireEvent> = events
            .into_iter()
            .filter(|e| e.kind == "agent.registered")
            .collect();
        let report = hub.store.ingest(&node.id(), &fresh, &[]).unwrap();
        assert_eq!(report.accepted, 1, "{report:?}");
        pull(&hub, node);
        assert!(signed_push_accepted(&hub, node));
    }

    for (node, name) in [(&lower, "alpha"), (&upper, "Alpha")] {
        pair_with(&hub, node, &[Perm::ReadFeed]);
        assert_eq!(hub.name_of(&node.id()).as_deref(), Some(name));
        assert_eq!(hub.peer(&node.id()).perms, [Perm::ReadFeed]);
        assert_eq!(hub.last_event("peer.paired")["again"], json!(true));
    }
}

/// A home whose name a rename would refuse (an older build took `ANOTHER_NODE_NAME`
/// unchecked) can neither make an invite nor join until it is renamed.
#[test]
fn a_node_with_an_invalid_name_cannot_invite_or_join() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let odd = Node::new(dir.path(), "odd");
    let (code, _) = invite(&hub, &Perm::ALL);
    odd.sql()
        .execute(
            "UPDATE nodes SET display_name = 'two words\u{1b}[2J' WHERE is_local = 1",
            [],
        )
        .unwrap();
    let expected = "this node's name is not valid (use 1-40 ASCII letters, digits, '-', '_' \
                    or '.'); rename it with `another node name <name>`";
    let err = odd
        .store
        .create_invite("https://odd.example", &Perm::ALL, 3600)
        .unwrap_err();
    assert_eq!(
        (err.code(), err.to_string().as_str()),
        ("invalid", expected)
    );
    let err = odd.store.prepare_join(&code).unwrap_err();
    assert_eq!(
        (err.code(), err.to_string().as_str()),
        ("invalid", expected)
    );

    odd.store.set_node_name("odd").unwrap();
    odd.store
        .create_invite("https://odd.example", &Perm::ALL, 3600)
        .unwrap();
    odd.store.prepare_join(&code).unwrap();
}

/// A spoke paired by an older build: the rows that build wrote, with no new field anywhere.
/// It pushes and pulls as before.
#[test]
fn already_paired_nodes_keep_syncing() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let perms = r#"["read_feed","send_mail","borrow_resource"]"#;
    hub.sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'spoke', 'peer', 0, ?2, ?3)",
            rusqlite::params![spoke.id(), T0, spoke.entry().public_key],
        )
        .unwrap();
    hub.sql()
        .execute(
            "INSERT INTO peers (node_id, role, perms, paired_at) VALUES (?1, 'spoke', ?2, ?3)",
            rusqlite::params![spoke.id(), perms, T0],
        )
        .unwrap();
    spoke
        .sql()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'hub', 'peer', 0, ?2, ?3)",
            rusqlite::params![hub.id(), T0, hub.entry().public_key],
        )
        .unwrap();
    spoke
        .sql()
        .execute(
            "INSERT INTO peers (node_id, role, url, perms, paired_at)
             VALUES (?1, 'hub', 'https://hub.example', ?2, ?3)",
            rusqlite::params![hub.id(), perms, T0],
        )
        .unwrap();

    assert!(signed_push_accepted(&hub, &spoke));
    spoke.agent("bob");
    hub.agent("carol");
    let pushed = push(&hub, &spoke);
    assert!(
        pushed.rejected.is_empty() && pushed.accepted > 0,
        "{pushed:?}"
    );
    let pulled = pull(&hub, &spoke);
    assert!(
        pulled.rejected.is_empty() && pulled.accepted > 0,
        "{pulled:?}"
    );
    mail(&spoke, "bob", "carol@@hub").unwrap();
    assert!(push(&hub, &spoke).rejected.is_empty());
    let inbox = hub.store.inbox("carol", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
}
