//! A pending OAuth authorization that has outlived its ten minutes cannot be approved: the
//! operator may be reading an old code, and the client that asked has long given up.

use std::sync::atomic::Ordering;

use another_core::*;

/// `PENDING_TTL_SECS` (10 minutes), which `another_core` does not export.
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

#[test]
fn an_expired_pending_authorization_cannot_be_approved() {
    let (clock, now) = Clock::manual(1_700_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "chatgpt".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let client = store
        .register_oauth_client(ClientRegistration {
            client_name: "ChatGPT".into(),
            redirect_uris: vec!["https://chatgpt.com/connector_platform_oauth_redirect".into()],
            application_type: Some("web".into()),
            remote: None,
        })
        .unwrap();
    let pending = store
        .begin_authorization(AuthorizeInput {
            client: ClientProfile {
                client_id: client.client_id.clone(),
                client_name: client.client_name.clone(),
                redirect_uris: client.redirect_uris.clone(),
            },
            redirect_uri: client.redirect_uris[0].clone(),
            state: None,
            scope: None,
            resource: "https://another.example.com/mcp".into(),
            // RFC 7636 appendix B: S256 of `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk`.
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
            code_challenge_method: "S256".into(),
            remote: None,
        })
        .unwrap();

    now.fetch_add(PENDING_TTL_MS + 1, Ordering::SeqCst);
    let err = store
        .approve_oauth(Approval {
            user_code: pending.user_code.clone(),
            agent: "chatgpt".into(),
            project_globs: vec!["/work/*".into()],
            label: None,
        })
        .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(
        store.oauth_grants().unwrap().is_empty(),
        "an expired code made a grant"
    );
}
