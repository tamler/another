//! `another oauth deny` (`Store::deny_oauth`), the operator's only way to refuse a waiting
//! client. A denied client gets an `access_denied` redirect, never a code or token, and the
//! denial is final and audited.

use another_core::*;

/// RFC 7636 appendix B: this verifier's S256 challenge.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const ISSUER: &str = "https://another.example.com";
const RESOURCE: &str = "https://another.example.com/mcp";

fn waiting_client(store: &Store) -> (RegisteredClient, PendingAuthorization) {
    let client = store
        .register_oauth_client(ClientRegistration {
            client_name: "Some App".into(),
            redirect_uris: vec!["https://app.example.com/callback".into()],
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
            state: Some("st8".into()),
            scope: Some("another".into()),
            resource: RESOURCE.into(),
            code_challenge: CHALLENGE.into(),
            code_challenge_method: "S256".into(),
            remote: None,
        })
        .unwrap();
    (client, pending)
}

fn store() -> Store {
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "app".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    store
}

#[test]
fn a_denied_client_is_sent_back_with_access_denied_and_never_gets_a_code() {
    let store = store();
    let (client, pending) = waiting_client(&store);
    assert_eq!(store.oauth_pending().unwrap().len(), 1);

    // The operator may type the code in any case.
    let denied = store
        .deny_oauth(&pending.user_code.to_ascii_uppercase())
        .unwrap();
    assert_eq!(denied.user_code, pending.user_code);
    assert_eq!(denied.client_name, "Some App");
    assert!(store.oauth_pending().unwrap().is_empty());

    // The browser poll learns of it, even when asked to issue a code.
    let status = store
        .authorization_status(&pending.request_id, &pending.redirect_token, ISSUER, true)
        .unwrap();
    assert_eq!(status.status, "denied");
    let redirect = status.redirect.expect("a denied client is sent back");
    assert!(
        redirect.starts_with("https://app.example.com/callback?"),
        "{redirect}"
    );
    assert!(redirect.contains("error=access_denied"), "{redirect}");
    assert!(redirect.contains("state=st8"), "{redirect}");
    assert!(!redirect.contains("code="), "{redirect}");

    // A code guessed from the request id is refused.
    let guessed = store.exchange_code(CodeExchange {
        code: format!("anoac_{}_{}", pending.request_id, "0".repeat(64)),
        client_id: client.client_id.clone(),
        redirect_uri: client.redirect_uris[0].clone(),
        code_verifier: VERIFIER.into(),
        resource: RESOURCE.into(),
        remote: None,
    });
    assert!(guessed.is_err());
    assert!(store.oauth_grants().unwrap().is_empty());
}

#[test]
fn a_denial_is_final_and_audited() {
    let store = store();
    let (_, pending) = waiting_client(&store);
    store.deny_oauth(&pending.user_code).unwrap();

    // It cannot be approved afterwards, or denied twice.
    let approve = store.approve_oauth(Approval {
        user_code: pending.user_code.clone(),
        agent: "app".into(),
        project_globs: vec![],
        label: None,
    });
    assert!(approve.is_err(), "{approve:?}");
    assert!(store.oauth_grants().unwrap().is_empty());
    let again = store.deny_oauth(&pending.user_code).unwrap_err();
    assert!(matches!(again, Error::Conflict(_)), "{again:?}");

    let audit = store.audit_log(50).unwrap();
    let denials: Vec<_> = audit
        .iter()
        .filter(|e| e.operation == "oauth.deny")
        .collect();
    assert_eq!(denials.len(), 1, "{audit:?}");
    assert_eq!(denials[0].outcome, "denied");
    assert!(
        denials[0]
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("Some App"),
        "{denials:?}"
    );
}

#[test]
fn denying_an_unknown_code_names_it() {
    let store = store();
    let err = store.deny_oauth("zzzz-zzzz").unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    assert!(err.to_string().contains("zzzz-zzzz"), "{err}");
}
