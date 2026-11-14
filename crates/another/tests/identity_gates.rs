//! Session identity safety gates through the real binary: no hook event creates a home,
//! a stalled launcher cannot stall a hook, refusals leave a home untouched, edit admission
//! is all-or-nothing, and an unconfirmed identity commit fences every writer.
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A git checkout and a user home; the Another home is not created.
    fn bare() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join("src").join(name), "before").unwrap();
        }
        let root = std::fs::canonicalize(root).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Fixture {
            _dir: dir,
            root,
            home,
        }
    }

    fn initialized() -> Fixture {
        let fixture = Fixture::bare();
        assert!(fixture.run(&["init"]).status.success());
        fixture
    }

    fn another_home(&self) -> PathBuf {
        self.home.join(".another")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", self.another_home())
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn hook(&self, event: &str, client: &str, input: &Value) -> String {
        let mut child = self
            .command(&["hook", "run", event, "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook run must always exit 0: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, session: &str) -> String {
        let out = self.hook(
            "session-start",
            "claude",
            &json!({"session_id": session, "cwd": self.root}),
        );
        out.strip_prefix("Another: ")
            .unwrap_or_else(|| panic!("session did not start: {out}"))
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    fn edit(&self, session: &str, files: &[&Path]) -> String {
        let input = match files {
            [one] => json!({"file_path": one}),
            many => {
                let mut patch = String::from("*** Begin Patch\n");
                for file in many {
                    patch.push_str(&format!("*** Update File: {}\n", file.display()));
                }
                patch.push_str("*** End Patch\n");
                json!({"command": patch})
            }
        };
        self.hook(
            "pre-native-edit",
            "claude",
            &json!({"session_id": session, "cwd": self.root, "hook_event_name": "PreToolUse",
                "tool_name": "Edit", "tool_input": input}),
        )
    }

    fn controlled(&self, session: &str, path: &Path, call: &str) -> String {
        self.hook("pre-controlled-edit","claude",&json!({"session_id":session,"cwd":self.root,
            "tool_name":"mcp__another__controlled_edit","tool_use_id":call,
            "tool_input":{"project":self.root,"path":path.strip_prefix(&self.root).unwrap_or(path),
                "expected_sha256":another_core::controlled_sha256(b"before"),"replacement":"after"}}))
    }

    fn holds(&self) -> Vec<String> {
        let mut holds: Vec<String> = self
            .json(&["holds"])
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                format!(
                    "{} {}",
                    h["agent"].as_str().unwrap(),
                    h["pattern"].as_str().unwrap()
                )
            })
            .collect();
        holds.sort();
        holds
    }

    fn fence(&self, name: &str) -> PathBuf {
        let dir = self.another_home().join("identity-ops");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"session.binding.v1\n").unwrap();
        path
    }
}

fn deny_reason(out: &str) -> Option<String> {
    let reply: Value = serde_json::from_str(out).ok()?;
    let decision = &reply["hookSpecificOutput"];
    (decision["permissionDecision"] == "deny").then(|| {
        decision["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_owned()
    })
}

/// Everything under `dir`, with sizes and contents, to prove a run changed nothing.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&next)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                out.push((path.clone(), Vec::new()));
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn no_hook_event_creates_a_home() {
    let fixture = Fixture::bare();
    let edit = json!({"session_id": "s1", "cwd": fixture.root, "hook_event_name": "PreToolUse",
        "tool_name": "Edit", "tool_input": {"file_path": fixture.root.join("src/a.rs")}});
    for client in ["claude", "codex", "cursor", "grok"] {
        for event in [
            "session-start",
            "prompt",
            "session-end",
            "post-tool",
            "pre-native-edit",
        ] {
            let out = fixture.hook(event, client, &edit);
            assert!(
                !fixture.another_home().exists(),
                "{event} for {client} created the home"
            );
            match event {
                // An edit that cannot be protected is stopped, and says how to fix it.
                "pre-native-edit" => {
                    let reason = deny_reason(&out).unwrap_or_else(|| panic!("{client}: {out}"));
                    assert!(reason.contains("another init"), "{reason}");
                }
                // A session that cannot start says so where the client shows it.
                "session-start" => assert!(out.contains("another init"), "{client}: {out}"),
                "post-tool" | "session-end" => assert_eq!(out, "", "{client} {event}"),
                _ => {}
            }
        }
    }

    // A home directory that exists without a database is left exactly as it was.
    std::fs::create_dir(fixture.another_home()).unwrap();
    for event in [
        "session-start",
        "prompt",
        "session-end",
        "post-tool",
        "pre-native-edit",
    ] {
        fixture.hook(event, "claude", &edit);
        assert_eq!(
            std::fs::read_dir(fixture.another_home()).unwrap().count(),
            0,
            "{event} wrote into an uninitialized home"
        );
    }
}

#[test]
fn a_stalled_launcher_cannot_stall_a_hook() {
    let fixture = Fixture::initialized();
    for (event, limit) in [("pre-native-edit", 4), ("post-tool", 4), ("prompt", 7)] {
        let mut child = fixture
            .command(&["hook", "run", event, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // A partial envelope on a pipe that stays open.
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"{").unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(limit);
        let exited = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Closing our own pipe lets a child that did wait finish without a signal.
        drop(input);
        let out = child.wait_with_output().unwrap();
        assert!(exited, "{event} waited for stdin to close");
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        match event {
            "pre-native-edit" => assert!(deny_reason(&text).is_some(), "{text}"),
            // A launcher fault is reported wherever the client shows context.
            _ => assert!(text.contains("did not receive"), "{text}"),
        }
    }
    assert!(fixture
        .json(&["agent", "list"])
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn refusals_leave_an_existing_home_untouched() {
    // Not a database at all.
    let fixture = Fixture::bare();
    std::fs::create_dir(fixture.another_home()).unwrap();
    std::fs::write(fixture.another_home().join("another.db"), b"sentinel").unwrap();
    let before = snapshot(&fixture.another_home());
    for client in ["claude", "codex", "cursor", "grok"] {
        for event in ["session-start", "prompt", "session-end", "post-tool"] {
            fixture.hook(
                event,
                client,
                &json!({"session_id": "s1", "cwd": fixture.root}),
            );
        }
        let out = fixture.edit("s1", &[&fixture.root.join("src/a.rs")]);
        assert!(deny_reason(&out).is_some(), "{client}: {out}");
        assert_eq!(snapshot(&fixture.another_home()), before, "{client}");
    }

    // An initialized home, and claims that fail before any store is opened.
    let fixture = Fixture::initialized();
    fixture.start("s1");
    let before = snapshot(&fixture.another_home());
    let invalid = [
        json!({"cwd": fixture.root, "tool_input": {"file_path": fixture.root.join("src/a.rs")}}),
        json!({"session_id": "", "cwd": fixture.root}),
        json!({"session_id": "a\u{7}b", "cwd": fixture.root}),
        json!("garbage"),
    ];
    for input in &invalid {
        for event in ["session-start", "prompt", "session-end", "post-tool"] {
            fixture.hook(event, "claude", input);
        }
        let out = fixture.hook("pre-native-edit", "claude", input);
        assert!(deny_reason(&out).is_some(), "{input}: {out}");
    }
    // A provenance pair that is half set fails the same gate.
    let mut child = fixture
        .command(&["hook", "run", "pre-native-edit", "--client", "claude"])
        .env("ANOTHER_AGENT", "someone")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id": "s1", "cwd": fixture.root,
                "tool_input": {"file_path": fixture.root.join("src/a.rs")}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(deny_reason(&String::from_utf8(out.stdout).unwrap()).is_some());
    assert_eq!(snapshot(&fixture.another_home()), before);
}

#[test]
fn an_edit_is_held_completely_or_not_at_all() {
    let fixture = Fixture::initialized();
    let alice = fixture.start("alice");
    let bob = fixture.start("bob");
    let a = fixture.root.join("src/a.rs");
    let b = fixture.root.join("src/b.rs");
    let c = fixture.root.join("src/c.rs");

    assert_eq!(fixture.controlled("bob", &b, "bob-b"), "");
    assert_eq!(fixture.holds(), [format!("{bob} src/b.rs")]);

    // Alice's patch touches a free file and Bob's file: stopped, and the free file is
    // not left held.
    let out = fixture.edit("alice", &[&a, &b, &c]);
    let reason = deny_reason(&out).unwrap_or_else(|| panic!("{out}"));
    assert!(reason.contains("native"), "{reason}");
    assert_eq!(fixture.holds(), [format!("{bob} src/b.rs")]);

    // Multi-file native edits stay denied. Each free file needs its own controlled call.
    assert!(deny_reason(&fixture.edit("alice", &[&a, &c])).is_some());
    assert_eq!(fixture.controlled("alice", &a, "alice-a"), "");
    assert_eq!(fixture.controlled("alice", &c, "alice-c"), "");
    let mut expected = [
        format!("{alice} src/a.rs"),
        format!("{alice} src/c.rs"),
        format!("{bob} src/b.rs"),
    ];
    expected.sort();
    assert_eq!(fixture.holds(), expected);

    // A session that never started is stopped and registers nothing.
    let agents = fixture.json(&["agent", "list"]);
    let out = fixture.controlled("never-started", &a, "never-a");
    assert!(deny_reason(&out).unwrap().contains("session"), "{out}");
    assert_eq!(fixture.json(&["agent", "list"]), agents);
}

#[test]
fn an_edit_never_registers_a_project() {
    let fixture = Fixture::initialized();
    let alice = fixture.start("alice");
    let nested = fixture.root.join("vendor/lib");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(nested.join("lib.rs"), "before").unwrap();
    let projects = fixture.json(&["project", "list"]);

    // One file in the session's project, one in a checkout nobody registered.
    let out = fixture.edit(
        "alice",
        &[&fixture.root.join("src/a.rs"), &nested.join("lib.rs")],
    );
    let reason = deny_reason(&out).unwrap_or_else(|| panic!("{out}"));
    assert!(reason.contains("native"), "{reason}");
    assert_eq!(fixture.json(&["project", "list"]), projects);
    assert!(fixture.holds().is_empty());

    // Registration does not turn a native or cross-project edit into a controlled call.
    assert!(fixture
        .run(&["project", "ensure", nested.to_str().unwrap()])
        .status
        .success());
    let out = fixture.edit(
        "alice",
        &[&fixture.root.join("src/a.rs"), &nested.join("lib.rs")],
    );
    assert!(deny_reason(&out).is_some());
    assert!(
        deny_reason(&fixture.controlled("alice", &nested.join("lib.rs"), "nested-call")).is_some()
    );
    assert!(fixture.holds().is_empty());
    assert_eq!(fixture.start("alice"), alice);
}

#[test]
fn an_unconfirmed_identity_commit_fences_every_writer() {
    let fixture = Fixture::initialized();
    let alice = fixture.start("alice");
    let a = fixture.root.join("src/a.rs");

    // A pending operation is settled by whichever writer comes next: here an edit.
    let pending = fixture.fence("01HZZZZZZZZZZZZZZZZZZZZZZZ.pending");
    assert_eq!(fixture.controlled("alice", &a, "fence-a"), "");
    assert!(!pending.exists());
    assert_eq!(fixture.holds(), [format!("{alice} src/a.rs")]);

    // A fence this build cannot interpret stops every writer, hook or not, and stays.
    let stray = fixture.fence("unreadable");
    let events = fixture.json(&["events", "--limit", "500"]);
    let out = fixture.controlled("alice", &fixture.root.join("src/b.rs"), "fence-b");
    assert!(deny_reason(&out).unwrap().contains("fence"), "{out}");
    let out = fixture.hook(
        "session-start",
        "claude",
        &json!({"session_id": "bob", "cwd": fixture.root}),
    );
    assert!(
        out.contains("fence") && !out.starts_with("Another: "),
        "{out}"
    );
    for args in [
        vec!["project", "ensure", fixture.home.to_str().unwrap()],
        vec!["send", "--from", &alice, "--to", &alice, "--subject", "x"],
        vec!["hold", "acquire", "--agent", &alice, "src/z.rs"],
    ] {
        let out = fixture.run(&args);
        assert!(!out.status.success(), "{args:?} wrote through the fence");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("fence"),
            "{args:?}: {out:?}"
        );
    }
    assert!(stray.exists());
    // Reads still work, and nothing was written.
    assert_eq!(fixture.json(&["events", "--limit", "500"]), events);

    // Cleared by the operator, writers proceed from exactly the earlier state.
    std::fs::remove_file(&stray).unwrap();
    assert_eq!(fixture.holds(), [format!("{alice} src/a.rs")]);
    assert_eq!(
        fixture.controlled("alice", &fixture.root.join("src/b.rs"), "after-fence-b"),
        ""
    );
}

#[test]
fn session_exec_never_creates_a_home_or_runs_unidentified() {
    let fixture = Fixture::bare();
    let marker = fixture.home.join("child-ran");
    let exec = |agent: &str| {
        fixture.run(&[
            "session",
            "exec",
            "--agent",
            agent,
            "--",
            "/usr/bin/touch",
            marker.to_str().unwrap(),
        ])
    };
    assert!(!exec("fixture").status.success());
    assert!(
        !fixture.another_home().exists(),
        "session exec created a home"
    );
    assert!(!marker.exists(), "session exec launched its child");

    // A foreign database with a live WAL: refused, no child, data and files preserved.
    std::fs::create_dir(fixture.another_home()).unwrap();
    let db_path = fixture.another_home().join("another.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE marker(value TEXT); INSERT INTO marker VALUES ('preserve');",
    )
    .unwrap();
    let wal_path = fixture.another_home().join("another.db-wal");
    let before = (
        std::fs::read(&db_path).unwrap(),
        std::fs::read(&wal_path).unwrap(),
    );
    assert!(!exec("fixture").status.success());
    assert!(!marker.exists());
    assert_eq!(
        (
            std::fs::read(&db_path).unwrap(),
            std::fs::read(&wal_path).unwrap()
        ),
        before
    );
    assert_eq!(
        std::fs::read_dir(fixture.another_home()).unwrap().count(),
        3
    );
    let value: String = db
        .query_row("SELECT value FROM marker", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "preserve");
    drop(db);

    // With a started session, the exact address runs the child; an unknown one does not.
    let fixture = Fixture::initialized();
    let alice = fixture.start("alice");
    let marker = fixture.home.join("child-ran");
    let exec = |agent: &str| {
        fixture.run(&[
            "session",
            "exec",
            "--agent",
            agent,
            "--",
            "/usr/bin/touch",
            marker.to_str().unwrap(),
        ])
    };
    assert!(!exec("nobody@shop").status.success());
    assert!(!marker.exists());
    assert!(exec(&alice).status.success());
    assert!(marker.exists());
}

/// Round-1 review reproduction: a second edit of a held file must not start on the old
/// hold's remaining time.
#[test]
fn a_repeated_edit_gets_fresh_coverage() {
    let fixture = Fixture::initialized();
    fixture.start("alice");
    let a = fixture.root.join("src/a.rs");
    assert_eq!(fixture.controlled("alice", &a, "fresh-a"), "");

    let db = rusqlite::Connection::open(fixture.another_home().join("another.db")).unwrap();
    let hold = |db: &rusqlite::Connection| -> (String, i64, i64) {
        db.query_row(
            "SELECT hold_id, expires_at, ttl_seconds FROM holds WHERE released_at IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let (id, _, ttl) = hold(&db);
    assert_eq!(ttl, 1800);
    // 901 seconds left: more than half the TTL, so the old rule left it alone.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    db.execute(
        "UPDATE holds SET expires_at = ?1,created_at=?1-ttl_seconds*1000,renewed_at=?1-ttl_seconds*1000 WHERE hold_id = ?2",
        rusqlite::params![now + 901_000, id],
    )
    .unwrap();
    db.execute("UPDATE admissions SET created_at=(SELECT created_at FROM holds WHERE hold_id=?1) WHERE admission_id=(SELECT admission_id FROM admission_holds WHERE hold_id=?1)",[&id]).unwrap();
    assert_eq!(fixture.controlled("alice", &a, "fresh-b"), "");
    let (new_id, expires_at): (String, i64) = db
        .query_row(
            "SELECT hold_id,expires_at FROM holds WHERE hold_id!=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_ne!(new_id, id, "a new operation gets a distinct pending hold");
    assert_eq!(
        db.query_row(
            "SELECT expires_at FROM holds WHERE hold_id=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        now + 901_000
    );
    assert!(
        expires_at >= now + 1_800_000,
        "the hold has {} s left after a fresh admission",
        (expires_at - now) / 1000
    );
}

/// Round-1 review reproduction: one patch with a project file and a file outside any
/// project must not be reported as protected.
#[test]
fn a_partly_guarded_edit_is_stopped() {
    let fixture = Fixture::initialized();
    let alice = fixture.start("alice");
    let scratch = fixture.home.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let a = fixture.root.join("src/a.rs");
    let b = scratch.join("b.rs");

    let out = fixture.edit("alice", &[&a, &b]);
    let reason = deny_reason(&out).unwrap_or_else(|| panic!("{out}"));
    assert!(reason.contains("native"), "{reason}");
    assert!(fixture.holds().is_empty());

    // A link inside the project to a file outside it is the same mixed edit.
    std::os::unix::fs::symlink(&b, fixture.root.join("src/link.rs")).unwrap();
    let out = fixture.edit("alice", &[&fixture.root.join("src/link.rs")]);
    assert!(deny_reason(&out).is_some(), "{out}");
    assert!(fixture.holds().is_empty());

    // Scratch and native edits stay denied; a project file has a controlled route.
    assert!(deny_reason(&fixture.edit("alice", &[&b])).is_some());
    assert!(deny_reason(&fixture.controlled("alice", &b, "scratch-call")).is_some());
    assert!(fixture.holds().is_empty());
    assert_eq!(fixture.controlled("alice", &a, "guarded-a"), "");
    assert_eq!(fixture.holds(), [format!("{alice} src/a.rs")]);
}
