//! FINDINGS-main-debug-secrets: values that carry a credential must not print it through
//! `Debug`, so a future `tracing` field, `dbg!` or failed `unwrap` cannot put it in a log.
//! `IssuedTokens` already omits its secrets; the other credential carriers derive `Debug`.

use another_core::*;

const SECRET: &str = "s3cr3t-credential-value";

fn assert_hidden(what: &str, debug: String) {
    assert!(!debug.contains(SECRET), "{what} prints a secret: {debug}");
}

#[test]
fn issued_tokens_hide_their_secrets() {
    let t = IssuedTokens {
        access_token: SECRET.into(),
        refresh_token: Some(SECRET.into()),
        expires_in: 60,
        scope: "another".into(),
        grant_id: "g".into(),
        agent: "a".into(),
    };
    assert_hidden("IssuedTokens", format!("{t:?}"));
}

#[test]
fn a_code_exchange_hides_the_code_and_verifier() {
    for (code, verifier) in [(SECRET, "v"), ("c", SECRET)] {
        let e = CodeExchange {
            code: code.into(),
            client_id: "client".into(),
            redirect_uri: "http://127.0.0.1/cb".into(),
            code_verifier: verifier.into(),
            resource: "https://example.test/mcp".into(),
            remote: None,
        };
        assert_hidden("CodeExchange", format!("{e:?}"));
    }
}

#[test]
fn a_refresh_exchange_hides_the_refresh_token() {
    let e = RefreshExchange {
        refresh_token: SECRET.into(),
        client_id: "client".into(),
        resource: "https://example.test/mcp".into(),
        remote: None,
    };
    assert_hidden("RefreshExchange", format!("{e:?}"));
}

#[test]
fn a_new_token_hides_its_secret() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let new = store
        .create_token("alice", &["**".to_string()], None, None)
        .unwrap();
    let debug = format!("{new:?}");
    assert!(
        !debug.contains(&new.secret),
        "NewToken prints its bearer secret: {debug}"
    );
    // The JSON the CLI and MCP print at creation still carries it: that is its only copy.
    let json = serde_json::to_value(&new).unwrap();
    assert_eq!(json["secret"].as_str(), Some(new.secret.as_str()));
}
