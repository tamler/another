//! D70: Herdr-compatible pane status through the real binary, against a fake `herdr` that
//! records its arguments. Hooks report display-only pane metadata (tokens), never
//! lifecycle authority: no `report-agent`, no `release-agent`, so Herdr's own Claude or
//! Codex integration keeps the pane's state and session restore.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    main: PathBuf,
    herdr_dir: PathBuf,
    fake: PathBuf,
    /// `HERDR_ENV` for hooks run with a pane: `Some("1")` inside Herdr.
    herdr_env: Option<&'static str>,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Appends one line per call: the arguments separated by 0x1f, written in one `printf` so
/// concurrent calls cannot interleave.
#[cfg(unix)]
const FAKE_HERDR: &str = r#"#!/bin/sh
sep=$(printf '\037')
line=""
for a in "$@"; do line="$line$a$sep"; done
printf '%s\n' "$line" >> '@DIR@/calls.log'
"#;

#[cfg(windows)]
const FAKE_HERDR: &str = "@echo %*>>\"@DIR@\\calls.log\"\r\n";

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let main = root.join("work").join("shop");
        write(
            &main.join(".git").join("config"),
            "[remote \"origin\"]\n\turl = git@example.com:team/shop.git\n",
        );
        std::fs::create_dir_all(main.join("src")).unwrap();
        let herdr_dir = root.join("herdr");
        std::fs::create_dir_all(&herdr_dir).unwrap();
        let fake = herdr_dir.join(if cfg!(windows) { "herdr.cmd" } else { "herdr" });
        std::fs::write(
            &fake,
            FAKE_HERDR.replace("@DIR@", &herdr_dir.to_string_lossy()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let home = root.join("home");
        Env {
            another_home: home.join(".another"),
            home,
            main,
            herdr_dir,
            fake,
            herdr_env: Some("1"),
            _dir: dir,
        }
    }

    /// A hook run like a client runs it. `pane` sets the Herdr pane environment; `None`
    /// leaves the process outside Herdr (but still able to find the fake binary).
    fn hook(&self, event: &str, client: &str, stdin: &Value, pane: Option<&str>) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(["hook", "run", event, "--client", client])
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env("HERDR_BIN_PATH", &self.fake)
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("TERM_PROGRAM");
        if let Some(pane) = pane {
            cmd.env("HERDR_PANE_ID", pane);
            if let Some(v) = self.herdr_env {
                cmd.env("HERDR_ENV", v);
            }
        }
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
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook run must always exit 0");
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, client: &str, dir: &Path, pane: Option<&str>) {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook("session-start", client, &stdin, pane);
    }

    fn edit(&self, client: &str, dir: &Path, file: &str, pane: Option<&str>) -> String {
        let stdin = json!({
            "session_id": format!("{client}-s1"),
            "cwd": dir,
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join(file) },
        });
        self.hook("pre-edit", client, &stdin, pane)
    }

    fn post_tool(&self, client: &str, dir: &Path, pane: Option<&str>) {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook("post-tool", client, &stdin, pane);
    }

    fn end(&self, client: &str, dir: &Path, pane: Option<&str>) {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook("session-end", client, &stdin, pane);
    }

    /// Every recorded call, arguments joined by single spaces.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.herdr_dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                l.split('\u{1f}')
                    .filter(|a| !a.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|l| !l.trim().is_empty())
            .collect()
    }

    /// Reports run in the background: wait until `n` calls name `pane`.
    fn wait_for(&self, pane: &str, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let calls: Vec<String> = self
                .calls()
                .into_iter()
                .filter(|c| c.contains(&format!("report-metadata {pane} ")))
                .collect();
            if calls.len() >= n || Instant::now() > deadline {
                assert!(
                    calls.len() >= n,
                    "expected {n} reports for {pane}, got {calls:#?}"
                );
                return calls;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Give background reports that should not exist a chance to appear.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(600));
    }
}

fn seq(call: &str) -> u64 {
    let mut words = call.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--seq" {
            return words
                .next()
                .and_then(|n| n.trim_matches('"').parse().ok())
                .expect("--seq takes a number");
        }
    }
    panic!("no --seq in {call}");
}

#[test]
fn nothing_is_reported_outside_herdr() {
    let env = Env::new();
    // HERDR_BIN_PATH is set, but no pane.
    env.start("claude", &env.main, None);
    env.edit("claude", &env.main, "src/a.rs", None);
    env.post_tool("claude", &env.main, None);
    env.end("claude", &env.main, None);
    env.settle();
    assert!(env.calls().is_empty(), "{:#?}", env.calls());
}

#[test]
fn a_pane_without_herdr_env_1_is_outside_herdr() {
    for herdr_env in [None, Some("0")] {
        let mut env = Env::new();
        env.herdr_env = herdr_env;
        env.start("claude", &env.main, Some("w1:p1"));
        env.edit("claude", &env.main, "src/a.rs", Some("w1:p1"));
        env.post_tool("claude", &env.main, Some("w1:p1"));
        env.end("claude", &env.main, Some("w1:p1"));
        env.settle();
        assert!(
            env.calls().is_empty(),
            "HERDR_ENV={herdr_env:?}: {:#?}",
            env.calls()
        );
    }
}

#[test]
fn an_empty_pane_id_is_outside_herdr_too() {
    let env = Env::new();
    env.start("claude", &env.main, Some(""));
    env.edit("claude", &env.main, "src/a.rs", Some(""));
    env.settle();
    assert!(env.calls().is_empty(), "{:#?}", env.calls());
}

#[test]
fn hooks_report_working_blocked_and_clear_as_display_metadata() {
    let env = Env::new();
    let (p1, p2) = ("w1:p1", "w1:p2");
    env.start("claude", &env.main, Some(p1));
    let first = env.wait_for(p1, 1);
    assert!(
        first[0].starts_with("pane report-metadata w1:p1 --source another"),
        "{first:?}"
    );
    assert!(first[0].contains("another_state=working"), "{first:?}");
    assert!(first[0].contains("--ttl-ms"), "tokens expire: {first:?}");

    env.start("codex", &env.main, Some(p2));
    env.wait_for(p2, 1);
    assert_eq!(env.edit("claude", &env.main, "src/a.rs", Some(p1)), "");
    // Codex's edit of the same file is stopped by claude's hold: blocked, with a reason.
    assert!(env
        .edit("codex", &env.main, "src/a.rs", Some(p2))
        .contains("\"deny\""));
    let calls = env.wait_for(p2, 2);
    let blocked = calls.last().unwrap();
    assert!(blocked.contains("another_state=blocked"), "{blocked}");
    assert!(
        blocked.contains("src/a.rs") && blocked.contains("claude"),
        "the reason names the file and the holder: {blocked}"
    );

    // Claude's session ends (its hold is released): its tokens are cleared.
    env.end("claude", &env.main, Some(p1));
    let calls = env.wait_for(p1, 2);
    let cleared = calls.last().unwrap();
    assert!(
        cleared.contains("--clear-token another_state")
            && cleared.contains("--clear-token another"),
        "{cleared}"
    );
    // Codex is no longer blocked once that hold has ended.
    env.post_tool("codex", &env.main, Some(p2));
    let calls = env.wait_for(p2, 3);
    let working = calls.last().unwrap();
    assert!(working.contains("another_state=working"), "{working}");
    assert!(working.contains("--clear-token another"), "{working}");

    // Lifecycle authority stays with Herdr's own integration.
    for c in env.calls() {
        assert!(
            !c.contains("report-agent") && !c.contains("release-agent"),
            "{c}"
        );
    }
    // Sequence numbers increase with every report for a pane.
    for pane in [p1, p2] {
        let seqs: Vec<u64> = env.wait_for(pane, 1).iter().map(|c| seq(c)).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seqs.len(), "distinct: {seqs:?}");
        assert_eq!(sorted, seqs, "increasing in report order: {seqs:?}");
    }
}

#[test]
fn an_unchanged_state_is_not_reported_again() {
    let env = Env::new();
    env.start("claude", &env.main, Some("w1:p1"));
    env.wait_for("w1:p1", 1);
    for _ in 0..5 {
        env.post_tool("claude", &env.main, Some("w1:p1"));
    }
    env.settle();
    assert_eq!(env.wait_for("w1:p1", 1).len(), 1, "{:#?}", env.calls());
}

#[test]
fn a_missing_herdr_binary_never_fails_a_hook() {
    let env = Env::new();
    std::fs::remove_file(&env.fake).unwrap();
    env.start("claude", &env.main, Some("w1:p1"));
    assert_eq!(env.edit("claude", &env.main, "src/a.rs", Some("w1:p1")), "");
    env.end("claude", &env.main, Some("w1:p1"));
}
