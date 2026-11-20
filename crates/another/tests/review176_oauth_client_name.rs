//! Adversarial review of #176: `another serve` prints a registering client's name to the
//! operator's terminal (`oauth_http.rs`, "{client_name} wants to connect"), outside `emit`.
//! Dynamic client registration is unauthenticated, so the name must not carry control
//! characters into that line.

use std::net::SocketAddr;

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn a_registered_client_name_cannot_carry_terminal_control_characters() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        auth: true,
        bound_port: addr.port(),
        oauth: Some(parse_public_url(&base).unwrap()),
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
    let stop = ct.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "Helper\u{1b}]0;pwned\u{7}\u{1b}[2J",
            "redirect_uris": ["https://example.com/cb"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "web"
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    // If it registered, the authorization request is what makes `another serve` print the
    // name ("... wants to connect") on the operator's terminal.
    if let Some(client_id) = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["client_id"].as_str().map(str::to_string))
    {
        let url = format!(
            "{base}/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=https%3A%2F%2Fexample.com%2Fcb&state=s\
             &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &code_challenge_method=S256&resource={}",
            urlencode(&format!("{base}/mcp"))
        );
        let r = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        println!("authorize status: {}", r.status());
    }
    ct.cancel();
    server.await.unwrap();

    println!("register status: {status}; body: {body}");
    assert!(
        status.is_client_error(),
        "a client_name with ESC/BEL was accepted ({status}); `another serve` later prints it raw: {body}"
    );
}

/// Review R1 of Q19b: a name stored by a build before that refusal is still in the
/// database. The operator approves a client from what `another oauth pending` lists, so a
/// stored name must not add a row there, nor a line to what approve, deny, list and revoke
/// print. The name is handed over as `resolve_client` reads it from `oauth_clients`.
#[test]
fn a_stored_client_name_cannot_add_a_row_to_the_oauth_commands() {
    use another_core::{AuthorizeInput, ClientProfile, ClientRegistration, Program, RegisterAgent};
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "helper".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let client = store
        .register_oauth_client(ClientRegistration {
            client_name: "Helper".into(),
            redirect_uris: vec!["https://example.com/cb".into()],
            application_type: Some("web".into()),
            remote: None,
        })
        .unwrap();
    let stored = "Helper\n0000-0000  ChatGPT  redirect host chatgpt.com  expires never\u{1b}[2J";
    let begin = || {
        store
            .begin_authorization(AuthorizeInput {
                client: ClientProfile {
                    client_id: client.client_id.clone(),
                    client_name: stored.into(),
                    redirect_uris: client.redirect_uris.clone(),
                },
                redirect_uri: client.redirect_uris[0].clone(),
                state: None,
                scope: None,
                resource: "https://another.example.com/mcp".into(),
                code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
                code_challenge_method: "S256".into(),
                remote: None,
            })
            .unwrap()
    };
    let (first, second) = (begin(), begin());
    drop(store);

    let lines = |args: &[&str]| -> Vec<String> {
        let out = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .current_dir(dir.path())
            .env("ANOTHER_HOME", &home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .expect("run another");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(
            !text
                .chars()
                .any(|c| c != '\n' && another_core::is_unsafe_text_char(c)),
            "{args:?}: {text:?}"
        );
        text.lines().map(str::to_string).collect()
    };

    let pending = lines(&["oauth", "pending"]);
    assert_eq!(pending.len(), 2, "one row for each request: {pending:?}");
    assert!(
        pending.iter().all(|l| !l.starts_with("0000-0000")),
        "{pending:?}"
    );
    let denied = lines(&["oauth", "deny", &second.user_code]);
    assert_eq!(denied.len(), 1, "{denied:?}");
    let approved = lines(&["oauth", "approve", &first.user_code, "--agent", "helper"]);
    assert_eq!(approved.len(), 3, "{approved:?}");
    let listed = lines(&["oauth", "list"]);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let grant = listed[0].split_whitespace().next().unwrap().to_string();
    let revoked = lines(&["oauth", "revoke", &grant]);
    assert_eq!(revoked.len(), 1, "{revoked:?}");

    // Review R2: the audit rows of the approval and the revocation quote the stored name.
    // Each entry is one line.
    let entries: serde_json::Value =
        serde_json::from_str(&lines(&["--json", "audit"]).join("\n")).unwrap();
    let entries = entries.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["detail"].as_str().is_some_and(|d| d.contains('\n'))),
        "an audit row holds the stored name: {entries:?}"
    );
    let audit = lines(&["audit"]);
    assert_eq!(audit.len(), entries.len(), "{audit:?}");
}

/// Review R2 of Q19b: the redirect host is what tells the operator who receives the code,
/// and its end is the part that says so. `another oauth pending` shows a host of up to 253
/// characters whole, and a longer one by a mark, its length and its last 120 characters.
#[test]
fn oauth_pending_shows_the_end_of_a_long_redirect_host() {
    use another_core::{AuthorizeInput, ClientProfile, ClientRegistration};
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    let client = store
        .register_oauth_client(ClientRegistration {
            client_name: "ChatGPT".into(),
            redirect_uris: vec!["https://example.com/cb".into()],
            application_type: Some("web".into()),
            remote: None,
        })
        .unwrap();
    // A trusted-looking start, the attacker's registrable domain at the end.
    let long = format!(
        "good.example.com.{}.{}.attacker.test",
        "x".repeat(60),
        "x".repeat(60)
    );
    let huge = format!("good.example.com.{}.attacker.test", "h".repeat(5000 - 31));
    for host in [&long, &huge] {
        let uri = format!("https://{host}/cb");
        store
            .begin_authorization(AuthorizeInput {
                client: ClientProfile {
                    client_id: client.client_id.clone(),
                    client_name: client.client_name.clone(),
                    redirect_uris: vec![uri.clone()],
                },
                redirect_uri: uri,
                state: None,
                scope: None,
                resource: "https://another.example.com/mcp".into(),
                code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
                code_challenge_method: "S256".into(),
                remote: None,
            })
            .unwrap();
    }
    drop(store);

    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["oauth", "pending"])
        .current_dir(dir.path())
        .env("ANOTHER_HOME", &home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run another");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter()
            .any(|r| r.contains(&format!("  ChatGPT  redirect host {long}  expires "))),
        "{rows:?}"
    );
    let cut = format!(
        "  ChatGPT  redirect host (cut, 5000 characters) ...{}.attacker.test  expires ",
        "h".repeat(120 - 14)
    );
    assert!(rows.iter().any(|r| r.contains(&cut)), "{rows:?}");
}
