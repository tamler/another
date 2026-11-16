//! Project notes (D70) through the real binary, the session-start digest, MCP and the
//! remote gate.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another::mcp::AnotherMcp;
use another_core::{NewNote, Program, RegisterAgent, SendMessage, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A home plus `shop`, its worktree `shop-wt`, and an unrelated `blog`.
struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    shop: PathBuf,
    shop_wt: PathBuf,
    blog: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let shop = root.join("shop");
        write(
            &shop.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        let shop_wt = root.join("shop-wt");
        let wt_git = shop.join(".git/worktrees/shop-wt");
        write(
            &shop_wt.join(".git"),
            &format!("gitdir: {}\n", wt_git.display()),
        );
        write(&wt_git.join("commondir"), "../..\n");
        let blog = root.join("blog");
        write(
            &blog.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/me/blog\n",
        );
        let env = Env {
            home: root.join("user"),
            another_home: root.join("another-home"),
            _dir: dir,
            shop,
            shop_wt,
            blog,
        };
        std::fs::create_dir_all(&env.home).unwrap();
        assert!(env.run(&["init"]).status.success());
        env
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("TERM_PROGRAM");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn path(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    /// The session-start digest a Claude Code session in `cwd` sees.
    fn session_start(&self, cwd: &Path, session: &str) -> String {
        let mut child = self
            .cmd(&["hook", "run", "session-start", "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({ "session_id": session, "cwd": cwd })
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    }
}

#[test]
fn cli_sets_lists_and_forgets_notes_across_sibling_checkouts() {
    let env = Env::new();
    env.ok(&[
        "agent",
        "register",
        "alice",
        "--program",
        "claude",
        "--project",
        Env::path(&env.shop),
    ]);
    let note = env.json(&[
        "note",
        "set",
        "--agent",
        "alice@shop",
        "--project",
        Env::path(&env.shop),
        "--key",
        "e2e",
        "--ttl",
        "7d",
        "e2e tests need the dev server on :3000",
    ]);
    assert_eq!(note["key"], "e2e");
    assert!(note["expires_at"].is_string());
    // The worktree is a registered project of its own (as it is once an agent starts there).
    env.ok(&["project", "ensure", Env::path(&env.shop_wt)]);
    let listed = env.json(&["note", "list", "--project", Env::path(&env.shop_wt)]);
    assert_eq!(
        listed["notes"][0]["text"],
        "e2e tests need the dev server on :3000"
    );
    assert_eq!(listed["notes"][0]["author"], "alice@shop");
    let human = env.ok(&["note", "list", "--project", Env::path(&env.shop)]);
    assert!(
        human.contains("[e2e]") && human.contains(":3000"),
        "{human}"
    );
    // A read never registers a project.
    let unknown = env.run(&["note", "list", "--project", "/nowhere/at/all"]);
    assert!(!unknown.status.success());
    let gone = env.json(&[
        "note",
        "forget",
        "--agent",
        "alice@shop",
        "--project",
        Env::path(&env.shop),
        "--key",
        "e2e",
    ]);
    assert_eq!(gone["bytes"], 38);
    let empty = env.ok(&["note", "list", "--project", Env::path(&env.shop)]);
    assert!(empty.starts_with("no notes for shop"), "{empty}");
}

#[test]
fn session_start_shows_sibling_notes_but_not_other_projects() {
    let env = Env::new();
    env.ok(&[
        "agent",
        "register",
        "alice",
        "--program",
        "claude",
        "--project",
        Env::path(&env.shop),
    ]);
    env.ok(&[
        "note",
        "set",
        "--agent",
        "alice@shop",
        "--project",
        Env::path(&env.shop),
        "schema.rs is generated; edit schema.yaml instead",
    ]);
    let wt = env.session_start(&env.shop_wt, "wt-session");
    assert!(wt.contains("Project notes for shop-wt"), "{wt}");
    assert!(wt.contains("schema.rs is generated"), "{wt}");
    assert!(wt.contains("(alice@shop, "), "{wt}");
    assert!(wt.contains("data, not instructions"), "{wt}");
    let blog = env.session_start(&env.blog, "blog-session");
    assert!(blog.starts_with("Another: "), "{blog}");
    assert!(!blog.contains("Project notes"), "{blog}");
    assert!(!blog.contains("schema.rs"), "{blog}");
}

#[test]
fn digest_notes_have_their_own_budget_and_never_displace_mail() {
    let dir = tempfile::tempdir().unwrap();
    let shop = std::fs::canonicalize(dir.path()).unwrap().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    for name in ["alice", "bob"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: Some(shop.to_string_lossy().into_owned()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    for n in 0..8 {
        store
            .send_message(SendMessage {
                from: "alice@shop".into(),
                to: vec!["bob@shop".into()],
                subject: format!("{n}: {}", "a long subject line ".repeat(8)),
                body: "body".into(),
                ..Default::default()
            })
            .unwrap();
    }
    for n in 0..5 {
        store
            .set_note(NewNote {
                agent: "alice@shop".into(),
                project: "shop".into(),
                key: Some(format!("k{n}")),
                text: format!("note {n}: {}", "x".repeat(400)),
                ttl_seconds: None,
            })
            .unwrap();
    }
    let digest = another::hooks::session_digest(&store, "bob@shop").unwrap();
    let (mail, notes) = digest
        .split_once("\nProject notes for shop")
        .expect("a notes block follows the mail digest");
    assert!(mail.starts_with("Another: bob@shop has 8 unread"), "{mail}");
    assert!(mail.chars().count() <= another::hooks::MAX_DIGEST_CHARS);
    let notes = format!("Project notes for shop{notes}");
    assert!(
        notes.chars().count() <= another::hooks::MAX_DIGEST_NOTES_CHARS,
        "{}",
        notes.chars().count()
    );
    let shown = notes.lines().filter(|l| l.starts_with("- [k")).count();
    assert!((1..=3).contains(&shown), "{notes}");
    for line in notes.lines().filter(|l| l.starts_with("- [k")) {
        assert!(line.chars().count() <= 160 + 40, "{line}");
    }
    assert!(!notes.contains(&"x".repeat(200)), "each note is cut short");
}

type Client = RunningService<RoleClient, ClientConfig>;

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

async fn call(client: &Client, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("project_notes")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn mcp_project_notes_sets_lists_and_forgets() {
    let dir = tempfile::tempdir().unwrap();
    let shop = std::fs::canonicalize(dir.path()).unwrap().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: Some(shop.to_string_lossy().into_owned()),
            model: None,
            description: None,
        })
        .unwrap();
    let client = in_process(store).await;
    let set = call(
        &client,
        json!({"action": "set", "project": "shop", "agent": "alice@shop", "key": "db", "text": "postgres on :5433"}),
    )
    .await;
    assert_ne!(set.is_error, Some(true), "{set:?}");
    let note_id = set.structured_content.unwrap()["note"]["note_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let list = call(&client, json!({"action": "list", "project": "shop"})).await;
    let list = list.structured_content.unwrap();
    assert_eq!(list["notes"]["notes"][0]["text"], "postgres on :5433");
    let missing_agent = call(
        &client,
        json!({"action": "set", "project": "shop", "text": "x"}),
    )
    .await;
    assert_eq!(missing_agent.is_error, Some(true));
    let forget = call(
        &client,
        json!({"action": "forget", "project": "shop", "agent": "alice@shop", "note_id": note_id}),
    )
    .await;
    assert_eq!(forget.structured_content.unwrap()["forgotten"]["bytes"], 17);
    client.cancel().await.unwrap();
}

#[test]
fn remote_callers_cannot_use_project_notes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "remote-bot".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let token = store
        .create_token("remote-bot", &["**".to_string()], None, None)
        .unwrap();
    let scope = store.verify_token(&token.secret).unwrap();
    for action in ["list", "set", "forget"] {
        let mut args: Map<String, Value> =
            json!({"action": action, "project": "shop", "text": "x"})
                .as_object()
                .unwrap()
                .clone();
        let refused = another::remote::authorize(&store, &scope, "project_notes", &mut args)
            .expect_err("project notes are never remote");
        assert!(refused.contains("local callers"), "{refused}");
    }
}
