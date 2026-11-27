//! Q29: the hub signs its answers to a push and a pull, and its invites. These are the
//! rules that need no HTTP: what the answer signature covers, which invites a hub makes,
//! and what a join records. The attacks over real HTTP are in the `another` crate's
//! `federation_relay.rs` and `federation_signed.rs`.

use std::path::Path;

use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

/// A well-formed node id that no test node has.
const OTHER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let store = Store::open(&home).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
        }
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    /// A signature made with this node's private key, as stored. The key is only ever
    /// read from a test's temporary home.
    fn sign(&self, message: &[u8]) -> String {
        let key: String = self
            .sql()
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        crypto::sign(&crypto::decode_secret(&key).unwrap(), message)
    }

    /// The payload of this node's newest `peer.paired` record.
    fn last_paired(&self) -> Value {
        let payload: String = self
            .sql()
            .query_row(
                "SELECT payload FROM events WHERE kind = 'peer.paired' ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }
}

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

/// One answer as a hub signs it and a spoke checks it.
struct Exchange {
    nonce: String,
    request_signature: String,
    body: Vec<u8>,
}

fn exchange(spoke: &Node) -> Exchange {
    Exchange {
        nonce: crypto::request_nonce().unwrap(),
        request_signature: spoke.store.sign_request(
            "GET",
            "/fed/pull?after=0&limit=500",
            1_800_000_000_000,
            b"",
        ),
        body: br#"{"events":[],"cursor":7,"more":false,"nodes":[],"reset":false}"#.to_vec(),
    }
}

fn refused(result: Result<()>) {
    match result {
        Err(Error::Forbidden(text)) => {
            assert_eq!(text, "answer signature is not the hub's for this request")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The group order of Ed25519, little endian. Adding it to the `s` half of a signature
/// gives the same value modulo the order in a form that is not canonical.
const GROUP_ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn non_canonical(signature: &str) -> String {
    let mut bytes = crypto::unb64(signature).unwrap();
    let mut carry = 0u16;
    for (i, add) in GROUP_ORDER.iter().enumerate() {
        let sum = bytes[32 + i] as u16 + *add as u16 + carry;
        bytes[32 + i] = sum as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0);
    crypto::b64(&bytes)
}

#[test]
fn every_nonce_is_32_lowercase_hex_characters_and_new() {
    let a = crypto::request_nonce().unwrap();
    let b = crypto::request_nonce().unwrap();
    for nonce in [&a, &b] {
        assert_eq!(nonce.len(), 32);
        assert!(nonce
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    }
    assert_ne!(a, b);
}

#[test]
fn verify_hub_answer_refuses_each_changed_field() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let x = exchange(&spoke);
    let signature = hub
        .store
        .sign_answer(&spoke.id(), &x.nonce, &x.request_signature, &x.body);
    let check = |hub_id: &str, nonce: &str, request_signature: &str, body: &[u8], sig: &str| {
        spoke
            .store
            .verify_hub_answer(hub_id, nonce, request_signature, body, sig)
    };

    check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &signature,
    )
    .unwrap();

    // The hub id: a signature over another hub's id, and a check under an id with no key.
    let other_hub = hub.sign(&crypto::answer_message(
        OTHER_ID,
        &spoke.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
    ));
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &other_hub,
    ));
    refused(check(
        OTHER_ID,
        &x.nonce,
        &x.request_signature,
        &x.body,
        &signature,
    ));
    // The spoke id: the hub's signature for another spoke.
    let for_another = hub
        .store
        .sign_answer(OTHER_ID, &x.nonce, &x.request_signature, &x.body);
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &for_another,
    ));
    // The nonce.
    let other_nonce = crypto::request_nonce().unwrap();
    refused(check(
        &hub.id(),
        &other_nonce,
        &x.request_signature,
        &x.body,
        &signature,
    ));
    // The request signature: the same request signed a millisecond later.
    let later =
        spoke
            .store
            .sign_request("GET", "/fed/pull?after=0&limit=500", 1_800_000_000_001, b"");
    refused(check(&hub.id(), &x.nonce, &later, &x.body, &signature));
    // One body byte, and a body cut short.
    let mut changed = x.body.clone();
    let at = changed.iter().position(|b| *b == b'7').unwrap();
    changed[at] = b'8';
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &changed,
        &signature,
    ));
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body[..x.body.len() - 1],
        &signature,
    ));
    // A signature that is not in its canonical form, and no signature at all.
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &non_canonical(&signature),
    ));
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        "",
    ));

    // A weak key pinned for the hub (a home an older build left): the signature that
    // verifies for every message under such a key is refused.
    let mut weak = [0u8; 32];
    weak[0] = 1;
    let mut degenerate = [0u8; 64];
    degenerate[0] = 1;
    spoke
        .sql()
        .execute(
            "UPDATE nodes SET public_key = ?1 WHERE node_id = ?2",
            [crypto::b64(&weak), hub.id()],
        )
        .unwrap();
    let message = crypto::answer_message(
        &hub.id(),
        &spoke.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
    );
    assert!(
        crypto::verify(&crypto::b64(&weak), &message, &crypto::b64(&degenerate)),
        "the non-strict check accepts it, which is the weakness"
    );
    refused(check(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &crypto::b64(&degenerate),
    ));
}

#[test]
fn an_answer_signed_by_another_paired_node_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let other = Node::new(dir.path(), "other");
    pair(&hub, &spoke);
    pair(&hub, &other);
    // The spoke learns the other node's key from the hub's list.
    let page = hub.store.pull_for(&spoke.id(), 0, 500).unwrap();
    spoke
        .store
        .ingest(&hub.id(), &page.events, &page.nodes)
        .unwrap();
    let known: Option<String> = spoke
        .sql()
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [other.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(known, Some(other.store.node_entry().unwrap().public_key));

    let x = exchange(&spoke);
    // The exact bytes the hub would sign, signed with the other node's key.
    let signature = other.sign(&crypto::answer_message(
        &hub.id(),
        &spoke.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
    ));
    refused(spoke.store.verify_hub_answer(
        &hub.id(),
        &x.nonce,
        &x.request_signature,
        &x.body,
        &signature,
    ));
}

#[test]
fn create_invite_refuses_an_address_the_joiner_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    for url in [
        "ftp://attacker.example/x",
        "file:///etc/hosts",
        "http://",
        "https:///path",
        "hub.example",
        "//hub.example",
        "http://two words.example",
        "javascript:alert(1)",
        "",
        "http://hub.example/a\nb",
        "http://hub.example/a b",
        "http://hub.example\r",
        "http://hub.example/\u{1b}[2J",
    ] {
        match hub.store.create_invite(url, &Perm::ALL, 3600) {
            Err(Error::Invalid(text)) => assert_eq!(
                text, "hub url must be an http:// or https:// URL with a host",
                "{url:?}"
            ),
            other => panic!("{url:?}: {other:?}"),
        }
        assert!(!hub_url_is_http(url) || url.contains([' ', '\n', '\r', '\u{1b}']));
    }
    let made: i64 = hub
        .sql()
        .query_row("SELECT count(*) FROM invites", [], |r| r.get(0))
        .unwrap();
    assert_eq!(made, 0, "a refused address makes no invite");
    for url in [
        "http://127.0.0.1:7466",
        "https://hub.example",
        "https://hub.example/",
        "http://[::1]:7466/base",
    ] {
        hub.store.create_invite(url, &Perm::ALL, 3600).unwrap();
        assert!(hub_url_is_http(url), "{url:?}");
    }
}

#[test]
fn an_invite_is_signed_over_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let code = hub
        .store
        .create_invite(
            "https://hub.example/",
            &[Perm::SendMail, Perm::ReadFeed],
            3600,
        )
        .unwrap();
    let raw = code.strip_prefix("another-invite:").unwrap();
    let invite: Value = serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap();
    let text = |v: &Value| v.as_str().unwrap().to_string();
    assert_eq!(invite["v"], 2, "the format number is unchanged");
    // The signed bytes, built by hand from the documented layout.
    let signed = [
        "another-invite-v1".to_string(),
        "2".to_string(),
        text(&invite["hub"]["node_id"]),
        text(&invite["hub"]["public_key"]),
        "hub".to_string(),
        "https://hub.example".to_string(),
        text(&invite["invite_id"]),
        crypto::sha256_hex(text(&invite["secret"]).as_bytes()),
        "read_feed,send_mail".to_string(),
        invite["expires_at"].as_i64().unwrap().to_string(),
    ]
    .join("\n");
    assert!(crypto::verify_strict(
        &hub.store.node_entry().unwrap().public_key,
        signed.as_bytes(),
        &text(&invite["sig"]),
    ));
}

#[test]
fn a_join_records_that_the_invite_was_signed() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let old = Node::new(dir.path(), "old");

    pair(&hub, &spoke);
    assert_eq!(spoke.last_paired()["invite_signed"], true);
    assert_eq!(
        spoke.store.hub_joined_before_signed_invites().unwrap(),
        None
    );
    assert_eq!(hub.store.hub_joined_before_signed_invites().unwrap(), None);

    // A join as a build before signed invites made it, on an invite of such a hub.
    let code = hub
        .store
        .create_invite_before_signed_invites("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = old.store.prepare_join_before_signed_invites(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    old.store.complete_join(&invite, &resp).unwrap();
    assert!(old.last_paired().get("invite_signed").is_none());
    assert_eq!(
        old.store.hub_joined_before_signed_invites().unwrap(),
        Some(hub.id())
    );
    // The same join on a signed invite: that build did not know the field, so it kept and
    // recorded nothing of it.
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = old.store.prepare_join_before_signed_invites(&code).unwrap();
    assert!(invite.sig.is_empty());
    let resp = hub.store.accept_pairing(&req).unwrap();
    old.store.complete_join(&invite, &resp).unwrap();
    assert!(old.last_paired().get("invite_signed").is_none());

    // Joining again on this build with a fresh invite clears it.
    pair(&hub, &old);
    assert_eq!(old.last_paired()["invite_signed"], true);
    assert_eq!(old.store.hub_joined_before_signed_invites().unwrap(), None);
}

#[test]
fn a_listed_key_that_differs_from_the_pinned_one_is_a_conflict_only_from_a_hub() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let other = Node::new(dir.path(), "other");
    pair(&hub, &spoke);
    pair(&hub, &other);
    let page = hub.store.pull_for(&spoke.id(), 0, 500).unwrap();
    spoke
        .store
        .ingest(&hub.id(), &page.events, &page.nodes)
        .unwrap();
    assert_eq!(spoke.store.listed_key_conflict(&page.nodes).unwrap(), None);

    // The spoke's home holds another key for that node than the hub lists.
    let real = other.store.node_entry().unwrap().public_key;
    let wrong = crypto::public_of(&crypto::generate_key().unwrap());
    spoke
        .sql()
        .execute(
            "UPDATE nodes SET public_key = ?1 WHERE node_id = ?2",
            [&wrong, &other.id()],
        )
        .unwrap();
    let conflict = spoke
        .store
        .listed_key_conflict(&page.nodes)
        .unwrap()
        .unwrap();
    assert_eq!(
        conflict,
        KeyConflict {
            node_id: other.id(),
            listed_fingerprint: crypto::fingerprint(&real),
            pinned_fingerprint: crypto::fingerprint(&wrong),
        }
    );
    match spoke.store.ingest(&hub.id(), &page.events, &page.nodes) {
        Err(Error::Conflict(text)) => {
            assert_eq!(text, conflict.refusal().to_string());
            assert!(text.contains(&crypto::fingerprint(&real)), "{text}");
            assert!(text.contains(&crypto::fingerprint(&wrong)), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        conflict.detail(),
        format!(
            "node list names {} with key {}; pinned {}",
            other.id(),
            crypto::fingerprint(&real),
            crypto::fingerprint(&wrong)
        )
    );
}
