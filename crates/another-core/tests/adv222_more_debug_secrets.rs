//! adv222: credential carriers #222 does not reach. `PendingAuthorization::redirect_token`
//! is "presented once in the browser URL; only its hash is stored" and is `serde(skip)`, yet
//! `Debug` prints it; `Invite` and `PairRequest` carry the single-use pairing secret.

use another_core::*;

const SECRET: &str = "s3cr3t-credential-value";

fn node() -> NodeEntry {
    NodeEntry {
        node_id: "01NODE".into(),
        display_name: "hub".into(),
        public_key: "pk".into(),
    }
}

#[test]
fn adv222_a_pending_authorization_hides_its_redirect_token() {
    let p = PendingAuthorization {
        request_id: "r".into(),
        user_code: "ABCD-EFGH".into(),
        client_name: "c".into(),
        redirect_host: "127.0.0.1".into(),
        loopback_redirect: true,
        expires_at: "t".into(),
        redirect_token: SECRET.into(),
    };
    let d = format!("{p:?}");
    assert!(
        !d.contains(SECRET),
        "PendingAuthorization prints redirect_token: {d}"
    );
}

#[test]
fn adv222_an_invite_and_a_pair_request_hide_the_pairing_secret() {
    let invite = Invite {
        v: 1,
        hub_url: "https://hub.example".into(),
        hub: node(),
        invite_id: "i".into(),
        secret: SECRET.into(),
        perms: vec![Perm::ReadFeed],
        expires_at: 0,
    };
    let d = format!("{invite:?}");
    assert!(!d.contains(SECRET), "Invite prints its secret: {d}");
    let req = PairRequest {
        invite_id: "i".into(),
        secret: SECRET.into(),
        node: node(),
    };
    let d = format!("{req:?}");
    assert!(!d.contains(SECRET), "PairRequest prints its secret: {d}");
}
