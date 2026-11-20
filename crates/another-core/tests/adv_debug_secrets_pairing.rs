//! FINDINGS-222-w9Z: the pairing secret is a credential (the hub stores only its hash, and
//! it lets a node join until the invite expires), so the `Debug` of `Invite` never prints
//! it, in the one-line form or the pretty one. `Serialize` keeps it: the invite code is built
//! from it. Since Q27 a pairing request holds no secret at all, in any form.

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
        v: 2,
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

/// A pairing request has no place for a secret: one sent by an older joiner is an unknown
/// field, so it is neither kept nor printed nor sent on.
#[test]
fn a_pair_request_does_not_keep_a_secret_it_is_sent() {
    let r: PairRequest = serde_json::from_value(serde_json::json!({
        "invite_id": "i",
        "secret": SECRET,
        "node": node(),
    }))
    .unwrap();
    assert_hidden(&r, "PairRequest", SECRET);
    assert!(
        !serde_json::to_string(&r).unwrap().contains(SECRET),
        "a pairing request must not carry the secret"
    );
}

/// The same for the values a real pairing makes: the invite a spoke decodes hides the
/// secret the hub generated, and the request it builds holds neither the secret nor its
/// hash, as JSON or as `Debug`.
#[test]
fn a_real_invite_hides_the_generated_secret_and_the_request_has_none() {
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
    assert_hidden(&req, "PairRequest", &invite.secret);
    assert_hidden(&(invite.clone(), req.clone()), "Invite", &invite.secret);
    let sent = serde_json::to_string(&req).unwrap();
    let hash = crypto::sha256_hex(invite.secret.as_bytes());
    assert!(!sent.contains(&invite.secret), "the request has the secret");
    assert!(!sent.contains(&hash), "the request has the secret's hash");
    assert!(!format!("{req:?}").contains(&hash));
}
