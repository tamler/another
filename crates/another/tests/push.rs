//! Real CLI mail, native denial and wake checks. Legacy advisory path projections
//! remain separately exercised; they are not authorization for a native editor.
#![cfg(unix)]

#[path = "common/hosted_ci.rs"]
mod hosted_ci;

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
    # Test controls: `hang` never answers; `slow` holds the answer until it is removed.
    if [ -f "$dir/hang" ]; then exec sleep 30; fi
    while [ -f "$dir/slow" ]; do sleep 0.05; done
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
        let mut stdin = stdin.clone();
        if stdin.get("session_id").and_then(Value::as_str) == Some("s1") {
            stdin["session_id"] = json!(format!("{client}-s1"));
        }
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

    /// Deny the real native call first; separately inspect the legacy advisory API.
    /// Its result is never presented to a client and never writes a file.
    fn advisory(&self, client: &str, input: &Value) -> String {
        let native = self.hook("pre-edit", client, input);
        assert!(
            native.contains("\"permissionDecision\":\"deny\""),
            "native path edits must remain denied"
        );
        let parsed = another::hooks::HookInput::parse(&input.to_string());
        let cwd = parsed.cwd.as_ref().unwrap();
        let root = another::hooks::project_root(cwd);
        let physical_root = another::hooks::resolve_existing(&root);
        let mut files = Vec::new();
        for raw in parsed.edit_paths() {
            let raw = PathBuf::from(raw);
            let full = if raw.is_absolute() {
                raw
            } else {
                cwd.join(raw)
            };
            let mut targets = vec![another::hooks::resolve_existing(&full)];
            if let (Some(parent), Some(name)) = (full.parent(), full.file_name()) {
                let entry = another::hooks::resolve_existing(parent).join(name);
                if !targets.contains(&entry) {
                    targets.push(entry);
                }
            }
            for target in targets {
                if let Ok(relative) = target.strip_prefix(&physical_root) {
                    let relative = relative.to_str().unwrap().to_owned();
                    if !files.contains(&relative) {
                        files.push(relative);
                    }
                }
            }
        }
        if files.is_empty() {
            return String::new();
        }
        let store = another_core::Store::open(&self.another_home).unwrap();
        let program = serde_json::from_value(json!(client)).unwrap();
        let result = store.admit_edit(
            client,
            program,
            root.to_str().unwrap(),
            None,
            &format!("{client}-s1"),
            another::hooks::AUTO_HOLD_REASON,
            |_| another_core::EditScope {
                groups: vec![(root.to_str().unwrap().into(), files)],
                unguarded: vec![],
            },
        );
        let result=match result {
            Ok(result)=>result,
            Err(another_core::Error::Invalid(message))=>return json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":message}}).to_string(),
            Err(error)=>panic!("unexpected advisory failure: {error}"),
        };
        if result.conflicts.is_empty() {
            String::new()
        } else {
            json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
                "permissionDecisionReason":another::hooks::deny_message(&result.agent,&result.conflicts)}}).to_string()
        }
    }

    fn start(&self, client: &str, dir: &Path, extra_env: &[(&str, &str)]) -> String {
        // Hooks never create a home: it exists before any session starts.
        self.ok(&["init"]);
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        let out = self
            .hook_timed("session-start", client, &stdin, extra_env)
            .0;
        let digest = if client == "cursor" {
            serde_json::from_str::<Value>(&out).unwrap()["additional_context"]
                .as_str()
                .unwrap()
                .to_string()
        } else {
            out
        };
        digest
            .strip_prefix("Another: ")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
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

    fn seed_auto_target(&self, agent: &str, via: another_core::WakeVia, target: &str) {
        another_core::Store::open(&self.another_home)
            .unwrap()
            .set_wake_target(agent, via, target, true)
            .unwrap();
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
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    let after_bash = tool(&env.main, "Bash", json!({ "command": "ls" }));

    assert_eq!(
        env.hook("post-tool", "claude", &after_bash),
        "",
        "nothing new"
    );

    env.send(&codex, &claude, "Release src/a.rs?", true);
    let notice = context(&env.hook("post-tool", "claude", &after_bash));
    assert!(
        notice.contains(&format!("1 new message(s) for {claude} (0 urgent)")),
        "{notice}"
    );
    assert!(notice.contains(&format!("\"Release src/a.rs?\" from {codex}")));
    assert!(notice.contains("opens request"), "{notice}");
    assert!(!notice.contains("secret body"), "bodies never appear");

    // Announced once, whichever hook sees it first.
    assert_eq!(env.hook("post-tool", "claude", &after_bash), "");
    assert_eq!(env.hook("prompt", "claude", &after_bash), "");

    // Cursor gets its own context format.
    let cursor = env.start("cursor", &env.main, &[]);
    env.send(&codex, &cursor, "Docs", false);
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
    let unread = env.json(&["inbox", "--agent", &claude, "--unread"]);
    let id = unread["messages"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["ack", "--agent", &claude, &id]);
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["notices_mid_task"], 2, "{stats}");
    assert_eq!(stats["notices_prompt"], 0);
    assert_eq!(stats["messages_pushed"], 2);
    assert_eq!(stats["pushes_acted_on"], 1);
    assert_eq!(stats["requests_created"], 1);
}

#[test]
fn advisory_projection_holds_files_and_reports_sibling_worktree_conflicts() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    let a = env.main.join("src/a.rs");

    // Claude edits: allowed silently, and the file is now held.
    let out = env.advisory(
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
    assert_eq!(holds[0]["agent"], claude);
    assert_eq!(holds[0]["pattern"], "src/a.rs");
    assert_eq!(holds[0]["exclusive"], true);
    assert!(holds[0]["reason"].as_str().unwrap().contains("edit hook"));

    // Codex, in a sibling worktree of the same repository, patches the same file: stopped,
    // with the holder named and a way to ask.
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch\n";
    let out = env.advisory(
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch })),
    );
    let reason = denial(&out);
    assert!(reason.contains(&format!("held by {claude}")), "{reason}");
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
        reason.contains(&format!(
            "another send --from {codex} --to {claude} --request"
        )),
        "{reason}"
    );
    assert!(!reason.contains('\u{2014}'), "no em dashes in copy");

    // A different file is fine, and gets its own hold in the worktree's project.
    let patch = "*** Begin Patch\n*** Add File: src/b.rs\n+fn b() {}\n*** End Patch\n";
    let out = env.advisory(
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch })),
    );
    assert_eq!(out, "");
    assert!(env
        .holds()
        .iter()
        .any(|h| h["agent"] == codex && h["pattern"] == "src/b.rs"));

    // Relative paths resolve against cwd; Claude is now the one stopped.
    let out = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": "src/b.rs", "content": "" }),
        ),
    );
    assert!(denial(&out).contains(&format!("held by {codex}")));

    // Notebooks, files outside any project, and tools without a path.
    let out = env.advisory(
        "claude",
        &tool(
            &env.main,
            "NotebookEdit",
            json!({ "notebook_path": env.main.join("notes/x.ipynb") }),
        ),
    );
    assert_eq!(out, "");
    let scratch = env.root.join("scratch/x.txt");
    let out = env.advisory(
        "claude",
        &tool(&env.main, "Write", json!({ "file_path": scratch })),
    );
    assert_eq!(out, "");
    assert_eq!(
        env.advisory("claude", &tool(&env.main, "Edit", json!({}))),
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

    // Editing a file one already holds takes no new hold: it renews the session's holds
    // so the edit starts with a full TTL, and records that once.
    let events_before = env.json(&["events", "--limit", "1000"]);
    let holds_before = env.holds().len();
    env.advisory(
        "claude",
        &tool(&env.main, "Edit", json!({ "file_path": a })),
    );
    let events_after = env.json(&["events", "--limit", "1000"]);
    let added = &events_after.as_array().unwrap()[events_before.as_array().unwrap().len()..];
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!(added[0]["kind"], "hold.renewed");
    assert_eq!(env.holds().len(), holds_before);

    // An unfenced hook End does not release holds; explicit exact-actor End does.
    env.hook(
        "session-end",
        "claude",
        &json!({ "session_id": "claude-s1", "cwd": env.main }),
    );
    assert!(env.holds().iter().any(|h| h["agent"] == claude));
    env.ok(&["heartbeat", "--agent", &claude, "--end"]);
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch\n";
    let out = env.advisory(
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
    assert_eq!(first["agent"], codex);
    assert_eq!(first["holder"], claude);
    assert_eq!(first["path"], "src/a.rs");
    assert_eq!(first["project"], "shop-review");
    assert_eq!(first["holder_project"], "shop");
    let human = env.ok(&["stats", "--since", "1h"]);
    assert!(human.contains("edits stopped by a hold"), "{human}");
    assert!(
        human.contains(&format!(
            "{codex} on shop-review:src/a.rs, held by {claude}"
        )),
        "{human}"
    );
}

#[test]
fn controlled_admissions_conflict_across_worktrees_and_keep_distinct_holds() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    for root in [&env.main, &env.wt] {
        for file in ["src/a.rs", "src/b.rs"] {
            std::fs::write(root.join(file), b"x").unwrap();
        }
    }
    let admit = |client: &str, root: &Path, path: &str, id: &str| {
        let mut input = tool(
            root,
            "mcp__another__controlled_edit",
            json!({"project":root,"path":path,
            "expected_sha256":another_core::controlled_sha256(b"x"),"replacement":"y"}),
        );
        input["tool_use_id"] = json!(id);
        env.hook("pre-controlled-edit", client, &input)
    };
    assert_eq!(admit("claude", &env.main, "src/a.rs", "claude-a"), "");
    let first = env.holds();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["agent"], claude);
    assert!(denial(&admit("codex", &env.wt, "src/a.rs", "codex-a")).contains(&claude));
    assert_eq!(
        env.holds(),
        first,
        "conflict must not leave partial coverage"
    );
    assert_eq!(admit("codex", &env.wt, "src/b.rs", "codex-b"), "");
    assert!(denial(&admit("claude", &env.main, "src/b.rs", "claude-b")).contains(&codex));
    assert_eq!(admit("claude", &env.main, "src/a.rs", "claude-a2"), "");
    let held = env.holds();
    assert_eq!(held.len(), 3, "each operation needs its own pending hold");
    assert!(
        held.contains(&first[0]),
        "older pending coverage must remain unchanged"
    );
    env.hook(
        "session-end",
        "claude",
        &json!({"session_id":"claude-s1","cwd":env.main}),
    );
    assert_eq!(env.holds(), held);
    env.ok(&["heartbeat", "--agent", &claude, "--end"]);
    assert_eq!(
        env.holds(),
        held,
        "explicit end must not release pending admissions"
    );
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["edits_blocked"], 2);
    assert_eq!(stats["holds_auto_acquired"], 3);
}

#[test]
fn tool_hooks_are_fast_when_the_database_is_missing_or_locked() {
    let denied = |out: &str| {
        serde_json::from_str::<Value>(out).unwrap()["hookSpecificOutput"]["permissionDecision"]
            == "deny"
    };
    let env = Env::new();
    let edit = tool(
        &env.main,
        "Edit",
        json!({ "file_path": env.main.join("src/a.rs") }),
    );
    // No database yet: the notice hook is silent, the edit guard stops the edit it cannot
    // protect, both are quick, and nothing is created.
    let (out, took) = env.hook_timed("post-tool", "claude", &edit, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let (out, _) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert!(denied(&out), "{out}");
    assert!(!env.another_home.exists());

    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    env.send(&codex, &claude, "While locked", false);

    // A writer holding the lock: announcing needs the claim write, so the notice waits for
    // the next tool call instead of risking a duplicate. Quiet and quick, never lost.
    let db = rusqlite::Connection::open(env.another_home.join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let after = tool(&env.main, "Bash", json!({}));
    let (out, took) = env.hook_timed("post-tool", "claude", &after, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2), "{took:?}");
    // The edit guard cannot write, so it cannot hold the file: the edit is stopped.
    let (out, took) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert!(denied(&out), "{out}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    let (out, _) = env.hook_timed("post-tool", "claude", &after, &[]);
    assert!(
        context(&out).contains("While locked"),
        "delivered once the lock is gone"
    );

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
    assert!(denied(&out), "{out}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);

    // Only the hook that failed open leaves a trace; a stopped edit says why itself.
    let log = std::fs::read_to_string(env.another_home.join("logs/hooks.log")).unwrap();
    assert!(log.contains("post-tool --client claude"), "{log}");
    assert!(log.contains("failing open"), "{log}");
    assert!(!log.contains("pre-edit"), "{log}");
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
fn hook_start_and_end_do_not_change_wake_targets() {
    let env = Env::new();
    let in_herdr = [("HERDR_PANE_ID", "w7:p2"), ("TERM_PROGRAM", "herdr")];
    let claude = env.start("claude", &env.main, &in_herdr);
    assert!(env.json(&["wake", "list"]).as_array().unwrap().is_empty());
    env.seed_auto_target(&claude, another_core::WakeVia::Herdr, "w7:p2");

    // tmux running inside a Herdr pane: tmux is the innermost terminal.
    let codex = env.start(
        "codex",
        &env.wt,
        &[
            ("HERDR_PANE_ID", "w7:p3"),
            ("TMUX_PANE", "%4"),
            ("TERM_PROGRAM", "tmux"),
        ],
    );
    assert_eq!(env.json(&["wake", "list"]).as_array().unwrap().len(), 1);
    env.seed_auto_target(&codex, another_core::WakeVia::Tmux, "%4");
    let targets = env.json(&["wake", "list"]);
    assert!(targets
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["agent"] == codex && t["via"] == "tmux" && t["target"] == "%4"));

    // Neither a different pane's End nor a matching pane's End clears the target.
    let end = json!({ "session_id": "claude-s1", "cwd": env.main });
    env.hook_timed("session-end", "claude", &end, &[("HERDR_PANE_ID", "w9:p9")]);
    assert_eq!(env.json(&["wake", "list"]).as_array().unwrap().len(), 2);
    env.hook_timed("session-end", "claude", &end, &in_herdr);
    let targets = env.json(&["wake", "list"]);
    assert!(targets
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["agent"] == claude));
}

#[test]
fn wake_prune_drops_dead_automatic_targets_only() {
    hosted_ci::require_hosted_runner();
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[("HERDR_PANE_ID", "w1:p1")]);
    let codex = env.start("codex", &env.wt, &[("HERDR_PANE_ID", "w1:p2")]);
    env.seed_auto_target(&claude, another_core::WakeVia::Herdr, "w1:p1");
    env.seed_auto_target(&codex, another_core::WakeVia::Herdr, "w1:p2");
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
    assert_eq!(row(&claude)["pruned"], true);
    assert!(row(&claude)["reason"].as_str().unwrap().contains("gone"));
    assert_eq!(row(&codex)["pruned"], false);
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
    assert_eq!(left, ["bob".to_string(), codex]);
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
        hosted_ci::require_hosted_runner();
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

impl Drop for Daemon {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("test-owned daemon {pid} did not exit after SIGTERM");
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
    let claude = env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    let codex = env.start("codex", &env.wt, &[]);
    env.seed_auto_target(&claude, another_core::WakeVia::Herdr, "w7:p2");
    env.set_pane("w7:p2", Some("working claude"));
    let daemon = Daemon::start(&env);

    // Working: the daemon looks, but leaves delivery to the post-tool hook.
    env.send(&codex, &claude, "Rebase please", false);
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
    env.send(&codex, &claude, "One more", false);
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
    assert_eq!(stats["wakes"][0]["agent"], claude);
    assert_eq!(stats["wakes"][0]["target"], "w7:p2");
    assert_eq!(stats["wakes"][0]["outcome"], "sent");
}

// ---------------------------------------------------------------------------
// Regressions for the PR #103 review (REVIEW-another-pr103-codex.md).

/// F1: an edit through a symlink is checked against the file it really reaches.
#[test]
fn advisory_symlinked_paths_do_not_bypass_a_hold() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    std::fs::write(env.wt.join("src/a.rs"), "x").unwrap();
    let held = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Edit",
            json!({ "file_path": env.main.join("src/a.rs") }),
        ),
    );
    assert_eq!(held, "");

    // An alias inside the worktree pointing at its src/.
    std::os::unix::fs::symlink(env.wt.join("src"), env.wt.join("alias")).unwrap();
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("alias/a.rs") }),
        ),
    );
    let reason = denial(&out);
    assert!(
        reason.starts_with(&format!(
            "Another stopped this edit: src/a.rs is held by {claude}"
        )),
        "{reason}"
    );
    // An alias outside any checkout pointing into it.
    let outside = env.root.join("outside-link");
    std::os::unix::fs::symlink(env.wt.join("src"), &outside).unwrap();
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": outside.join("a.rs") }),
        ),
    );
    assert!(denial(&out).contains(&format!("held by {claude}")));
    // A file that is itself a symlink to the held file.
    std::os::unix::fs::symlink(env.wt.join("src/a.rs"), env.wt.join("src/link.rs")).unwrap();
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("src/link.rs") }),
        ),
    );
    assert!(denial(&out).contains(&format!("held by {claude}")));
    // No alias hold was taken by the stopped agent.
    assert!(!env.holds().iter().any(|h| h["agent"] == codex));

    // Manual holds resolve aliases too: a hold taken through `alias/` covers src/.
    let wt = env.wt.to_string_lossy().into_owned();
    let out = env
        .cmd(&[
            "hold",
            "acquire",
            "--agent",
            &codex,
            "--project",
            &wt,
            "alias/a.rs",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "conflicts with claude's src/a.rs"
    );
}

/// F2: concurrent hooks announce one message once.
#[test]
fn concurrent_hooks_announce_a_message_once() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    let after = tool(&env.main, "Bash", json!({}));
    for round in 0..5 {
        env.send(&codex, &claude, &format!("round {round}"), false);
        let outs: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| env.hook("post-tool", "claude", &after)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let shown = outs.iter().filter(|o| !o.is_empty()).count();
        assert_eq!(shown, 1, "round {round}: {outs:?}");
    }
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["notices_mid_task"], 5);
    assert_eq!(stats["messages_pushed"], 5);
}

/// F2: a hook that announces while the daemon is probing Herdr wins; no second prompt.
#[test]
fn daemon_does_not_wake_for_mail_a_hook_announced_during_the_probe() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    let codex = env.start("codex", &env.wt, &[]);
    env.seed_auto_target(&claude, another_core::WakeVia::Herdr, "w7:p2");
    env.set_pane("w7:p2", Some("idle claude"));
    std::fs::write(env.herdr_dir.join("slow"), "").unwrap();
    let daemon = Daemon::start(&env);
    env.send(&codex, &claude, "Race me", false);
    // The daemon has picked the message and is waiting on Herdr.
    assert!(wait_for(Duration::from_secs(15), || env
        .herdr_calls()
        .iter()
        .any(|c| c == "agent get w7:p2")));
    let notice = env.hook("post-tool", "claude", &tool(&env.main, "Bash", json!({})));
    assert!(context(&notice).contains("Race me"));
    std::fs::remove_file(env.herdr_dir.join("slow")).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !env.herdr_calls()
            .iter()
            .any(|c| c.starts_with("agent prompt")),
        "{:?}",
        env.herdr_calls()
    );
    assert!(daemon.stop().success());
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["wakes_sent"], 0);
    assert_eq!(stats["notices_mid_task"], 1);
}

/// F3: a burst larger than one notice's preview is announced and counted in full.
#[test]
fn a_large_burst_is_counted_in_full() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    let after = tool(&env.main, "Bash", json!({}));
    env.send(&codex, &claude, "warmup", false);
    assert!(!env.hook("post-tool", "claude", &after).is_empty());
    for i in 0..120 {
        env.send(&codex, &claude, &format!("burst {i}"), false);
    }
    let notice = context(&env.hook("post-tool", "claude", &after));
    assert!(notice.contains("120 new message(s)"), "{notice}");
    assert_eq!(env.hook("post-tool", "claude", &after), "");
    let stats = env.json(&["stats", "--since", "1h"]);
    assert_eq!(stats["messages_pushed"], 121, "warmup plus all 120");
}

/// F5: an adapter that never answers cannot stall the daemon's shutdown.
#[test]
fn a_hung_adapter_does_not_block_sigterm() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    let codex = env.start("codex", &env.wt, &[]);
    env.seed_auto_target(&claude, another_core::WakeVia::Herdr, "w7:p2");
    std::fs::write(env.herdr_dir.join("hang"), "").unwrap();
    let daemon = Daemon::start(&env);
    env.send(&codex, &claude, "Anyone there?", false);
    assert!(wait_for(Duration::from_secs(15), || env
        .herdr_calls()
        .iter()
        .any(|c| c == "agent get w7:p2")));
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let status = daemon.stop();
    let took = started.elapsed();
    assert!(status.success(), "{status:?}");
    assert!(took < Duration::from_secs(5), "shutdown took {took:?}");
}

/// F5: a probe that hangs times out, and the pass moves on.
#[test]
fn adapter_commands_are_bounded() {
    hosted_ci::require_hosted_runner();
    let started = Instant::now();
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let out = another::wake::run_bounded(&mut cmd, Duration::from_millis(300), None).unwrap();
    assert!(out.is_none(), "timed out");
    assert!(started.elapsed() < Duration::from_secs(3));
    let flag = std::sync::atomic::AtomicBool::new(true);
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let started = Instant::now();
    assert!(
        another::wake::run_bounded(&mut cmd, Duration::from_secs(30), Some(&flag))
            .unwrap()
            .is_none()
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel stops it"
    );
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo hi"]);
    let (status, stdout) = another::wake::run_bounded(&mut cmd, Duration::from_secs(5), None)
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(stdout, b"hi\n");
}

// ---------------------------------------------------------------------------
// Regressions for the round 2 review (REVIEW-another-pr103-r2.md).

/// N1: `..` after a symlink is applied where the link points, not lexically.
#[test]
fn advisory_dot_dot_after_a_symlink_does_not_escape_the_guard() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    std::fs::create_dir_all(env.wt.join("src/deep")).unwrap();
    std::fs::write(env.wt.join("src/a.rs"), "x").unwrap();
    std::os::unix::fs::symlink(env.wt.join("src/deep"), env.wt.join("deeplink")).unwrap();
    let held = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Edit",
            json!({ "file_path": env.main.join("src/a.rs") }),
        ),
    );
    assert_eq!(held, "");
    // Lexically `shop-review/a.rs`; physically `shop-review/src/a.rs`, the held file.
    for path in [
        "deeplink/../a.rs".to_string(),
        format!("{}/deeplink/../a.rs", env.wt.display()),
    ] {
        let out = env.advisory(
            "codex",
            &tool(&env.wt, "Edit", json!({ "file_path": path })),
        );
        assert!(
            denial(&out).contains(&format!("src/a.rs is held by {claude}")),
            "{path}"
        );
    }
    // A plain `..` with no symlink still resolves the ordinary way.
    let out = env.advisory(
        "codex",
        &tool(&env.wt, "Edit", json!({ "file_path": "src/deep/../a.rs" })),
    );
    assert!(denial(&out).contains(&format!("held by {claude}")));
}

/// N2: a dangling symlink to a held, not yet existing file is checked at its target.
#[test]
fn advisory_dangling_symlink_to_a_held_new_file_is_stopped() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    let out = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": env.main.join("src/new.rs") }),
        ),
    );
    assert_eq!(out, "");
    assert!(!env.wt.join("src/new.rs").exists());
    std::os::unix::fs::symlink(env.wt.join("src/new.rs"), env.wt.join("dangle.rs")).unwrap();
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("dangle.rs") }),
        ),
    );
    assert!(
        denial(&out).contains(&format!("src/new.rs is held by {claude}")),
        "{out}"
    );
    assert!(!env.holds().iter().any(|h| h["agent"] == codex));
    // A chain of links, and a loop, do not hang or crash the hook.
    std::os::unix::fs::symlink(env.wt.join("dangle.rs"), env.wt.join("chain.rs")).unwrap();
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("chain.rs") }),
        ),
    );
    assert!(denial(&out).contains(&format!("held by {claude}")));
    std::os::unix::fs::symlink(env.wt.join("loop2"), env.wt.join("loop1")).unwrap();
    std::os::unix::fs::symlink(env.wt.join("loop1"), env.wt.join("loop2")).unwrap();
    let (out, took) = env.hook_timed(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("loop1/x.rs") }),
        ),
        &[],
    );
    assert!(out.contains("\"permissionDecision\":\"deny\""));
    assert!(took < Duration::from_secs(2));
}

/// N5: a glob hold taken through a symlinked directory protects the real files.
#[test]
fn advisory_glob_holds_through_a_symlinked_directory_are_resolved() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.main.join("src"), env.main.join("alias")).unwrap();
    let main = env.main.to_string_lossy().into_owned();
    let granted = env.json(&[
        "hold",
        "acquire",
        "--agent",
        &claude,
        "--project",
        &main,
        "alias/**",
    ]);
    assert_eq!(granted["granted"][0]["pattern"], "src/**", "{granted}");
    // An absolute glob through the alias, and a glob at the project root, as well.
    let abs = format!("{}/alias/*.toml", env.main.display());
    let granted = env.json(&[
        "hold",
        "acquire",
        "--agent",
        &claude,
        "--project",
        &main,
        &abs,
        "*.md",
    ]);
    let patterns: Vec<&str> = granted["granted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["pattern"].as_str().unwrap())
        .collect();
    assert_eq!(patterns, ["src/*.toml", "*.md"]);
    // The sibling worktree's edit of a file under src/ is now stopped.
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("src/b.rs") }),
        ),
    );
    assert!(denial(&out).contains(&format!("held by {claude}")));
}

/// N4: an adapter that leaves a process behind on its stdout does not stall the caller.
#[test]
fn a_descendant_holding_stdout_does_not_outlast_the_deadline() {
    hosted_ci::require_hosted_runner();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 20 & echo '{\"result\":{}}'"]);
    let started = Instant::now();
    let (status, stdout) = another::wake::run_bounded(&mut cmd, Duration::from_secs(3), None)
        .unwrap()
        .expect("the adapter itself exited");
    assert!(status.success());
    assert_eq!(stdout, b"{\"result\":{}}\n");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "returned after {:?}",
        started.elapsed()
    );
    // A stuck adapter with a stuck child: both stop at the deadline.
    let marker = std::env::temp_dir().join(format!("another-n4-{}", std::process::id()));
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!("(sleep 2; touch '{}') & sleep 30", marker.display()),
    ]);
    let started = Instant::now();
    let out = another::wake::run_bounded(&mut cmd, Duration::from_millis(300), None).unwrap();
    assert!(out.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !marker.exists(),
        "the adapter's background process was stopped with it"
    );
}

#[test]
fn advisory_dangling_link_after_a_missing_directory_and_dot_dot_is_followed() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let out = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": env.main.join("src/new.rs") }),
        ),
    );
    assert_eq!(out, "");
    std::os::unix::fs::symlink(env.wt.join("src/new.rs"), env.wt.join("dangle.rs")).unwrap();
    // `nope` does not exist; a client that creates parent directories before writing
    // ends up writing through `dangle.rs`.
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("nope/../dangle.rs") }),
        ),
    );
    assert!(
        denial(&out).contains(&format!("src/new.rs is held by {claude}")),
        "{out}"
    );
}

/// Whether the volume holding `dir` treats `a` and `A` as one name, found by writing.
fn volume_folds_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    std::fs::write(&probe, "").unwrap();
    let folds = dir.join("CASE-PROBE").exists();
    std::fs::remove_file(probe).unwrap();
    folds
}

#[test]
fn advisory_new_file_case_matches_only_where_the_volume_folds_case() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let out = env.advisory(
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": env.main.join("src/new.rs") }),
        ),
    );
    assert_eq!(out, "");
    let out = env.advisory(
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("src/NEW.RS") }),
        ),
    );
    if volume_folds_case(&env.wt) {
        assert!(
            denial(&out).contains(&format!("src/NEW.RS is held by {claude}")),
            "{out}"
        );
    } else {
        assert_eq!(out, "", "two different files on a case-sensitive volume");
    }
}

#[test]
fn advisory_symlink_loop_does_not_register_a_second_project() {
    let env = Env::new();
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.wt.join("loop2"), env.wt.join("loop1")).unwrap();
    std::os::unix::fs::symlink(env.wt.join("loop1"), env.wt.join("loop2")).unwrap();
    // Spell the worktree through a symlinked parent, so a fallback to the raw path
    // would name a directory that is not the registered one.
    let alias = env.root.join("alias");
    std::os::unix::fs::symlink(env.wt.parent().unwrap(), &alias).unwrap();
    let raw = alias.join(env.wt.file_name().unwrap()).join("loop1/x.rs");
    let out = env.advisory(
        "codex",
        &tool(&env.wt, "Write", json!({ "file_path": raw })),
    );
    assert_eq!(out, "");
    let projects: Vec<String> = env
        .holds()
        .iter()
        .map(|h| h["project"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(projects.iter().all(|p| p == "shop-review"), "{projects:?}");
}

#[test]
fn advisory_case_folding_keeps_glob_character_classes_exact() {
    let env = Env::new();
    let claude = env.start("claude", &env.main, &[]);
    let codex = env.start("codex", &env.wt, &[]);
    write(&env.wt.join("src/Upper.rs"), "");
    write(&env.wt.join("src/a.rs"), "");
    let main = env.main.to_string_lossy().into_owned();
    let folds = volume_folds_case(&env.wt);
    // (claude's glob, codex edits, denied)
    let rows = [
        ("src/[!a-z]*.rs", "src/Upper.rs", true),
        ("src/[!A-Z]*.rs", "src/a.rs", true),
        ("src/[A-Z]*.rs", "src/a.rs", false),
        ("src/*.RS", "src/a.rs", folds),
    ];
    for (glob, file, denied) in rows {
        env.json(&[
            "hold",
            "acquire",
            "--agent",
            &claude,
            "--project",
            &main,
            glob,
        ]);
        let out = env.advisory(
            "codex",
            &tool(&env.wt, "Write", json!({ "file_path": env.wt.join(file) })),
        );
        if denied {
            assert!(
                out.contains(&format!("held by {claude}")),
                "{glob} vs {file}: {out}"
            );
            assert!(denial(&out).contains(&format!("held by {claude}")));
        } else {
            assert_eq!(out, "", "{glob} vs {file}");
        }
        env.json(&["hold", "release", "--agent", &claude]);
        env.json(&["hold", "release", "--agent", &codex]);
    }
}

#[test]
fn advisory_symlink_loop_then_dot_dot_does_not_register_a_second_project() {
    let env = Env::new();
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.wt.join("loop2"), env.wt.join("loop1")).unwrap();
    std::os::unix::fs::symlink(env.wt.join("loop1"), env.wt.join("loop2")).unwrap();
    let alias = env.root.join("alias");
    std::os::unix::fs::symlink(env.wt.parent().unwrap(), &alias).unwrap();
    let raw = alias
        .join(env.wt.file_name().unwrap())
        .join("loop1/../src/a.rs");
    env.advisory(
        "codex",
        &tool(&env.wt, "Write", json!({ "file_path": raw })),
    );
    let projects = env.json(&["project", "list"]);
    let slugs: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap())
        .collect();
    assert!(
        !slugs.iter().any(|s| s.starts_with("shop-review-")),
        "{slugs:?}"
    );
}
