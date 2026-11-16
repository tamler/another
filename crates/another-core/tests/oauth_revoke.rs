//! `another oauth revoke` (`Store::revoke_oauth_grant`), the operator's way to cut off an
//! approved client. Revoking a grant ends its access and refresh tokens at once, leaves
//! other grants alone, is audited once, and is a no-op when repeated.

use another_core::*;

/// RFC 7636 appendix B: this verifier's S256 challenge.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const ISSUER: &str = "https://another.example.com";
const RESOURCE: &str = "https://another.example.com/mcp";

fn store() -> Store {
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    for name in ["app", "other"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    store
}

/// Register a client, have the operator approve it for `agent`, and exchange the code:
/// what a hosted app holds after connecting.
fn connected(store: &Store, name: &str, agent: &str) -> (RegisteredClient, IssuedTokens) {
    let client = store
        .register_oauth_client(ClientRegistration {
            client_name: name.into(),
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
            // offline_access: the client also gets a refresh token.
            scope: Some("another offline_access".into()),
            resource: RESOURCE.into(),
            code_challenge: CHALLENGE.into(),
            code_challenge_method: "S256".into(),
            remote: None,
        })
        .unwrap();
    store
        .approve_oauth(Approval {
            user_code: pending.user_code.clone(),
            agent: agent.into(),
            project_globs: vec!["/w/**".into()],
            label: None,
        })
        .unwrap();
    let status = store
        .authorization_status(&pending.request_id, &pending.redirect_token, ISSUER, true)
        .unwrap();
    let redirect = status.redirect.expect("an approved client is sent back");
    let code = redirect
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("code="))
        .expect("a code in the redirect")
        .to_string();
    let tokens = store
        .exchange_code(CodeExchange {
            code,
            client_id: client.client_id.clone(),
            redirect_uri: client.redirect_uris[0].clone(),
            code_verifier: VERIFIER.into(),
            resource: RESOURCE.into(),
            remote: None,
        })
        .unwrap();
    (client, tokens)
}

fn refresh(store: &Store, client: &RegisteredClient, tokens: &IssuedTokens) -> bool {
    store
        .refresh_oauth(RefreshExchange {
            refresh_token: tokens.refresh_token.clone().expect("a refresh token"),
            client_id: client.client_id.clone(),
            resource: RESOURCE.into(),
            remote: None,
        })
        .is_ok()
}

fn revokes(store: &Store) -> Vec<AuditEntry> {
    store
        .audit_log(100)
        .unwrap()
        .into_iter()
        .filter(|e| e.operation == "oauth.revoke")
        .collect()
}

#[test]
fn a_revoked_grant_loses_its_tokens_and_others_keep_theirs() {
    let store = store();
    let (app, app_tokens) = connected(&store, "Some App", "app");
    let (other, other_tokens) = connected(&store, "Other App", "other");
    // Both work before.
    let scope = store
        .verify_access_token(&app_tokens.access_token, RESOURCE)
        .unwrap();
    assert_eq!(scope.agent_id, store.agent("app").unwrap().agent_id);
    store
        .verify_access_token(&other_tokens.access_token, RESOURCE)
        .unwrap();

    let revoked = store.revoke_oauth_grant(&app_tokens.grant_id).unwrap();
    assert!(revoked.revoked_at.is_some(), "{revoked:?}");

    // The access token is refused at once, as forbidden, and so is its refresh token.
    let err = store
        .verify_access_token(&app_tokens.access_token, RESOURCE)
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");
    assert!(
        !refresh(&store, &app, &app_tokens),
        "a revoked grant refreshed"
    );

    // The other client's grant is untouched.
    store
        .verify_access_token(&other_tokens.access_token, RESOURCE)
        .unwrap();
    assert!(refresh(&store, &other, &other_tokens));
    let grants = store.oauth_grants().unwrap();
    let still = grants
        .iter()
        .find(|g| g.grant_id == other_tokens.grant_id)
        .unwrap();
    assert!(still.revoked_at.is_none());
}

#[test]
fn revoking_is_audited_once_and_repeating_it_changes_nothing() {
    let store = store();
    let (_, tokens) = connected(&store, "Some App", "app");
    let first = store.revoke_oauth_grant(&tokens.grant_id).unwrap();
    let audits = revokes(&store);
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0].outcome, "ok");
    assert_eq!(
        audits[0].token_id.as_deref(),
        Some(tokens.grant_id.as_str())
    );
    assert!(
        audits[0]
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("Some App"),
        "{audits:?}"
    );

    // A second revoke keeps the first time and writes no second entry.
    let again = store.revoke_oauth_grant(&tokens.grant_id).unwrap();
    assert_eq!(again.revoked_at, first.revoked_at);
    assert_eq!(revokes(&store).len(), 1);
}

#[test]
fn revoking_an_unknown_grant_is_not_found() {
    let store = store();
    let err = store.revoke_oauth_grant("no-such-grant").unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    assert!(revokes(&store).is_empty());
}
