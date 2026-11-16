//! FINDINGS-222-w9Z: the pairing secret is a credential too (it is stored only as a hash and
//! lets a node join the hub until the invite expires), but `Invite` and `PairRequest`
//! derive `Debug` and print it.

use another_core::*;

const SECRET: &str = "s3cr3t-pairing-value";

fn node() -> NodeEntry {
    NodeEntry {
        node_id: "n".into(),
        display_name: "hub".into(),
        public_key: "k".into(),
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
    let debug = format!("{i:?}");
    assert!(!debug.contains(SECRET), "Invite prints its secret: {debug}");
}

#[test]
fn a_pair_request_hides_its_secret() {
    let r = PairRequest {
        invite_id: "i".into(),
        secret: SECRET.into(),
        node: node(),
    };
    let debug = format!("{r:?}");
    assert!(
        !debug.contains(SECRET),
        "PairRequest prints its secret: {debug}"
    );
}
