//! `another board`: the read-only work board, driven through the real binary and the real
//! edit hook.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "common/cli.rs"]
mod cli;

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    main: PathBuf,
    wt: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The temp dir as a client would report it. Canonicalizing resolves macOS's
/// `/var` -> `/private/var` link, but on Windows it adds a `\\?\` verbatim prefix that no
/// client sends in `cwd`, so drop that prefix there.
fn client_path(p: &Path) -> PathBuf {
    let real = std::fs::canonicalize(p).unwrap();
    #[cfg(windows)]
    if let Some(plain) = real.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        return PathBuf::from(plain);
    }
    real
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = client_path(dir.path());
        let main = root.join("work").join("shop");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:team/shop.git\n",
        );
        std::fs::create_dir_all(main.join("src")).unwrap();
        let wt = root.join("work").join("shop-review");
        let wt_git = main.join(".git/worktrees/shop-review");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let home = root.join("home");
        Env {
            another_home: home.join(".another"),
            home,
            main,
            wt,
            _dir: dir,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = cli::another_cmd(&self.another_home);
        c.args(args).env("HOME", &self.home);
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

    fn hook(&self, event: &str, client: &str, stdin: &Value) -> String {
        let mut child = self
            .cmd(&["hook", "run", event, "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook run must always exit 0");
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, client: &str, dir: &Path) {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook("session-start", client, &stdin);
    }

    fn edit(&self, client: &str, dir: &Path, file: &str) -> String {
        let stdin = json!({
            "session_id": format!("{client}-s1"),
            "cwd": dir,
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join(file), "old_string": "x", "new_string": "y" },
        });
        self.hook("pre-edit", client, &stdin)
    }

    fn event_count(&self) -> usize {
        self.json(&["events", "--limit", "100000"])
            .as_array()
            .unwrap()
            .len()
    }
}

#[test]
fn edit_hook_reason_matches_the_core_constant() {
    assert_eq!(
        another::hooks::AUTO_HOLD_REASON,
        another_core::EDIT_HOOK_HOLD_REASON
    );
}

#[test]
fn board_needs_an_existing_database_and_creates_nothing() {
    let env = Env::new();
    let out = env.run(&["board"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("run `another init` first"), "{err}");
    assert!(!env.another_home.exists(), "board must not create a home");
    std::fs::create_dir_all(&env.another_home).unwrap();
    board_read_error(&env, "error", "run `another init` first");
    assert!(env.another_home.read_dir().unwrap().next().is_none());
}

fn board_read_error(env: &Env, code: &str, message: &str) {
    let out = env.run(&["--json", "board"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty(), "{:?}", out.stderr);
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    let error = &body["error"];
    assert_eq!(error["code"], code);
    assert!(
        error["message"].as_str().unwrap().contains(message),
        "{body}"
    );
    assert_eq!(error["hint"], "run another doctor");
}

#[test]
fn board_reports_an_exclusive_writer_lock() {
    let env = Env::new();
    env.ok(&["init"]);
    let db = rusqlite::Connection::open(env.another_home.join(another_core::DB_FILE)).unwrap();
    db.pragma_update(None, "journal_mode", "DELETE").unwrap();
    db.execute_batch("BEGIN EXCLUSIVE; UPDATE nodes SET display_name = display_name;")
        .unwrap();
    board_read_error(&env, "db", "database is locked");
    db.execute_batch("ROLLBACK").unwrap();
    env.ok(&["board"]);
}

#[test]
fn board_refuses_an_old_schema_without_migrating() {
    let env = Env::new();
    let current = Env::new();
    current.ok(&["init"]);
    let db_path = |env: &Env| env.another_home.join(another_core::DB_FILE);
    let db = rusqlite::Connection::open(db_path(&current)).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    std::fs::create_dir_all(&env.another_home).unwrap();
    let db = rusqlite::Connection::open(db_path(&env)).unwrap();
    another_core::apply_schema(&db, version - 1).unwrap();
    drop(db);
    let before = std::fs::read(db_path(&env)).unwrap();

    board_read_error(&env, "invalid", "schema");
    assert_eq!(std::fs::read(db_path(&env)).unwrap(), before);
    assert!(!env.another_home.join(another_core::ORIGIN_FILE).exists());
}

#[test]
fn board_shows_what_the_edit_hook_recorded_and_what_waits() {
    let env = Env::new();
    env.start("claude", &env.main);
    env.start("codex", &env.wt);
    assert_eq!(env.edit("claude", &env.main, "src/a.rs"), "");
    // Codex works in a sibling worktree, where the same file is its own (D92): a hold blocks
    // only inside its checkout.
    assert_eq!(env.edit("codex", &env.wt, "src/a.rs"), "");
    // Reaching into the main checkout by path, it is stopped on Claude's file there.
    let stopped = env.edit(
        "codex",
        &env.wt,
        env.main.join("src/a.rs").to_str().unwrap(),
    );
    assert!(
        stopped.contains("held by claude@shop"),
        "the edit hook did not stop codex: {stopped:?}; holds: {}",
        env.ok(&["--json", "holds"])
    );
    env.ok(&[
        "send",
        "--from",
        "claude@shop",
        "--to",
        "codex@shop-review",
        "--subject",
        "Review src/a.rs",
        "--body",
        "secret body",
        "--request",
        "--ack",
    ]);

    let before = env.event_count();
    let board = env.json(&["board", "--project", "shop"]);
    let human = env.ok(&["board"]);
    assert_eq!(env.event_count(), before, "board writes no events");

    let projects = board["projects"].as_array().unwrap();
    let shop = projects.iter().find(|p| p["project"] == "shop").unwrap();
    assert_eq!(shop["holds"][0]["agent"], "claude@shop");
    assert_eq!(shop["holds"][0]["pattern"], "src/a.rs");
    assert_eq!(shop["holds"][0]["edit_hook"], true);
    assert_eq!(shop["edits"][0]["path"], "src/a.rs");
    assert_eq!(shop["blocked"][0]["agent"], "codex@shop-review");
    assert_eq!(shop["blocked"][0]["holder"], "claude@shop");
    let review = projects
        .iter()
        .find(|p| p["project"] == "shop-review")
        .expect("sibling checkout is on the board for --project shop");
    assert_eq!(review["holds"][0]["agent"], "codex@shop-review");
    assert_eq!(review["holds"][0]["pattern"], "src/a.rs");
    assert!(review["blocked"].as_array().unwrap().is_empty());

    let codex = board["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["address"] == "codex@shop-review")
        .unwrap();
    assert_eq!(codex["unread"], 1);
    assert_eq!(codex["awaiting_ack"], 1);
    assert_eq!(codex["requests"], 1);
    assert_eq!(board["requests"][0]["title"], "Review src/a.rs");
    assert!(!board.to_string().contains("secret body"));

    for want in [
        "shop  ",
        "claude@shop  src/a.rs  exclusive",
        "(edit hook)",
        "recent edits (paths the edit hook claimed)",
        "stopped edits",
        "codex@shop-review on src/a.rs, held by claude@shop (shop:src/a.rs)",
        "\nAgents\n",
        "codex@shop-review  live, seen ",
        " ago: 1 unread, 1 to acknowledge, 1 request(s)",
        "Pending requests",
        "open  Review src/a.rs  from claude@shop to codex@shop-review",
    ] {
        assert!(human.contains(want), "missing {want:?} in:\n{human}");
    }
    assert!(!human.contains('\u{2014}'), "no em dashes in copy");

    // A project path works like its slug.
    let by_path = env.json(&["board", "-p", env.main.to_str().unwrap()]);
    assert_eq!(by_path["projects"].as_array().unwrap().len(), 2);

    let bad = env.run(&["board", "--since", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not an event id"));
}

#[test]
fn an_empty_board_says_so() {
    let env = Env::new();
    env.ok(&["init"]);
    let human = env.ok(&["board"]);
    assert!(human.contains("No live holds and no edits in this window."));
    assert!(human.contains("none with holds, unread mail, acknowledgements or requests"));
}

#[test]
fn board_opens_wal_mode_without_sidecars() {
    let env = Env::new();
    env.start("claude", &env.main);
    assert_eq!(env.edit("claude", &env.main, "src/a.rs"), "");
    for suffix in ["-wal", "-shm"] {
        assert!(!env
            .another_home
            .join(format!("{}{suffix}", another_core::DB_FILE))
            .exists());
    }

    let board = env.json(&["board"]);
    assert_eq!(board["projects"][0]["holds"][0]["pattern"], "src/a.rs");
}

#[cfg(unix)]
#[test]
fn board_opens_a_read_only_rollback_database() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    env.start("claude", &env.main);
    assert_eq!(env.edit("claude", &env.main, "src/a.rs"), "");
    let path = env.another_home.join(another_core::DB_FILE);
    let db = rusqlite::Connection::open(&path).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    drop(db);
    let before = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

    let board = env.json(&["board"]);
    assert_eq!(board["projects"][0]["holds"][0]["pattern"], "src/a.rs");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
