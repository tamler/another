//! The node list that comes with `ingest(from, events, nodes)`.
//!
//! It used to be applied as given: any node whose public key matched was renamed, and public
//! keys are public. A spoke could rename another node on the hub, or give itself a name
//! another node has, which `node.renamed` events refuse (FINDINGS-141). The first three tests
//! are the finder's (FINDINGS-main-peer-node-entry-w9Y); the rest pin the hub's list, which a
//! spoke still needs to learn the other nodes and their keys.

use std::path::Path;

use another_core::*;
use rusqlite::{Connection, OptionalExtension};

const T0: i64 = 1_800_000_000_000;

fn node(dir: &Path, name: &str) -> Store {
    let (clock, _) = Clock::manual(T0);
    let store = Store::open_with_clock(&dir.join(name), clock)
        .unwrap()
        .unguarded_repair();
    store.set_node_name(name).unwrap();
    store
}

fn pair(hub: &Store, spoke: &Store) {
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
}

fn entry(of: &Store, display_name: &str) -> NodeEntry {
    let info = of.node().unwrap();
    NodeEntry {
        node_id: info.node_id,
        display_name: display_name.into(),
        public_key: info.public_key,
    }
}

/// The name the hub has for `of`.
fn name_on(hub: &Store, of: &Store) -> String {
    let id = of.node_id().to_string();
    hub.peers()
        .unwrap()
        .into_iter()
        .find(|p| p.node_id == id)
        .map(|p| p.display_name)
        .unwrap_or_else(|| panic!("hub does not know {id}"))
}

#[test]
fn adv_a_spoke_cannot_rename_another_node_through_its_node_list() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let mallory = node(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);
    assert_eq!(name_on(&hub, &alpha), "alpha");

    // mallory sends alpha's real id and public key with a new name.
    let res = hub.ingest(
        mallory.node_id(),
        &[],
        &[entry(&alpha, "renamed-by-mallory")],
    );
    eprintln!("ADV rename other: {res:?} -> {}", name_on(&hub, &alpha));
    assert_eq!(
        name_on(&hub, &alpha),
        "alpha",
        "a spoke renamed another node on the hub"
    );
}

fn agent(store: &Store, name: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
}

/// Push everything `spoke` authored to `hub`, as `another sync` does.
fn push(hub: &Store, spoke: &Store) {
    let (events, _) = spoke.outbound_events(0, 10_000).unwrap();
    hub.ingest(spoke.node_id(), &events, &[]).unwrap();
}

#[test]
fn adv_mail_to_an_agent_on_alpha_cannot_be_redirected_to_mallory() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let mallory = node(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);
    agent(&alpha, "bob");
    agent(&mallory, "bob");
    push(&hub, &alpha);
    push(&hub, &mallory);
    agent(&hub, "carol");

    // mallory moves alpha out of the way, then answers to "alpha" itself.
    let _ = hub.ingest(mallory.node_id(), &[], &[entry(&alpha, "gone")]);
    let _ = hub.ingest(mallory.node_id(), &[], &[entry(&mallory, "alpha")]);

    let sent = hub.send_message(SendMessage {
        from: "carol".into(),
        to: vec!["bob@@alpha".into()],
        subject: "for bob on alpha".into(),
        body: "the plan".into(),
        ..Default::default()
    });
    let landed_on = sent.as_ref().ok().and_then(|m| m.to.first().cloned());
    // The node that owns each delivered agent, read from the hub's own database.
    let owners: Vec<String> = match &sent {
        Ok(m) => {
            let db = rusqlite::Connection::open(dir.path().join("hub").join(DB_FILE)).unwrap();
            let mut stmt = db
                .prepare(
                    "SELECT a.node_id FROM deliveries d JOIN agents a ON a.agent_id = d.agent_id
                     WHERE d.message_id = ?1",
                )
                .unwrap();
            stmt.query_map([&m.message_id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }
        Err(_) => Vec::new(),
    };
    eprintln!("ADV redirect: to={landed_on:?} owners={owners:?}");
    assert!(
        !owners.iter().any(|n| n == mallory.node_id()),
        "mail addressed to bob@@alpha was delivered to mallory's bob ({landed_on:?})"
    );
}

#[test]
fn adv_a_spoke_cannot_take_a_taken_or_invalid_name_through_its_node_list() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let mallory = node(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);

    let long = "x".repeat(100_000);
    let mut kept = Vec::new();
    for name in ["alpha", "hub", "a@b", "two words", long.as_str()] {
        let res = hub.ingest(mallory.node_id(), &[], &[entry(&mallory, name)]);
        let now = name_on(&hub, &mallory);
        eprintln!("ADV self {:.20}: ok={} -> {:.20}", name, res.is_ok(), now);
        if now == name {
            kept.push(format!("{name:.20} ({} bytes)", name.len()));
        }
    }
    assert!(
        kept.is_empty(),
        "the hub took node names from mallory's node list that a rename or a local name refuses: {kept:?}"
    );
}

/// The name `on` (opened at `dir/on`) has recorded for node `of`, if it knows it.
fn known_name(dir: &Path, on: &str, of: &Store) -> Option<String> {
    Connection::open(dir.join(on).join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT display_name FROM nodes WHERE node_id = ?1",
            [of.node_id()],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

#[test]
fn a_spoke_may_still_rename_itself_through_its_node_list() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let mallory = node(dir.path(), "mallory");
    pair(&hub, &mallory);

    hub.ingest(mallory.node_id(), &[], &[entry(&mallory, "mallory-2")])
        .unwrap();
    assert_eq!(name_on(&hub, &mallory), "mallory-2");
}

#[test]
fn a_spoke_learns_other_nodes_and_renames_from_its_hubs_list() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let beta = node(dir.path(), "beta");
    pair(&hub, &alpha);
    pair(&hub, &beta);

    // alpha learns beta, key and name, from the hub's list, as a pull delivers it.
    alpha
        .ingest(hub.node_id(), &[], &[entry(&beta, "beta")])
        .unwrap();
    assert_eq!(
        known_name(dir.path(), "alpha", &beta).as_deref(),
        Some("beta")
    );

    // The hub is the directory for its spokes: a free, valid new name it lists is applied.
    alpha
        .ingest(hub.node_id(), &[], &[entry(&beta, "beta-2")])
        .unwrap();
    assert_eq!(
        known_name(dir.path(), "alpha", &beta).as_deref(),
        Some("beta-2")
    );
}

#[test]
fn a_name_from_the_hubs_list_that_is_invalid_or_taken_is_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let beta = node(dir.path(), "beta");
    let gamma = node(dir.path(), "gamma");
    pair(&hub, &alpha);
    pair(&hub, &beta);
    alpha
        .ingest(hub.node_id(), &[], &[entry(&beta, "beta")])
        .unwrap();

    // A known node keeps its name: not someone else's, not one a local rename refuses, and
    // not `local`, the placeholder every unnamed node has.
    for name in ["hub", "alpha", "two words", "a@b", "local"] {
        alpha
            .ingest(hub.node_id(), &[], &[entry(&beta, name)])
            .unwrap();
        assert_eq!(
            known_name(dir.path(), "alpha", &beta).as_deref(),
            Some("beta"),
            "{name:?} was applied"
        );
    }

    // An unknown node is still recorded, so its key is known and its signed events verify,
    // but under its id, which names no one else.
    alpha
        .ingest(hub.node_id(), &[], &[entry(&gamma, "beta")])
        .unwrap();
    assert_eq!(
        known_name(dir.path(), "alpha", &gamma).as_deref(),
        Some(gamma.node_id())
    );
}

/// Nodes are addressed by display name or by id, so another node's id is a taken name on
/// every path that names a node: a peer's node list, a signed rename, a local rename and
/// pairing. Otherwise `bob@@<alpha's id>`, the form that resolves an ambiguity, is ambiguous.
/// (FINDINGS-248-w9Y.)
#[test]
fn another_nodes_id_is_a_taken_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let mallory = node(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);
    let alpha_id = alpha.node_id().to_string();

    // A spoke's own entry in its node list.
    hub.ingest(mallory.node_id(), &[], &[entry(&mallory, &alpha_id)])
        .unwrap();
    assert_eq!(name_on(&hub, &mallory), "mallory", "node list");

    // A signed node.renamed. mallory does not know alpha, so its own rename succeeds.
    mallory.set_node_name(&alpha_id).unwrap();
    let (events, _) = mallory.outbound_events(0, 10_000).unwrap();
    let report = hub.ingest(mallory.node_id(), &events, &[]).unwrap();
    assert!(
        report
            .rejected
            .iter()
            .any(|r| r.reason.contains("another node already has")),
        "node.renamed: {report:?}"
    );
    assert_eq!(name_on(&hub, &mallory), "mallory", "node.renamed");

    // A local rename on the hub.
    let err = hub.set_node_name(&alpha_id).unwrap_err();
    assert_eq!(err.code(), "conflict", "set_node_name: {err}");

    // Pairing a node that calls itself by alpha's id.
    let joiner = node(dir.path(), "joiner");
    joiner.set_node_name(&alpha_id).unwrap();
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (_, req) = joiner.prepare_join(&code).unwrap();
    let err = hub.accept_pairing(&req).unwrap_err();
    assert_eq!(err.code(), "conflict", "pairing: {err}");
}

/// The inverse of `another_nodes_id_is_a_taken_name`: a node being recorded for the first
/// time must not have an id that is already another node's name. The next two tests are the
/// cross-family reviewer's probes (codex@another-ack-rename, REVIEW-62-R3), adopted as
/// written apart from this file's helpers.
#[test]
fn reviewer_alias_pairing_must_not_make_node_id_address_ambiguous() {
    for colliding in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hub = node(dir.path(), "hub");
        let peer = node(dir.path(), "peer");
        if colliding {
            // The peer is not known yet: a real generated id and a valid local name.
            hub.set_node_name(peer.node_id()).unwrap();
        }
        let code = hub
            .create_invite("https://hub.example", &Perm::ALL, 3600)
            .unwrap();
        let (invite, req) = peer.prepare_join(&code).unwrap();
        let response = match hub.accept_pairing(&req) {
            Ok(response) => response,
            Err(error) if colliding => {
                assert_eq!(error.code(), "conflict", "{error}");
                assert!(hub.peers().unwrap().is_empty());
                continue;
            }
            Err(error) => panic!("ordinary pairing failed: {error}"),
        };
        peer.complete_join(&invite, &response).unwrap();
        agent(&hub, "carol");
        agent(&hub, "bob");
        agent(&peer, "bob");
        let (events, _) = peer.outbound_events(0, 10_000).unwrap();
        let report = hub.ingest(peer.node_id(), &events, &[]).unwrap();
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let result = hub.send_message(SendMessage {
            from: "carol".into(),
            to: vec![format!("bob@@{}", peer.node_id())],
            subject: "address by actual generated node id".into(),
            body: "test".into(),
            ..Default::default()
        });
        assert!(
            result.is_ok(),
            "colliding={colliding}: generated node id cannot disambiguate: {:?}",
            result.err()
        );
    }
}

#[test]
fn reviewer_alias_hub_list_fallback_must_not_duplicate_a_known_name() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let alpha = node(dir.path(), "alpha");
    let beta = node(dir.path(), "beta");
    let gamma = node(dir.path(), "gamma");
    alpha.set_node_name(beta.node_id()).unwrap();
    pair(&hub, &alpha);

    // Ordinary unusable-name fallback still learns a third node's genuine key and id.
    alpha
        .ingest(hub.node_id(), &[], &[entry(&gamma, "two words")])
        .unwrap();
    assert_eq!(
        known_name(dir.path(), "alpha", &gamma).as_deref(),
        Some(gamma.node_id())
    );

    // beta's id is alpha's own name: the entry is skipped, and the sync still succeeds.
    let result = alpha.ingest(hub.node_id(), &[], &[entry(&beta, "two words")]);
    assert!(result.is_ok(), "{result:?}");
    let aliases: i64 = Connection::open(dir.path().join("alpha").join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM nodes WHERE display_name = ?1 OR node_id = ?1",
            [beta.node_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        aliases <= 1,
        "fallback created {aliases} node matches for an existing name"
    );
}

/// The same rule where a spoke first records its hub: a spoke already named with the hub's
/// id cannot complete the join, and nothing is recorded.
#[test]
fn a_spoke_named_with_its_hubs_id_cannot_complete_the_join() {
    let dir = tempfile::tempdir().unwrap();
    let hub = node(dir.path(), "hub");
    let spoke = node(dir.path(), "spoke");
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let response = hub.accept_pairing(&req).unwrap();
    spoke.set_node_name(hub.node_id()).unwrap();

    let err = spoke.complete_join(&invite, &response).unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert!(spoke.peers().unwrap().is_empty());
    assert_eq!(known_name(dir.path(), "spoke", &hub), None);
}
