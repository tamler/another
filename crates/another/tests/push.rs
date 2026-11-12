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
fn manual_route_directory_holds_block_auto_edits_in_both_orders() {
    for (pattern, path) in [
        ("app/[id]", "app/[id]/page.tsx"),
        ("app/[id]/**", "app/[id]/page.tsx"),
        ("app/[id]/*.tsx", "app/[id]/page.tsx"),
        ("app/[id]/**/*.tsx", "app/[id]/deep/page.tsx"),
    ] {
        let env = Env::new();
        env.start("claude", &env.main, &[]);
        env.start("codex", &env.wt, &[]);
        let args = [
            "--json",
            "hold",
            "acquire",
            "--agent",
            "codex@shop-review",
            "--project",
            env.wt.to_str().unwrap(),
            pattern,
        ];
        env.ok(&args);
        let edit = tool(&env.main, "Write", json!({ "file_path": path }));
        assert!(
            denial(&env.hook("pre-edit", "claude", &edit)).contains("held by codex@shop-review")
        );
        env.ok(&["hold", "release", "--agent", "codex@shop-review"]);
        assert_eq!(env.hook("pre-edit", "claude", &edit), "");
        let out = env.cmd(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{pattern}");
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(report["granted"].as_array().unwrap().is_empty());
        assert_eq!(report["conflicts"][0]["held"]["agent"], "claude@shop");
    }
}

#[test]
fn linked_release_requests_close_and_repeated_asks_reuse_the_request() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    env.hook(
        "pre-edit",
        "claude",
        &tool(&env.main, "Edit", json!({ "file_path": "src/a.rs" })),
    );
    let holds = env.holds();
    let hold_id = holds[0]["hold_id"].as_str().unwrap();
    let args = [
        "hold",
        "request-release",
        "--agent",
        "codex@shop-review",
        hold_id,
    ];
    let first = env.json(&args);
    let again = env.json(&args);
    assert_eq!(first["message_id"], again["message_id"]);
    let rid = first["request_id"].as_str().unwrap();
    env.ok(&["request", "claim", rid, "--agent", "claude@shop"]);
    let unrelated = env.json(&[
        "send",
        "--from",
        "codex@shop-review",
        "--to",
        "claude@shop",
        "--request",
        "--subject",
        "Unrelated work",
    ]);
    env.ok(&["hold", "release", "--agent", "claude@shop", hold_id]);
    let requests = env.json(&["requests"]);
    let items = requests.as_array().unwrap();
    assert_eq!(
        items.iter().find(|r| r["request_id"] == rid).unwrap()["status"],
        "done"
    );
    assert_eq!(
        items
            .iter()
            .find(|r| r["request_id"] == unrelated["request_id"])
            .unwrap()["status"],
        "open"
    );
    let after = tool(&env.main, "Bash", json!({ "command": "true" }));
    let notice = context(&env.hook("post-tool", "claude", &after));
    assert!(notice.contains("Unrelated work"));
    assert!(!notice.contains("Release src/a.rs"));
}

#[test]
fn literal_metacharacter_filenames_are_protected_across_worktrees() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    for path in [
        "app/[id]/page.tsx",
        "src/{x}.ts",
        "src/*.rs",
        "src/a?.rs",
        "src/[.rs",
    ] {
        write(&env.main.join(path), "old");
        write(&env.wt.join(path), "old");
        assert_eq!(
            env.hook(
                "pre-edit",
                "claude",
                &tool(&env.main, "Edit", json!({ "file_path": path }))
            ),
            ""
        );
        let reason = denial(&env.hook(
            "pre-edit",
            "codex",
            &tool(&env.wt, "Edit", json!({ "file_path": path })),
        ));
        assert!(reason.contains("held by claude@shop"), "{path}: {reason}");
    }
    // Escaping must not turn the bracketed route into a hold on app/i/page.tsx.
    assert_eq!(
        env.hook(
            "pre-edit",
            "codex",
            &tool(&env.wt, "Write", json!({ "file_path": "app/i/page.tsx" }))
        ),
        ""
    );
    let patch = "*** Begin Patch\n*** Add File: src/ok.ts\n+ok\n*** Update File: app/[id]/page.tsx\n@@\n-old\n+new\n*** End Patch\n";
    assert!(denial(&env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "apply_patch", json!({ "command": patch }))
    ))
    .contains("held by claude@shop"));
    assert!(!env.holds().iter().any(|h| h["pattern"] == "src/ok.ts"));
}

#[test]
fn repeated_edits_get_a_short_backoff_and_a_free_ping() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    env.hook(
        "pre-edit",
        "claude",
        &tool(&env.main, "Edit", json!({ "file_path": "src/a.rs" })),
    );
    let edit = tool(&env.wt, "Edit", json!({ "file_path": "src/a.rs" }));
    let first = denial(&env.hook("pre-edit", "codex", &edit));
    assert!(first.contains("You'll be pinged"));
    for _ in 0..5 {
        let next = denial(&env.hook("pre-edit", "codex", &edit));
        assert!(next.starts_with("Another: this edit is still blocked."));
        assert!(!next.contains("another send"));
        assert!(next.contains("seconds"));
    }
    env.ok(&["hold", "release", "--agent", "claude@shop"]);
    let after = tool(&env.wt, "Bash", json!({ "command": "true" }));
    assert!(context(&env.hook("post-tool", "codex", &after)).contains("Hold free: src/a.rs"));
    assert_eq!(env.hook("post-tool", "codex", &after), "");
    assert_eq!(env.hook("pre-edit", "codex", &edit), "");
}

/// When the wait cannot be recorded the hook must say so and give a fallback, instead of
/// promising a ping that has no subscription behind it. A second file still gets its
/// own first notice.
#[test]
fn wait_record_failure_tells_the_agent_no_ping_is_coming() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    for path in ["src/a.rs", "src/b.rs"] {
        assert_eq!(
            env.hook(
                "pre-edit",
                "claude",
                &tool(&env.main, "Edit", json!({ "file_path": path }))
            ),
            ""
        );
    }
    let db = rusqlite::Connection::open(env.another_home.join("another.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER hold_waits_inject_failure BEFORE INSERT ON hold_waits
         BEGIN SELECT RAISE(ABORT, 'injected wait-record failure'); END;",
    )
    .unwrap();
    drop(db);

    let edit = tool(&env.wt, "Edit", json!({ "file_path": "src/a.rs" }));
    let first = denial(&env.hook("pre-edit", "codex", &edit));
    assert!(
        first.starts_with("Another stopped this edit: src/a.rs is held by claude@shop"),
        "{first}"
    );
    assert!(!first.contains("pinged"), "{first}");
    assert!(
        first.contains("could not record your wait for this file, so no inbox ping will come. Retry this edit after 30 seconds"),
        "{first}"
    );
    assert!(first.contains("another hold request-release --agent codex@shop-review"));
    for _ in 0..3 {
        let next = denial(&env.hook("pre-edit", "codex", &edit));
        assert!(
            next.starts_with(
                "Another: this edit is still blocked. src/a.rs is held by claude@shop."
            ),
            "{next}"
        );
        assert!(!next.contains("inbox ping or"), "{next}");
        assert!(
            next.contains(
                "no inbox ping will come. Retry after 30 seconds, or work on other files."
            ),
            "{next}"
        );
    }
    let other = denial(&env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "Edit", json!({ "file_path": "src/b.rs" })),
    ));
    assert!(
        other.starts_with("Another stopped this edit: src/b.rs is held by claude@shop"),
        "{other}"
    );
    assert!(other.contains("no inbox ping will come"), "{other}");
    // Nothing waits, so the release pings nobody; the retry it asked for then succeeds.
    env.ok(&["hold", "release", "--agent", "claude@shop"]);
    let after = tool(&env.wt, "Bash", json!({ "command": "true" }));
    assert_eq!(env.hook("post-tool", "codex", &after), "");
    assert_eq!(env.hook("pre-edit", "codex", &edit), "");
}

#[test]
fn pre_edit_never_turns_directory_hints_into_holds() {
    let env = Env::new();
    env.start("codex", &env.main, &[]);
    let patch = "*** Begin Patch\n*** Add File: src/new.rs\n+fn new() {}\n*** End Patch\n";
    assert_eq!(
        env.hook(
            "pre-edit",
            "codex",
            &tool(
                &env.main,
                "apply_patch",
                json!({ "path": "src", "command": patch })
            )
        ),
        ""
    );
    let holds = env.holds();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0]["pattern"], "src/new.rs");
    assert_eq!(
        env.hook(
            "pre-edit",
            "codex",
            &tool(&env.main, "Edit", json!({ "file_path": "src" }))
        ),
        ""
    );
    assert_eq!(env.holds().len(), 1);
    env.start("claude", &env.wt, &[]);
    assert_eq!(
        env.hook(
            "pre-edit",
            "claude",
            &tool(&env.wt, "Write", json!({ "file_path": "src/other.rs" }))
        ),
        "",
        "unrelated files stay free"
    );
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
    assert!(reason.contains("Worktrees of the same repository still conflict at merge"));
    assert!(
        reason.starts_with("Another stopped this edit: src/a.rs is held by"),
        "paths are project-relative: {reason}"
    );
    assert!(reason.contains("hold request-release"), "{reason}");
    assert!(reason.contains("shop:src/a.rs"), "{reason}");
    assert!(
        reason.contains("another hold request-release --agent codex@shop-review"),
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

    // A writer holding the lock: announcing needs the claim write, so the notice waits for
    // the next tool call instead of risking a duplicate. Quiet and quick, never lost.
    let db = rusqlite::Connection::open(env.another_home.join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let after = tool(&env.main, "Bash", json!({}));
    let (out, took) = env.hook_timed("post-tool", "claude", &after, &[]);
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2), "{took:?}");
    // The edit guard cannot write: it lets the edit through and logs why.
    let (out, took) = env.hook_timed("pre-edit", "claude", &edit, &[]);
    assert_eq!(out, "");
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

// ---------------------------------------------------------------------------
// Regressions for the PR #103 review (REVIEW-another-pr103-codex.md).

/// F1: an edit through a symlink is checked against the file it really reaches.
#[test]
fn symlinked_paths_do_not_bypass_a_hold() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    std::fs::write(env.wt.join("src/a.rs"), "x").unwrap();
    let held = env.hook(
        "pre-edit",
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
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("alias/a.rs") }),
        ),
    );
    let reason = denial(&out);
    assert!(
        reason.starts_with("Another stopped this edit: src/a.rs is held by claude@shop"),
        "{reason}"
    );
    // An alias outside any checkout pointing into it.
    let outside = env.root.join("outside-link");
    std::os::unix::fs::symlink(env.wt.join("src"), &outside).unwrap();
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": outside.join("a.rs") }),
        ),
    );
    assert!(denial(&out).contains("held by claude@shop"));
    // A file that is itself a symlink to the held file.
    std::os::unix::fs::symlink(env.wt.join("src/a.rs"), env.wt.join("src/link.rs")).unwrap();
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("src/link.rs") }),
        ),
    );
    assert!(denial(&out).contains("held by claude@shop"));
    // No alias hold was taken by the stopped agent.
    assert!(!env
        .holds()
        .iter()
        .any(|h| h["agent"] == "codex@shop-review"));

    // Manual holds resolve aliases too: a hold taken through `alias/` covers src/.
    let wt = env.wt.to_string_lossy().into_owned();
    let out = env
        .cmd(&[
            "hold",
            "acquire",
            "--agent",
            "codex@shop-review",
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
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let after = tool(&env.main, "Bash", json!({}));
    for round in 0..5 {
        env.send(
            "codex@shop-review",
            "claude@shop",
            &format!("round {round}"),
            false,
        );
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
    env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    env.start("codex", &env.wt, &[]);
    env.set_pane("w7:p2", Some("idle claude"));
    std::fs::write(env.herdr_dir.join("slow"), "").unwrap();
    let daemon = Daemon::start(&env);
    env.send("codex@shop-review", "claude@shop", "Race me", false);
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
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let after = tool(&env.main, "Bash", json!({}));
    env.send("codex@shop-review", "claude@shop", "warmup", false);
    assert!(!env.hook("post-tool", "claude", &after).is_empty());
    for i in 0..120 {
        env.send(
            "codex@shop-review",
            "claude@shop",
            &format!("burst {i}"),
            false,
        );
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
    env.start("claude", &env.main, &[("HERDR_PANE_ID", "w7:p2")]);
    env.start("codex", &env.wt, &[]);
    std::fs::write(env.herdr_dir.join("hang"), "").unwrap();
    let daemon = Daemon::start(&env);
    env.send("codex@shop-review", "claude@shop", "Anyone there?", false);
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
fn dot_dot_after_a_symlink_does_not_escape_the_guard() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    std::fs::create_dir_all(env.wt.join("src/deep")).unwrap();
    std::fs::write(env.wt.join("src/a.rs"), "x").unwrap();
    std::os::unix::fs::symlink(env.wt.join("src/deep"), env.wt.join("deeplink")).unwrap();
    let held = env.hook(
        "pre-edit",
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
        let out = env.hook(
            "pre-edit",
            "codex",
            &tool(&env.wt, "Edit", json!({ "file_path": path })),
        );
        assert!(
            denial(&out).contains("src/a.rs is held by claude@shop"),
            "{path}"
        );
    }
    // A plain `..` with no symlink still resolves the ordinary way.
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(&env.wt, "Edit", json!({ "file_path": "src/deep/../a.rs" })),
    );
    assert!(denial(&out).contains("held by claude@shop"));
}

/// N2: a dangling symlink to a held, not yet existing file is checked at its target.
#[test]
fn a_dangling_symlink_to_a_held_new_file_is_stopped() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let out = env.hook(
        "pre-edit",
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
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("dangle.rs") }),
        ),
    );
    assert!(
        denial(&out).contains("src/new.rs is held by claude@shop"),
        "{out}"
    );
    assert!(!env
        .holds()
        .iter()
        .any(|h| h["agent"] == "codex@shop-review"));
    // A chain of links, and a loop, do not hang or crash the hook.
    std::os::unix::fs::symlink(env.wt.join("dangle.rs"), env.wt.join("chain.rs")).unwrap();
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("chain.rs") }),
        ),
    );
    assert!(denial(&out).contains("held by claude@shop"));
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
    assert_eq!(out, "");
    assert!(took < Duration::from_secs(2));
}

/// N5: a glob hold taken through a symlinked directory protects the real files.
#[test]
fn glob_holds_through_a_symlinked_directory_are_resolved() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.main.join("src"), env.main.join("alias")).unwrap();
    let main = env.main.to_string_lossy().into_owned();
    let granted = env.json(&[
        "hold",
        "acquire",
        "--agent",
        "claude@shop",
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
        "claude@shop",
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
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Edit",
            json!({ "file_path": env.wt.join("src/b.rs") }),
        ),
    );
    assert!(denial(&out).contains("held by claude@shop"));
}

/// N4: an adapter that leaves a process behind on its stdout does not stall the caller.
#[test]
fn a_descendant_holding_stdout_does_not_outlast_the_deadline() {
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
fn a_dangling_link_after_a_missing_directory_and_dot_dot_is_followed() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let out = env.hook(
        "pre-edit",
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
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("nope/../dangle.rs") }),
        ),
    );
    assert!(
        denial(&out).contains("src/new.rs is held by claude@shop"),
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
fn a_new_file_in_another_letter_case_matches_only_where_the_volume_folds_case() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
    let out = env.hook(
        "pre-edit",
        "claude",
        &tool(
            &env.main,
            "Write",
            json!({ "file_path": env.main.join("src/new.rs") }),
        ),
    );
    assert_eq!(out, "");
    let out = env.hook(
        "pre-edit",
        "codex",
        &tool(
            &env.wt,
            "Write",
            json!({ "file_path": env.wt.join("src/NEW.RS") }),
        ),
    );
    if volume_folds_case(&env.wt) {
        assert!(
            denial(&out).contains("src/NEW.RS is held by claude@shop"),
            "{out}"
        );
    } else {
        assert_eq!(out, "", "two different files on a case-sensitive volume");
    }
}

#[test]
fn a_symlink_loop_does_not_register_a_second_project() {
    let env = Env::new();
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.wt.join("loop2"), env.wt.join("loop1")).unwrap();
    std::os::unix::fs::symlink(env.wt.join("loop1"), env.wt.join("loop2")).unwrap();
    // Spell the worktree through a symlinked parent, so a fallback to the raw path
    // would name a directory that is not the registered one.
    let alias = env.root.join("alias");
    std::os::unix::fs::symlink(env.wt.parent().unwrap(), &alias).unwrap();
    let raw = alias.join(env.wt.file_name().unwrap()).join("loop1/x.rs");
    let out = env.hook(
        "pre-edit",
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
fn case_folding_keeps_glob_character_classes_exact() {
    let env = Env::new();
    env.start("claude", &env.main, &[]);
    env.start("codex", &env.wt, &[]);
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
            "claude@shop",
            "--project",
            &main,
            glob,
        ]);
        let out = env.hook(
            "pre-edit",
            "codex",
            &tool(&env.wt, "Write", json!({ "file_path": env.wt.join(file) })),
        );
        if denied {
            assert!(
                out.contains("held by claude@shop"),
                "{glob} vs {file}: {out}"
            );
            assert!(denial(&out).contains("held by claude@shop"));
        } else {
            assert_eq!(out, "", "{glob} vs {file}");
        }
        env.json(&["hold", "release", "--agent", "claude@shop"]);
        env.json(&["hold", "release", "--agent", "codex@shop-review"]);
    }
}

#[test]
fn a_symlink_loop_then_dot_dot_does_not_register_a_second_project() {
    let env = Env::new();
    env.start("codex", &env.wt, &[]);
    std::os::unix::fs::symlink(env.wt.join("loop2"), env.wt.join("loop1")).unwrap();
    std::os::unix::fs::symlink(env.wt.join("loop1"), env.wt.join("loop2")).unwrap();
    let alias = env.root.join("alias");
    std::os::unix::fs::symlink(env.wt.parent().unwrap(), &alias).unwrap();
    let raw = alias
        .join(env.wt.file_name().unwrap())
        .join("loop1/../src/a.rs");
    env.hook(
        "pre-edit",
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
