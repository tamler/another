//! `another oauth pending | approve | deny | list | revoke`: the operator's side of OAuth,
//! through the real binary. Waiting clients are created with the core API, the same calls
//! the HTTP `/authorize` endpoint makes, in a temporary home.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use another_core::{
    AuthorizeInput, ClientProfile, ClientRegistration, Program, RegisterAgent, Store,
};
use serde_json::Value;

const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";

fn another(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> Value {
    assert!(o.status.success(), "{}", err(o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// A home with the agent `chatgpt` and `n` clients waiting for approval; their user codes.
fn waiting(home: &Path, names: &[&str]) -> Vec<String> {
    let store = Store::open(home).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "chatgpt".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    names
        .iter()
        .map(|name| {
            let client = store
                .register_oauth_client(ClientRegistration {
                    client_name: (*name).into(),
                    redirect_uris: vec![REDIRECT.into()],
                    application_type: None,
                    remote: None,
                })
                .unwrap();
            store
                .begin_authorization(AuthorizeInput {
                    client: ClientProfile {
                        client_id: client.client_id,
                        client_name: client.client_name,
                        redirect_uris: client.redirect_uris,
                    },
                    redirect_uri: REDIRECT.into(),
                    state: None,
                    scope: None,
                    resource: "https://another.example.com/mcp".into(),
                    code_challenge: CHALLENGE.into(),
                    code_challenge_method: "S256".into(),
                    remote: None,
                })
                .unwrap()
                .user_code
        })
        .collect()
}

#[test]
fn nothing_waiting_and_no_grants_say_so() {
    let dir = tempfile::tempdir().unwrap();
    waiting(dir.path(), &[]);
    let out = another(dir.path(), &["oauth", "pending"]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(
        text(&out).contains("no clients are waiting"),
        "{}",
        text(&out)
    );
    assert_eq!(
        json(&another(dir.path(), &["--json", "oauth", "pending"])),
        Value::Array(vec![])
    );
    let out = another(dir.path(), &["oauth", "list"]);
    assert_eq!(text(&out).trim(), "no OAuth grants");
    assert_eq!(
        json(&another(dir.path(), &["--json", "oauth", "list"])),
        Value::Array(vec![])
    );
}

/// pending → approve → list → revoke → list, in text and JSON.
#[test]
fn approve_list_and_revoke_a_grant() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let codes = waiting(home, &["ChatGPT", "Claude"]);

    let out = another(home, &["oauth", "pending"]);
    let t = text(&out);
    for (code, name) in codes.iter().zip(["ChatGPT", "Claude"]) {
        assert!(
            t.lines()
                .any(|l| l.starts_with(code.as_str()) && l.contains(name)),
            "{t}"
        );
    }
    assert!(t.contains("redirect host chatgpt.com"), "{t}");
    let rows = json(&another(home, &["--json", "oauth", "pending"]));
    assert_eq!(rows.as_array().unwrap().len(), 2, "{rows}");

    // Approve the first, typed in upper case, bound to one agent and one project glob.
    let upper = codes[0].to_ascii_uppercase();
    let out = another(
        home,
        &[
            "oauth",
            "approve",
            &upper,
            "--agent",
            "chatgpt",
            "--project",
            "/work/*",
            "--label",
            "laptop",
        ],
    );
    assert!(out.status.success(), "{}", err(&out));
    let t = text(&out);
    assert!(t.starts_with("approved ChatGPT"), "{t}");
    assert!(t.contains("as chatgpt"), "{t}");
    assert!(t.contains("projects: /work/*"), "{t}");
    let grant_id = t
        .lines()
        .last()
        .unwrap()
        .trim_start_matches("grant ")
        .to_string();

    // The approved client no longer waits; the other one still does.
    let rows = json(&another(home, &["--json", "oauth", "pending"]));
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows[0]["client_name"], "Claude");

    // A decided code cannot be approved again: a conflict (exit 3).
    let out = another(home, &["oauth", "approve", &codes[0], "--agent", "chatgpt"]);
    assert_eq!(out.status.code(), Some(3), "{}", err(&out));
    assert!(err(&out).contains("already"), "{}", err(&out));

    // Approve the second with no project: mailbox and feed only.
    let out = another(home, &["oauth", "approve", &codes[1], "--agent", "chatgpt"]);
    assert!(
        text(&out).contains("projects: none: mailbox, heartbeat and the activity feed only"),
        "{}",
        text(&out)
    );

    let out = another(home, &["oauth", "list"]);
    let t = text(&out);
    let line = t
        .lines()
        .find(|l| l.starts_with(&grant_id))
        .unwrap_or_else(|| panic!("{t}"));
    assert!(
        line.contains(" active ") && line.contains("ChatGPT") && line.contains("projects /work/*"),
        "{line}"
    );
    assert!(t.contains("projects none"), "{t}");
    let grants = json(&another(home, &["--json", "oauth", "list"]));
    let g = grants
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["grant_id"] == grant_id.as_str())
        .unwrap();
    assert_eq!(g["label"], "laptop");
    assert!(g["revoked_at"].is_null());

    let out = another(home, &["oauth", "revoke", &grant_id]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(
        text(&out).starts_with(&format!("revoked grant {grant_id} (ChatGPT, ")),
        "{}",
        text(&out)
    );
    let out = another(home, &["oauth", "list"]);
    let line = text(&out)
        .lines()
        .find(|l| l.starts_with(&grant_id))
        .unwrap()
        .to_string();
    assert!(line.contains(" revoked "), "{line}");
    let revoked = json(&another(home, &["--json", "oauth", "revoke", &grant_id]));
    assert!(revoked["revoked_at"].is_string(), "{revoked}");
}

#[test]
fn deny_and_the_mistakes_an_operator_can_make() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let codes = waiting(home, &["ChatGPT", "Claude"]);

    let out = another(home, &["oauth", "deny", &codes[0]]);
    assert!(out.status.success(), "{}", err(&out));
    assert_eq!(text(&out).trim(), format!("denied {} (ChatGPT)", codes[0]));
    let rows = json(&another(home, &["--json", "oauth", "pending"]));
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    // Denied is final: neither deny nor approve again (conflict, exit 3).
    for args in [
        vec!["oauth", "deny", codes[0].as_str()],
        vec!["oauth", "approve", codes[0].as_str(), "--agent", "chatgpt"],
    ] {
        let out = another(home, &args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", err(&out));
    }

    // Unknown code, unknown agent, unknown grant: errors that say what to do (exit 1).
    let out = another(home, &["oauth", "deny", "zzzz-zzzz"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(err(&out).contains("no client is waiting"), "{}", err(&out));
    let out = another(
        home,
        &["oauth", "approve", "zzzz-zzzz", "--agent", "chatgpt"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(err(&out).contains("another oauth pending"), "{}", err(&out));
    let out = another(home, &["oauth", "approve", &codes[1], "--agent", "nobody"]);
    assert!(!out.status.success(), "approved for an unknown agent");
    let rows = json(&another(home, &["--json", "oauth", "pending"]));
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "a failed approval decided the request: {rows}"
    );
    let out = another(home, &["oauth", "revoke", "no-such-grant"]);
    assert_eq!(out.status.code(), Some(1));
    // The JSON form of an error.
    let out = another(home, &["--json", "oauth", "revoke", "no-such-grant"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "not_found", "{v}");

    // `approve` needs --agent: clap's usage error (exit 2).
    let out = another(home, &["oauth", "approve", &codes[1]]);
    assert_eq!(out.status.code(), Some(2));
}
