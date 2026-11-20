//! FINDINGS-222-w9Z: the pairing secret is a credential (the hub stores only its hash, and
//! it lets a node join until the invite expires), so the `Debug` of `Invite` and
//! `PairRequest` never prints it, in the one-line form or the pretty one. `Serialize` keeps
//! it: the invite code and the pairing request are built from it.

use another_core::*;

const SECRET: &str = "s3cr3t-pairing-value";

fn node() -> NodeEntry {
    NodeEntry {
        node_id: "n".into(),
        display_name: "hub".into(),
        public_key: "k".into(),
    }
}

/// Both `Debug` forms of `value`, which must name the type and never hold `secret`.
fn assert_hidden(value: &impl std::fmt::Debug, what: &str, secret: &str) {
    for debug in [format!("{value:?}"), format!("{value:#?}")] {
        assert!(debug.contains(what), "not a {what}: {debug}");
        assert!(debug.contains("invite_id"), "{what} lost its other fields");
        assert!(!debug.contains(secret), "{what} prints its secret");
    }
}

#[test]
fn an_invite_hides_its_secret() {
    let i = Invite {
        v: 1,
        hub_url: "https://hub.example".into(),
        hub: node(),
        invite_id: "i".into(),
        secret: SECRET.into(),
        perms: Vec::new(),
        expires_at: 0,
    };
    assert_hidden(&i, "Invite", SECRET);
    assert!(
        serde_json::to_string(&i).unwrap().contains(SECRET),
        "the invite code must still carry the secret"
    );
}

#[test]
fn a_pair_request_hides_its_secret() {
    let r = PairRequest {
        invite_id: "i".into(),
        secret: SECRET.into(),
        node: node(),
    };
    assert_hidden(&r, "PairRequest", SECRET);
    assert!(
        serde_json::to_string(&r).unwrap().contains(SECRET),
        "the pairing request must still carry the secret"
    );
}

/// The same for the values a real pairing makes: the invite a spoke decodes and the request
/// it builds, with the secret the hub generated.
#[test]
fn a_real_invite_and_pair_request_hide_the_generated_secret() {
    let hub = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    hub.set_node_name("hub").unwrap();
    let spoke = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    spoke.set_node_name("spoke").unwrap();
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    assert_eq!(invite.secret.len(), 32, "24 random bytes as base64");
    assert_hidden(&invite, "Invite", &invite.secret);
    assert_hidden(&req, "PairRequest", &req.secret);
    assert_hidden(&(invite.clone(), req.clone()), "Invite", &invite.secret);
}
