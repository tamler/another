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

/// Review R3 of Q19b: `another oauth pending` is what the operator approves from. The
/// redirect host is the client's text, and a browser does not read it as a person does,
/// so only a plain host is shown as it is. Any other is shown whole, each character that
/// is not a letter, digit, `.` or `-` as an escape, behind Another's warning. Nothing is
/// cut and nothing becomes a space. The client's name is inside quotes it cannot close.
#[test]
fn oauth_pending_shows_a_redirect_host_exactly_or_escaped_behind_a_warning() {
    use another_core::{AuthorizeInput, ClientProfile, ClientRegistration, NOT_A_PLAIN_HOST};
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
    let begin = |name: &str, authority: &str| {
        let uri = format!("https://{authority}/cb");
        store
            .begin_authorization(AuthorizeInput {
                client: ClientProfile {
                    client_id: client.client_id.clone(),
                    client_name: name.into(),
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
            .unwrap()
            .user_code
    };

    let warned = |escaped: &str| format!("{NOT_A_PLAIN_HOST}{escaped}");
    let dns_limit = format!("{}.attacker.test", "a".repeat(253 - 14));
    let over_limit = format!("a{dns_limit}");
    let huge = format!("good.example.com.{}.attacker.test", "h".repeat(5000 - 31));
    let backslash_long = format!("attacker.test\\{}.good.example.com", "x".repeat(300));
    // (the authority in the redirect URI, what the row shows as the host)
    let cases: Vec<(String, String)> = vec![
        // Plain: exactly, no warning. The port is not part of the host Another keeps.
        ("chatgpt.com".into(), "chatgpt.com".into()),
        ("chatgpt.com:8443".into(), "chatgpt.com".into()),
        ("[2001:DB8::1]:8443".into(), "2001:db8::1".into()),
        ("xn--bcher-kva.example".into(), "xn--bcher-kva.example".into()),
        (dns_limit.clone(), dns_limit.clone()),
        // Not plain: whole, escaped, warned.
        (over_limit.clone(), warned(&over_limit)),
        (huge.clone(), warned(&huge)),
        (
            "b\u{fc}cher.example".into(),
            warned("b\\u{00FC}cher.example"),
        ),
        // A browser ends the host at the backslash: the code goes to `attacker.test`.
        (
            "attacker.test\\good.example.com".into(),
            warned("attacker.test\\u{005C}good.example.com"),
        ),
        (
            backslash_long,
            warned(&format!(
                "attacker.test\\u{{005C}}{}.good.example.com",
                "x".repeat(300)
            )),
        ),
        // A browser drops zero-width characters: one joined name, not two.
        (
            "auth.openai.com\u{200b}\u{200b}\u{200b}chatgpt.com".into(),
            warned("auth.openai.com\\u{200B}\\u{200B}\\u{200B}chatgpt.com"),
        ),
        // A host that spells the row's own fields.
        (
            "attacker.test\u{200b}\u{200b}expires\u{200b}2026-11-20t09.00.00.000z\u{200b}\u{200b}redirect\u{200b}host\u{200b}chatgpt.com".into(),
            warned("attacker.test\\u{200B}\\u{200B}expires\\u{200B}2026-11-20t09.00.00.000z\\u{200B}\\u{200B}redirect\\u{200B}host\\u{200B}chatgpt.com"),
        ),
        // A host that imitates the mark an earlier build printed for a cut host.
        (
            "(cut,\u{200b}12\u{200b}characters)\u{200b}...good.example.com".into(),
            warned("\\u{0028}cut\\u{002C}\\u{200B}12\\u{200B}characters\\u{0029}\\u{200B}...good.example.com"),
        ),
        // A host that imitates the warning.
        (
            "x[warning\u{200b}plain\u{200b}host]\u{200b}chatgpt.com".into(),
            warned("x\\u{005B}warning\\u{200B}plain\\u{200B}host\\u{005D}\\u{200B}chatgpt.com"),
        ),
    ];
    let mut expected = std::collections::BTreeMap::new();
    for (authority, shown) in &cases {
        expected.insert(begin("ChatGPT", authority), shown.clone());
    }
    // A name that spells the row's fields, and one that tries to close its quotes.
    let forged_name = "ChatGPT  redirect host chatgpt.com  expires 2026-11-20T09:00:00.000Z";
    let forged_code = begin(forged_name, "attacker.test");
    let quote_code = begin("ChatGPT\"  redirect host chatgpt.com", "attacker.test");
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
    assert!(
        !text
            .chars()
            .any(|c| c != '\n' && (another_core::is_unsafe_text_char(c) || c == '\u{200b}')),
        "{text:?}"
    );
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(
        rows.len(),
        cases.len() + 2,
        "one row for each request: {rows:?}"
    );
    for (code, shown) in &expected {
        let row = rows
            .iter()
            .find(|r| r.starts_with(code.as_str()))
            .unwrap_or_else(|| panic!("no row for {code}"));
        let rest = row
            .strip_prefix(&format!("{code}  \"ChatGPT\"  redirect host "))
            .unwrap_or_else(|| panic!("{row:?}"));
        let (host, expires) = rest.rsplit_once("  expires ").unwrap();
        assert_eq!(host, shown, "{row:?}");
        assert!(!expires.contains(' '), "{row:?}");
    }
    let row = rows.iter().find(|r| r.starts_with(&forged_code)).unwrap();
    assert!(
        row.starts_with(&format!(
            "{forged_code}  \"{forged_name}\"  redirect host attacker.test  expires "
        )),
        "{row:?}"
    );
    let row = rows.iter().find(|r| r.starts_with(&quote_code)).unwrap();
    assert!(
        row.starts_with(&format!(
            "{quote_code}  \"ChatGPT'  redirect host chatgpt.com\"  redirect host attacker.test  expires "
        )),
        "{row:?}"
    );

    // The audit row of each request names the host by the same rule.
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["audit", "--limit", "100"])
        .current_dir(dir.path())
        .env("ANOTHER_HOME", &home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run another");
    assert!(out.status.success(), "{out:?}");
    let audit = String::from_utf8_lossy(&out.stdout);
    for (_, shown) in &cases {
        assert!(
            audit.lines().any(|l| l.contains("oauth.authorize")
                && l.ends_with(&format!(" redirect host {shown}"))),
            "no audit row ends with the host shown as {shown:?}"
        );
    }
}
