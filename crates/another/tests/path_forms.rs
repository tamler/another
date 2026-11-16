//! One directory is one project, whichever way its path was spelled and whichever entry
//! point (CLI or MCP) named it first. Spellings covered: a symlink to the directory (as
//! macOS `/var` is to `/private/var`), and on Windows the path as the OS hands it out
//! (short 8.3 names, the drive-letter case, no `\\?\` prefix) against its canonical form.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use another::mcp::AnotherMcp;
use another_core::Store;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

type Client = RunningService<RoleClient, ClientConfig>;

async fn mcp(home: &Path) -> Client {
    let store = Store::open(home).unwrap();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let res = client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

fn cli(home: &Path, cwd: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(&all)
        .current_dir(cwd)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn project_paths(home: &Path) -> Vec<String> {
    Store::open(home)
        .unwrap()
        .projects()
        .unwrap()
        .into_iter()
        .map(|p| p.abs_path)
        .collect()
}

/// A directory and a second spelling of the same directory.
struct Spellings {
    _dir: tempfile::TempDir,
    home: PathBuf,
    /// The spelling an MCP client would send as given.
    given: PathBuf,
}

fn spellings() -> Spellings {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let real = dir.path().join("work").join("shop");
    std::fs::create_dir_all(&real).unwrap();
    #[cfg(unix)]
    let given = {
        let link = dir.path().join("shop-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        link
    };
    #[cfg(not(unix))]
    let given = real.clone();
    Spellings {
        _dir: dir,
        home,
        given,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_ensure_project_and_the_cli_agree_on_one_project() {
    let s = spellings();
    let given = s.given.to_string_lossy().into_owned();
    // The CLI registers the directory from inside it (it resolves the path).
    cli(
        &s.home,
        &s.given,
        &[
            "agent",
            "register",
            "w",
            "--program",
            "codex",
            "--project",
            ".",
        ],
    );
    // An MCP client names the same directory as it knows it.
    let client = mcp(&s.home).await;
    let p = call(&client, "ensure_project", json!({ "path": given })).await;
    let paths = project_paths(&s.home);
    assert_eq!(
        paths.len(),
        1,
        "one directory, two projects: {paths:?} (mcp gave {p})"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_first_then_the_cli_still_one_project() {
    let s = spellings();
    let given = s.given.to_string_lossy().into_owned();
    let client = mcp(&s.home).await;
    call(&client, "ensure_project", json!({ "path": given })).await;
    cli(
        &s.home,
        &s.given,
        &[
            "agent",
            "register",
            "w",
            "--program",
            "codex",
            "--project",
            ".",
        ],
    );
    let paths = project_paths(&s.home);
    assert_eq!(paths.len(), 1, "{paths:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_register_agent_with_a_project_path_uses_the_same_project() {
    let s = spellings();
    let given = s.given.to_string_lossy().into_owned();
    cli(
        &s.home,
        &s.given,
        &[
            "agent",
            "register",
            "w",
            "--program",
            "codex",
            "--project",
            ".",
        ],
    );
    let client = mcp(&s.home).await;
    call(
        &client,
        "register_agent",
        json!({ "name": "c", "program": "claude", "project": given }),
    )
    .await;
    let paths = project_paths(&s.home);
    assert_eq!(paths.len(), 1, "{paths:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_hold_through_another_spelling_conflicts_with_a_cli_hold() {
    let s = spellings();
    let given = s.given.to_string_lossy().into_owned();
    cli(
        &s.home,
        &s.given,
        &[
            "agent",
            "register",
            "w",
            "--program",
            "codex",
            "--project",
            ".",
        ],
    );
    cli(
        &s.home,
        &s.given,
        &[
            "agent",
            "register",
            "c",
            "--program",
            "claude",
            "--project",
            ".",
        ],
    );
    cli(
        &s.home,
        &s.given,
        &["hold", "acquire", "-a", "w", "src/lib.rs"],
    );
    let client = mcp(&s.home).await;
    let file = s
        .given
        .join("src")
        .join("lib.rs")
        .to_string_lossy()
        .into_owned();
    let out = call(
        &client,
        "hold_paths",
        json!({ "agent": "c", "action": "check", "project": given, "paths": [file] }),
    )
    .await;
    let conflicts = out["conflicts"].as_array().cloned().unwrap_or_default();
    assert_eq!(conflicts.len(), 1, "{out}");
}
