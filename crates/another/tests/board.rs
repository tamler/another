//! `another board`: the read-only work board, driven through the real binary and the real
//! edit hook.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_BIN_PATH")
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
}

#[test]
fn board_shows_what_the_edit_hook_recorded_and_what_waits() {
    let env = Env::new();
    env.start("claude", &env.main);
    env.start("codex", &env.wt);
    assert_eq!(env.edit("claude", &env.main, "src/a.rs"), "");
    // Codex in a sibling worktree is stopped on the same file.
    let stopped = env.edit("codex", &env.wt, "src/a.rs");
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
    let review = projects
        .iter()
        .find(|p| p["project"] == "shop-review")
        .expect("sibling checkout is on the board for --project shop");
    assert_eq!(review["blocked"][0]["agent"], "codex@shop-review");
    assert_eq!(review["blocked"][0]["holder"], "claude@shop");

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
