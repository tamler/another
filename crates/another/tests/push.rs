//! Mid-task push and enforced holds, through the real binary: the post-tool and pre-edit
//! hooks, failing open when the database is missing or locked, Herdr target capture,
//! `wake prune`, and the daemon's waker against a fake `herdr`.
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    another_home: PathBuf,
    /// Main checkout of a scratch repository with an origin.
    main: PathBuf,
    /// A linked worktree of the same repository.
    wt: PathBuf,
    /// Fake herdr: state files under `panes/`, calls appended to `calls.log`.
    herdr_dir: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const FAKE_HERDR: &str = r#"#!/bin/sh
dir='@DIR@'
printf '%s\n' "$*" >> "$dir/calls.log"
case "$1 $2" in
  "agent get")
    if [ -f "$dir/panes/$3" ]; then
      read st prog < "$dir/panes/$3"
      printf '{"result":{"agent":{"agent":"%s","agent_status":"%s","pane_id":"%s"}}}\n' "$prog" "$st" "$3"
      exit 0
    fi
    printf '{"error":{"code":"agent_not_found","message":"agent target %s not found"}}\n' "$3"
    exit 1 ;;
  "agent prompt")
    printf '{"result":{"type":"ok"}}\n'
    exit 0 ;;
esac
exit 2
"#;

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
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
        let herdr_dir = root.join("herdr");
        std::fs::create_dir_all(herdr_dir.join("panes")).unwrap();
        let script = herdr_dir.join("herdr");
        std::fs::write(
            &script,
            FAKE_HERDR.replace("@DIR@", &herdr_dir.to_string_lossy()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = root.join("home");
        Env {
            another_home: home.join(".another"),
            home,
            root,
            main,
            wt,
            herdr_dir,
            _dir: dir,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env("HERDR_BIN_PATH", self.herdr_dir.join("herdr"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("TERM_PROGRAM");
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).stdin(Stdio::null()).output().unwrap();
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

    /// Run a hook like a client would; returns stdout and how long it took.
    fn hook_timed(
        &self,
        event: &str,
        client: &str,
        stdin: &Value,
        extra_env: &[(&str, &str)],
    ) -> (String, Duration) {
        let mut cmd = self.cmd(&["hook", "run", event, "--client", client]);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let started = Instant::now();
        let mut child = cmd
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
        let out: Output = child.wait_with_output().unwrap();
        let took = started.elapsed();
        assert!(out.status.success(), "hook run must always exit 0");
        (String::from_utf8(out.stdout).unwrap(), took)
    }

    fn hook(&self, event: &str, client: &str, stdin: &Value) -> String {
        self.hook_timed(event, client, stdin, &[]).0
    }

    fn start(&self, client: &str, dir: &Path, extra_env: &[(&str, &str)]) -> String {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook_timed("session-start", client, &stdin, extra_env)
            .0
    }

    fn send(&self, from: &str, to: &str, subject: &str, request: bool) {
        let mut args = vec![
            "send",
            "--from",
            from,
            "--to",
            to,
            "--subject",
            subject,
            "--body",
            "secret body",
        ];
        if request {
            args.push("--request");
        }
        self.ok(&args);
    }

    fn holds(&self) -> Vec<Value> {
        self.json(&["holds"]).as_array().unwrap().clone()
    }

    fn set_pane(&self, pane: &str, state: Option<&str>) {
        let path = self.herdr_dir.join("panes").join(pane);
        match state {
            Some(s) => std::fs::write(path, format!("{s}\n")).unwrap(),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn herdr_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.herdr_dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn tool(dir: &Path, name: &str, input: Value) -> Value {
    json!({
        "session_id": "s1",
        "cwd": dir,
        "hook_event_name": "PreToolUse",
        "tool_name": name,
        "tool_input": input,
    })
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

fn denial(out: &str) -> String {
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn post_tool_mentions_new_mail_once_and_is_silent_otherwise() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let after_bash = tool(&env.main, "Bash", json!({ "command": "ls" }));

    assert_eq!(
        env.hook("post-tool", "claude", &after_bash),
        "",
        "nothing new"
    );

    env.send(
        "codex@shop-review",
        "claude@shop",
        "Release src/a.rs?",
        true,
    );
    let notice = context(&env.hook("post-tool", "claude", &after_bash));
    assert!(
        notice.contains("1 new message(s) for claude@shop (0 urgent)"),
        "{notice}"
    );
    assert!(notice.contains("\"Release src/a.rs?\" from codex@shop-review"));
    assert!(notice.contains("opens request"), "{notice}");
    assert!(!notice.contains("secret body"), "bodies never appear");

    // Announced once, whichever hook sees it first.
    assert_eq!(env.hook("post-tool", "claude", &after_bash), "");
    assert_eq!(env.hook("prompt", "claude", &after_bash), "");

    // Cursor gets its own context format.
    env.start("cursor", &env.main, &[]);
    env.send("codex@shop-review", "cursor@shop", "Docs", false);
    let out = env.hook("post-tool", "cursor", &after_bash);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(v["additional_context"].as_str().unwrap().contains("Docs"));

    // A project nobody registered: nothing printed, nothing registered.
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join(".git")).unwrap();
    let out = env.hook("post-tool", "claude", &tool(&elsewhere, "Bash", json!({})));
    assert_eq!(out, "");
    let projects = env.json(&["project", "list"]);
    assert!(!projects.to_string().contains("elsewhere"));

    // Both notices were recorded with their message ids; reading one counts as acting on it.
    let unread = env.json(&["inbox", "--agent", "claude@shop", "--unread"]);
    let id = unread["messages"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["ack", "--agent", "claude@shop", &id]);
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["notices_mid_task"], 2, "{stats}");
    assert_eq!(stats["notices_prompt"], 0);
    assert_eq!(stats["messages_pushed"], 2);
    assert_eq!(stats["pushes_acted_on"], 1);
    assert_eq!(stats["requests_created"], 1);
}

#[test]
fn pre_edit_holds_files_and_stops_edits_in_sibling_worktrees() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let a = env.main.join("src/a.rs");

    // Claude edits: allowed silently, and the file is now held.
    let out = env.hook(
        "pre-edit",
        "claude",
        &tool(
            &env.main,
            "Edit",
            json!({ "file_path": a, "old_string": "x", "new_string": "y" }),
        ),
    );
    assert_eq!(out, "", "an allowed edit prints nothing");
    let holds = env.holds();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0]["agent"], "claude@shop");
    assert_eq!(holds[0]["pattern"], "src/a.rs");
    assert_eq!(holds[0]["exclusive"], true);
    assert!(holds[0]["reason"].as_str().unwrap().contains("edit hook"));

    // Codex, in a sibling worktree of the same repository, patches the same file: stopped,
    // with the holder named and a way to ask.
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch\n";
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch })),
    );
    let reason = denial(&out);
    assert!(reason.contains("held by claude@shop"), "{reason}");
    assert!(
        reason.starts_with("Another stopped this edit: src/a.rs is held by"),
        "paths are project-relative: {reason}"
    );
    assert!(
        reason.contains("--subject \"Release src/a.rs?\""),
        "{reason}"
    );
    assert!(reason.contains("shop:src/a.rs"), "{reason}");
    assert!(
        reason.contains("another send --from codex@shop-review --to claude@shop --request"),
        "{reason}"
    );
    assert!(!reason.contains('\u{2014}'), "no em dashes in copy");

    // A different file is fine, and gets its own hold in the worktree's project.
    let patch = "*** Begin Patch\n*** Add File: src/b.rs\n+fn b() {}\n*** End Patch\n";
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch })),
    );
    assert_eq!(out, "");
    assert!(env
        .holds()
        .iter()
        .any(|h| h["agent"] == "codex@shop-review" && h["pattern"] == "src/b.rs"));

    // Relative paths resolve against cwd; Claude is now the one stopped.
    let out = env.hook(
        "pre-edit",
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": "src/b.rs", "content": "" }),
        ),
    );
    assert!(denial(&out).contains("held by codex@shop-review"));

    // Notebooks, files outside any project, and tools without a path.
    let out = env.hook(
        "pre-edit",
        "claude",
        &tool(
            &env.main,
            "NotebookEdit",
            json!({ "notebook_path": env.main.join("notes/x.ipynb") }),
        ),
    );
    assert_eq!(out, "");
    let scratch = env.root.join("scratch/x.txt");
    let out = env.hook(
        "pre-edit",
        "claude",
        &tool(&env.main, "Write", json!({ "file_path": scratch })),
    );
    assert_eq!(out, "");
    assert_eq!(
        env.hook("pre-edit", "claude", &tool(&env.main, "Edit", json!({}))),
        ""
    );
    let patterns: Vec<String> = env
        .holds()
        .iter()
        .map(|h| h["pattern"].as_str().unwrap().to_string())
        .collect();
    assert!(patterns.contains(&"notes/x.ipynb".to_string()));
    assert!(
        !patterns.iter().any(|p| p.contains("x.txt")),
        "{patterns:?}"
    );

    // Editing a file one already holds writes nothing new.
    let events_before = env
        .json(&["events", "--limit", "1000"])
        .as_array()
        .unwrap()
        .len();
    env.hook(
        "pre-edit",
        "claude",
        &tool(&env.main, "Edit", json!({ "file_path": a })),
    );
    let events_after = env
        .json(&["events", "--limit", "1000"])
        .as_array()
        .unwrap()
        .len();
    assert_eq!(events_before, events_after);

    // Session end releases Claude's holds; Codex may edit src/a.rs now.
    env.hook(
        "session-end",
        "claude",
        &json!({ "session_id": "claude-s1", "cwd": env.main }),
    );
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch\n";
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch })),
    );
    assert_eq!(out, "");

    // Every stop and every automatic hold is countable afterwards.
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["edits_blocked"], 2, "{stats}");
    assert_eq!(stats["collisions"], 2);
    assert_eq!(
        stats["holds_auto_acquired"], 4,
        "a.rs, b.rs, x.ipynb, then a.rs again"
    );
    let first = &stats["blocked"][1];
    assert_eq!(first["agent"], "codex@shop-review");
    assert_eq!(first["holder"], "claude@shop");
    assert_eq!(first["path"], "src/a.rs");
    assert_eq!(first["project"], "shop-review");
    assert_eq!(first["holder_project"], "shop");
    let human = env.ok(&["stats", "--since", "1h"]);
    assert!(human.contains("edits stopped by a hold"), "{human}");
    assert!(
        human.contains("codex@shop-review on shop-review:src/a.rs, held by claude@shop"),
        "{human}"
    );
}

#[test]
fn tool_hooks_fail_open_fast_when_the_database_is_missing_or_locked() {
    let env = Env::new();
    let edit = tool(
        &env.main,
        "Edit",
        json!({ "file_path": env.main.join("src/a.rs") }),
    );
    // No database yet: silent, quick, and nothing is created.
    let (out, took) = env.hook_timed("post-tool", "claude", &edit, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let (out, _) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert_eq!(out, "");
    assert!(!env.another_home.join("another.db").exists());

    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    env.send("codex@shop-review", "claude@shop", "While locked", false);

    // A writer holding the lock does not stop readers: the notice still arrives.
    let db = rusqlite::Connection::open(env.another_home.join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let after = tool(&env.main, "Bash", json!({}));
    let (out, took) = env.hook_timed("post-tool", "claude", &after, &[]);
    assert!(context(&out).contains("While locked"));
    assert!(took < Duration::from_secs(2), "{took:?}");
    // The edit guard cannot write: it lets the edit through and logs why.
    let (out, took) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(3), "{took:?}");
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);

    // A connection that takes the whole database exclusively blocks even reads.
    let db = rusqlite::Connection::open(env.another_home.join("another.db")).unwrap();
    db.pragma_update(None, "locking_mode", "EXCLUSIVE").unwrap();
    db.execute_batch("BEGIN EXCLUSIVE; UPDATE nodes SET display_name = display_name;")
        .unwrap();
    env.send_while_locked_is_impossible();
    let (out, took) = env.hook_timed("post-tool", "claude", &after, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let (out, took) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(3), "{took:?}");
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);

    let log = std::fs::read_to_string(env.another_home.join("logs/hooks.log")).unwrap();
    assert!(log.contains("pre-edit --client claude"), "{log}");
    assert!(log.contains("failing open"), "{log}");
}

impl Env {
    /// Documents the lock above: the CLI cannot write either while it is held.
    fn send_while_locked_is_impossible(&self) {
        let out = self
            .cmd(&["agent", "list"])
            .env("ANOTHER_LOG", "off")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "the database should be locked");
    }
}

#[test]
fn herdr_panes_are_captured_at_start_and_released_at_end() {
    let env = Env::new();
    let in_herdr = [("HERDR_PANE_ID", "w7:p2"), ("TERM_PROGRAM", "herdr")];
    env.start("claude", &env.main, &in_herdr);
    let targets = env.json(&["wake", "list"]);
    assert_eq!(targets[0]["agent"], "claude@shop");
    assert_eq!(targets[0]["via"], "herdr");
    assert_eq!(targets[0]["target"], "w7:p2");
    assert_eq!(targets[0]["auto"], true);

    // tmux running inside a Herdr pane: tmux is the innermost terminal.
    env.start(
        "codex",
        &env.wt,
        &[
            ("HERDR_PANE_ID", "w7:p3"),
            ("TMUX_PANE", "%4"),
            ("TERM_PROGRAM", "tmux"),
        ],
    );
    let targets = env.json(&["wake", "list"]);
    assert!(targets
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["agent"] == "codex@shop-review" && t["via"] == "tmux" && t["target"] == "%4"));

    // A session ending in another pane leaves the capture alone; ending in this one clears it.
    let end = json!({ "session_id": "claude-s1", "cwd": env.main });
    env.hook_timed("session-end", "claude", &end, &[("HERDR_PANE_ID", "w9:p9")]);
    assert_eq!(env.json(&["wake", "list"]).as_array().unwrap().len(), 2);
    env.hook_timed("session-end", "claude", &end, &in_herdr);
    let targets = env.json(&["wake", "list"]);
    assert!(!targets.to_string().contains("claude@shop"), "{targets}");
}

#[test]
fn wake_prune_drops_dead_automatic_targets_only() {
    let env = Env::new();
    env.start("claude", &env.main, &[("HERDR_PANE_ID", "w1:p1")]);
    env.start("codex", &env.wt, &[("HERDR_PANE_ID", "w1:p2")]);
    env.ok(&["agent", "register", "bob", "--program", "other"]);
    env.ok(&[
        "wake",
        "set",
        "--agent",
        "bob",
        "--via",
        "herdr",
        "--target",
        "gone-pane",
    ]);
    env.set_pane("w1:p2", Some("idle codex"));

    let dry = env.json(&["wake", "prune", "--dry-run"]);
    let row = |agent: &str| {
        dry.as_array()
            .unwrap()
            .iter()
            .find(|r| r["agent"] == agent)
            .unwrap()
            .clone()
    };
    assert_eq!(row("claude@shop")["pruned"], true);
    assert!(row("claude@shop")["reason"]
        .as_str()
        .unwrap()
        .contains("gone"));
    assert_eq!(row("codex@shop-review")["pruned"], false);
    assert_eq!(row("bob")["pruned"], false, "targets set by hand stay");
    assert_eq!(env.json(&["wake", "list"]).as_array().unwrap().len(), 3);

    env.ok(&["wake", "prune"]);
    let left: Vec<String> = env
        .json(&["wake", "list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["agent"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, ["bob", "codex@shop-review"]);
    // Probes only ever ask; nothing is focused or typed.
    assert!(env
        .herdr_calls()
        .iter()
        .all(|c| c.starts_with("agent get ")));
}

/// `another serve` running in the background with push on; stopped with SIGTERM.
struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn start(env: &Env) -> Daemon {
        let log = env.root.join("serve.log");
        let file = std::fs::File::create(&log).unwrap();
        let child = env
            .cmd(&[
                "serve",
                "--bind",
                "127.0.0.1:0",
                "--no-dashboard",
                "--wake-secs",
                "1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap();
        let d = Daemon { child, log };
        d.wait_log("push delivery on", Duration::from_secs(20));
        d
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&self, needle: &str, limit: Duration) {
        let started = Instant::now();
        while !self.text().contains(needle) {
            assert!(
                started.elapsed() < limit,
                "daemon log never showed {needle:?}:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// SIGTERM, as launchd sends; the daemon must exit by itself.
    fn stop(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(sent.success());
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "daemon ignored SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

#[test]
fn daemon_wakes_idle_herdr_agents_and_leaves_working_ones_to_hooks() {
    let env = Env::new();
    env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    env.start("codex", &env.wt, &[]);
    env.set_pane("w7:p2", Some("working claude"));
    let daemon = Daemon::start(&env);

    // Working: the daemon looks, but leaves delivery to the post-tool hook.
    env.send("codex@shop-review", "claude@shop", "Rebase please", false);
    assert!(wait_for(Duration::from_secs(15), || env
        .herdr_calls()
        .iter()
        .any(|c| c == "agent get w7:p2")));
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !env.herdr_calls()
            .iter()
            .any(|c| c.starts_with("agent prompt")),
        "{:?}",
        env.herdr_calls()
    );

    // Idle: one prompt with the inbox summary (subjects, never bodies).
    env.set_pane("w7:p2", Some("idle claude"));
    assert!(
        wait_for(Duration::from_secs(20), || env
            .herdr_calls()
            .iter()
            .any(|c| c.starts_with("agent prompt w7:p2 "))),
        "no wake: {:?}\n{}",
        env.herdr_calls(),
        daemon.text()
    );
    let prompt = env
        .herdr_calls()
        .into_iter()
        .find(|c| c.starts_with("agent prompt"))
        .unwrap();
    assert!(prompt.contains("Rebase please"), "{prompt}");
    assert!(!prompt.contains("secret body"));

    // The wake counts as the announcement: the next tool hook stays quiet.
    let after = tool(&env.main, "Bash", json!({}));
    assert_eq!(env.hook("post-tool", "claude", &after), "");

    // More mail right away does not cause a second prompt (debounced).
    env.send("codex@shop-review", "claude@shop", "One more", false);
    std::thread::sleep(Duration::from_secs(4));
    let prompts = env
        .herdr_calls()
        .iter()
        .filter(|c| c.starts_with("agent prompt"))
        .count();
    assert_eq!(prompts, 1);

    // Herdr is only ever called with a subcommand.
    assert!(env
        .herdr_calls()
        .iter()
        .all(|c| c.starts_with("agent get ") || c.starts_with("agent prompt ")));
    daemon.wait_log("action=woke", Duration::from_secs(5));
    let status = daemon.stop();
    assert!(status.success(), "clean exit on SIGTERM: {status:?}");

    // The wake is countable: agent, pane and outcome.
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["wakes_sent"], 1, "{stats}");
    assert_eq!(stats["wakes_failed"], 0);
    assert_eq!(stats["messages_pushed"], 1);
    assert_eq!(stats["wakes"][0]["agent"], "claude@shop");
    assert_eq!(stats["wakes"][0]["target"], "w7:p2");
    assert_eq!(stats["wakes"][0]["outcome"], "sent");
}
