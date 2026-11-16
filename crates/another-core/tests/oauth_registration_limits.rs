//! The size limits on an OAuth client: a name of 1 to 120 characters (counted as
//! characters, not bytes) and 1 to 10 redirect URIs. Dynamic registration
//! (`register_oauth_client`) and a client metadata document (`validate_cimd`) apply the same
//! limits. A refusal registers nothing.

use another_core::*;
use serde_json::json;

const REDIRECT: &str = "https://app.example/callback";
const DOC: &str = "https://app.example/client.json";

fn store() -> Store {
    Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap()
}

fn register(s: &Store, name: &str, uris: usize) -> Result<RegisteredClient, OAuthFail> {
    s.register_oauth_client(ClientRegistration {
        client_name: name.into(),
        redirect_uris: (0..uris).map(|i| format!("{REDIRECT}/{i}")).collect(),
        application_type: Some("web".into()),
        remote: None,
    })
}

fn cimd(name: &str, uris: usize) -> Result<ClientProfile, String> {
    let doc = json!({
        "client_id": DOC,
        "client_name": name,
        "redirect_uris": (0..uris).map(|i| format!("{REDIRECT}/{i}")).collect::<Vec<_>>(),
        "token_endpoint_auth_method": "none",
    });
    validate_cimd(DOC, &doc)
}

fn invalid_request(r: Result<RegisteredClient, OAuthFail>) -> String {
    match r {
        Err(OAuthFail::InvalidRequest(m)) => m,
        other => panic!("expected invalid_request, got {other:?}"),
    }
}

#[test]
fn a_client_name_is_one_to_120_characters() {
    let s = store();
    // 120 two-byte characters: 240 bytes, still within the limit, which counts characters.
    let at_limit = "é".repeat(120);
    assert_eq!(register(&s, &at_limit, 1).unwrap().client_name, at_limit);
    assert_eq!(cimd(&at_limit, 1).unwrap().client_name, at_limit);

    for bad in ["", "   ", &"é".repeat(121)] {
        let s = store();
        let m = invalid_request(register(&s, bad, 1));
        assert!(m.contains("at most 120 characters"), "{bad:?}: {m}");
        assert!(
            cimd(bad, 1).is_err(),
            "{bad:?} passed the metadata document check"
        );
    }
}

#[test]
fn a_client_has_one_to_ten_redirect_uris() {
    let s = store();
    assert_eq!(register(&s, "App", 10).unwrap().redirect_uris.len(), 10);
    assert_eq!(cimd("App", 10).unwrap().redirect_uris.len(), 10);

    for count in [0, 11] {
        let s = store();
        let m = invalid_request(register(&s, "App", count));
        assert!(m.contains("1 to 10 URIs"), "{count}: {m}");
        let m = cimd("App", count).unwrap_err();
        assert!(m.contains("1 to 10 URIs"), "{count}: {m}");
    }
}
