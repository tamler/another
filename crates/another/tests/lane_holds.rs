//! Lane-aware holds end to end: the opt-in pre-edit hook for Claude Code and Codex
//! (install, payload parsing, deny on conflict, same-lane edits allowed) and
//! `another holds --by-lane`. Temporary homes only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use another::hooks::{edited_files, HookInput};
use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    user: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("h");
        let user = dir.path().join("u");
        let repo = dir.path().join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        Env {
            _dir: dir,
            home,
            user,
            repo,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", &self.user)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LANE")
            .env_remove("GROK_HOOK_EVENT");
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).stdin(Stdio::null()).output().unwrap();
        assert!(
            out.status.success(),
            "another {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Run the pre-edit hook as `client`, acting as `agent`; returns stdout.
    fn pre_edit(&self, client: &str, agent: &str, payload: Value) -> String {
        let mut child = self
            .cmd(&["hook", "run", "pre-edit", "--client", client])
            .env("ANOTHER_AGENT", agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hooks always exit 0");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn config(&self, rel: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.user.join(rel)).unwrap()).unwrap()
    }
}

#[test]
fn pre_edit_install_is_opt_in_and_keeps_user_hooks() {
    let env = Env::new();
    std::fs::create_dir_all(env.user.join(".claude")).unwrap();
    std::fs::write(
        env.user.join(".claude/settings.json"),
        json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }] } }).to_string(),
    )
    .unwrap();
    env.ok(&["hook", "install", "--client", "claude,codex", "--pre-edit"]);
    let claude = env.config(".claude/settings.json");
    let groups = claude["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["hooks"][0]["command"], "guard.sh");
    assert_eq!(groups[1]["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
    assert!(groups[1]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("pre-edit --client claude"));
    let codex = env.config(".codex/hooks.json");
    assert_eq!(codex["hooks"]["PreToolUse"][0]["matcher"], "apply_patch");

    // Installing again is idempotent; without --pre-edit the hook goes, the user's stays.
    env.ok(&["hook", "install", "--client", "claude", "--pre-edit"]);
    assert_eq!(
        env.config(".claude/settings.json")["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    env.ok(&["hook", "install", "--client", "claude,codex"]);
    let claude = env.config(".claude/settings.json");
    assert_eq!(
        claude["hooks"]["PreToolUse"],
        json!([{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }])
    );
    assert!(env.config(".codex/hooks.json")["hooks"]
        .get("PreToolUse")
        .is_none());
}

#[test]
fn pre_edit_holds_files_and_denies_other_lanes() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();
    for (name, program) in [
        ("claude", "claude"),
        ("codex", "codex"),
        ("helper", "claude"),
    ] {
        env.ok(&[
            "agent",
            "register",
            name,
            "--program",
            program,
            "--project",
            repo,
        ]);
    }
    env.ok(&["agent", "register", "manager", "--program", "grok_bot"]);
    env.ok(&[
        "lane",
        "create",
        "api",
        "--agent",
        "manager",
        "-p",
        repo,
        "--lead",
        "claude@shop",
    ]);
    env.ok(&[
        "lane",
        "create",
        "billing",
        "--agent",
        "manager",
        "-p",
        repo,
        "--lead",
        "codex@shop",
    ]);

    // Claude Code edits a file: held for its lane, nothing printed (normal permission flow).
    let file = env.repo.join("src/api/routes.rs");
    let out = env.pre_edit(
        "claude",
        "claude@shop",
        json!({ "cwd": env.repo, "hook_event_name": "PreToolUse", "tool_name": "Edit",
                "tool_input": { "file_path": file, "old_string": "a", "new_string": "b" } }),
    );
    assert_eq!(out, "");
    let holds: Value = serde_json::from_str(&env.ok(&["--json", "holds"])).unwrap();
    assert_eq!(holds[0]["pattern"], "src/api/routes.rs");
    assert_eq!(holds[0]["lane"], "api");
    assert_eq!(holds[0]["reason"], "pre-edit");

    // Codex in another lane patches the same file: denied, with a usable reason.
    let patch = "*** Begin Patch\n*** Update File: src/api/routes.rs\n@@\n-a\n+b\n*** End Patch\n";
    let out = env.pre_edit(
        "codex",
        "codex@shop",
        json!({ "cwd": env.repo, "hook_event_name": "PreToolUse", "tool_name": "apply_patch",
                "tool_input": { "patch": patch } }),
    );
    let v: Value = serde_json::from_str(&out).expect("deny JSON");
    let h = &v["hookSpecificOutput"];
    assert_eq!(h["hookEventName"], "PreToolUse");
    assert_eq!(h["permissionDecision"], "deny");
    let reason = h["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("src/api/routes.rs is held by claude@shop (lane api)"),
        "{reason}"
    );

    // A lane-mate (ANOTHER_LANE=api) is blocked too: both holds are exclusive.
    let run_as = |agent: &str, lane: &str, file: &std::path::Path| {
        let mut child = env
            .cmd(&["hook", "run", "pre-edit", "--client", "claude"])
            .env("ANOTHER_AGENT", agent)
            .env("ANOTHER_LANE", lane)
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
                json!({ "cwd": env.repo, "tool_name": "Write", "tool_input": { "file_path": file, "content": "x" } })
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run_as("helper@shop", "api", &file);
    let v: Value = serde_json::from_slice(&out.stdout).expect("deny JSON");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");

    // A closed or foreign lane in $ANOTHER_LANE: still held (no lane), with a visible warning.
    env.ok(&[
        "lane",
        "close",
        "billing",
        "--agent",
        "manager",
        "--abandon",
    ]);
    // codex leads another open lane: the fallback must not slip the edit into it.
    env.ok(&[
        "lane",
        "create",
        "docs",
        "--agent",
        "manager",
        "-p",
        repo,
        "--lead",
        "codex@shop",
    ]);
    let other = env.repo.join("src/billing.rs");
    let out = run_as("codex@shop", "billing", &other);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("warning JSON");
    let w = v["systemMessage"].as_str().unwrap();
    assert!(
        w.contains("$ANOTHER_LANE=billing") && w.contains("held without a lane"),
        "{w}"
    );
    assert!(v.get("hookSpecificOutput").is_none(), "not a denial");
    assert!(String::from_utf8_lossy(&out.stderr).contains("held without a lane"));
    let holds: Value =
        serde_json::from_str(&env.ok(&["--json", "holds", "--agent", "codex@shop"])).unwrap();
    let h = holds
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["pattern"] == "src/billing.rs")
        .expect("held");
    assert!(h["lane"].is_null(), "no lane, not codex's docs lane: {h}");

    // File names with glob characters are held literally: both files of the patch are held,
    // and another agent editing the bracketed one is denied.
    let patch = "*** Begin Patch\n*** Add File: docs/notes[.md\n+x\n*** Update File: docs/plain.md\n@@\n-a\n+b\n*** End Patch\n";
    let out = env.pre_edit(
        "codex",
        "codex@shop",
        json!({ "cwd": env.repo, "tool_name": "apply_patch", "tool_input": { "patch": patch } }),
    );
    assert_eq!(out, "", "held, not failed open");
    let holds: Value =
        serde_json::from_str(&env.ok(&["--json", "holds", "--agent", "codex@shop"])).unwrap();
    let pats: Vec<&str> = holds
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["pattern"].as_str().unwrap())
        .collect();
    assert!(
        pats.contains(&"docs/notes[[].md") && pats.contains(&"docs/plain.md"),
        "{pats:?}"
    );
    let out = env.pre_edit(
        "claude",
        "claude@shop",
        json!({ "cwd": env.repo, "tool_name": "Write",
                "tool_input": { "file_path": env.repo.join("docs/notes[.md"), "content": "y" } }),
    );
    let v: Value = serde_json::from_str(&out).expect("deny JSON");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");

    // Files outside the project are ignored; a non-edit payload is a no-op.
    let out = env.pre_edit(
        "claude",
        "codex@shop",
        json!({ "cwd": env.repo, "tool_name": "Edit", "tool_input": { "file_path": "/etc/hosts" } }),
    );
    assert_eq!(out, "");

    // By-lane view.
    let view = env.ok(&["holds", "--by-lane"]);
    assert!(view.starts_with("lane api [planned]"), "{view}");
    assert!(view.contains("claude@shop"), "{view}");
    let events = env.ok(&["--json", "events"]);
    assert!(events.contains("lane.hold_conflict"));
}

#[test]
fn hold_check_output_and_lane_views_know_about_the_lane() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();
    for name in ["claude", "helper"] {
        env.ok(&[
            "agent",
            "register",
            name,
            "--program",
            "claude",
            "--project",
            repo,
        ]);
    }
    env.ok(&["agent", "register", "manager", "--program", "grok_bot"]);
    env.ok(&[
        "lane",
        "create",
        "api",
        "--agent",
        "manager",
        "-p",
        repo,
        "--lead",
        "claude@shop",
    ]);
    env.ok(&[
        "hold",
        "acquire",
        "src/a.rs",
        "--shared",
        "--agent",
        "claude@shop",
        "-p",
        repo,
        "--lane",
        "api",
    ]);

    // A lane-mate's check agrees with what acquire would do.
    let check = |extra: &[&str]| {
        let mut args = vec![
            "hold",
            "check",
            "src/a.rs",
            "--agent",
            "helper@shop",
            "-p",
            repo,
        ];
        args.extend_from_slice(extra);
        env.cmd(&args).stdin(Stdio::null()).output().unwrap()
    };
    let same = check(&["--lane", "api"]);
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );
    let text = String::from_utf8_lossy(&same.stdout);
    assert!(text.contains("same lane"), "{text}");
    assert_eq!(check(&[]).status.code(), Some(3));

    // Same-lane warnings don't claim the holder is on another machine.
    let acquired = env.ok(&[
        "hold",
        "acquire",
        "src/a.rs",
        "--agent",
        "helper@shop",
        "-p",
        repo,
        "--lane",
        "api",
    ]);
    assert!(acquired.contains("same lane"), "{acquired}");
    assert!(!acquired.contains("another node"), "{acquired}");

    // The lane's live holds appear in both human views.
    let shown = env.ok(&["lane", "show", "api"]);
    assert!(shown.contains("src/a.rs"), "{shown}");
    let markdown = env.ok(&["lane", "show", "api", "--markdown"]);
    assert!(markdown.contains("src/a.rs"), "{markdown}");
}

#[cfg(unix)]
#[test]
fn symlinked_edits_respect_holds_on_the_alias_and_ignore_outside_targets() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();
    std::fs::create_dir_all(env.repo.join("src")).unwrap();
    std::fs::write(env.repo.join("src/real.rs"), "x").unwrap();
    std::os::unix::fs::symlink("real.rs", env.repo.join("src/link.rs")).unwrap();
    let outside = env.user.join("outside.txt");
    std::fs::write(&outside, "x").unwrap();
    std::os::unix::fs::symlink(&outside, env.repo.join("src/out")).unwrap();

    let input = |p: &str| {
        HookInput::parse(&json!({ "tool_input": { "file_path": env.repo.join(p) } }).to_string())
    };
    let mut files = edited_files(&input("src/link.rs"), &env.repo, &env.repo);
    files.sort();
    assert_eq!(files, vec!["src/link.rs", "src/real.rs"]);
    assert!(edited_files(&input("src/out"), &env.repo, &env.repo).is_empty());

    for name in ["claude", "codex"] {
        env.ok(&[
            "agent",
            "register",
            name,
            "--program",
            name,
            "--project",
            repo,
        ]);
    }
    env.ok(&[
        "hold",
        "acquire",
        "src/link.rs",
        "--agent",
        "codex@shop",
        "-p",
        repo,
    ]);
    let denied = env.pre_edit(
        "claude",
        "claude@shop",
        json!({
            "cwd": repo,
            "tool_name": "Edit",
            "tool_input": { "file_path": env.repo.join("src/link.rs") },
        }),
    );
    assert!(denied.contains("deny"), "{denied:?}");
}

#[test]
fn parses_edit_payloads() {
    let root = Path::new("/w/shop");
    let input = |v: Value| HookInput::parse(&v.to_string());
    let claude =
        input(json!({ "tool_name": "Edit", "tool_input": { "file_path": "/w/shop/src/a.rs" } }));
    assert_eq!(edited_files(&claude, root, root), vec!["src/a.rs"]);
    let nb = input(json!({ "tool_input": { "notebook_path": "/w/shop/n.ipynb" } }));
    assert_eq!(edited_files(&nb, root, root), vec!["n.ipynb"]);
    let patch = "*** Begin Patch\n*** Add File: new.rs\n+x\n*** Update File: src/b.rs\n*** Move to: src/c.rs\n*** Delete File: old.rs\n*** End Patch";
    for ti in [
        json!(patch),
        json!({ "patch": patch }),
        json!({ "command": ["apply_patch", patch] }),
        json!({ "input": patch }),
    ] {
        let p = input(json!({ "tool_name": "apply_patch", "tool_input": ti }));
        assert_eq!(
            edited_files(&p, root, root),
            vec!["new.rs", "src/b.rs", "src/c.rs", "old.rs"]
        );
    }
    // Relative to the cwd; `..` cannot climb out of the project.
    let sub = input(json!({ "tool_input": { "file_path": "lib.rs" } }));
    assert_eq!(
        edited_files(&sub, root, &root.join("crates/x")),
        vec!["crates/x/lib.rs"]
    );
    let escape = input(json!({ "tool_input": { "patch": "*** Update File: ../../etc/passwd" } }));
    assert!(edited_files(&escape, root, root).is_empty());
    assert!(edited_files(&input(json!({ "tool_name": "Bash" })), root, root).is_empty());
}

/// /var vs /private/var: the project root and the edited file may be spelled through
/// different symlinks; both resolve to the same real path.
#[cfg(unix)]
#[test]
fn edited_files_follow_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-shop");
    std::fs::create_dir_all(real.join("src")).unwrap();
    std::fs::write(real.join("src/a.rs"), "x").unwrap();
    let link = dir.path().join("shop");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let input =
        |p: &Path| HookInput::parse(&json!({ "tool_input": { "file_path": p } }).to_string());
    // Root through the symlink, file through the real path (and the other way round).
    assert_eq!(
        edited_files(&input(&real.join("src/a.rs")), &link, &link),
        vec!["src/a.rs"]
    );
    assert_eq!(
        edited_files(&input(&link.join("src/a.rs")), &real, &real),
        vec!["src/a.rs"]
    );
    // A file about to be created resolves through its existing parent directory.
    assert_eq!(
        edited_files(&input(&real.join("src/new.rs")), &link, &link),
        vec!["src/new.rs"]
    );
    // Outside the project stays outside.
    assert!(edited_files(&input(&dir.path().join("elsewhere.rs")), &link, &link).is_empty());
}

#[test]
fn patch_context_lines_are_not_file_headers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    // Editing a document that shows a patch: its unchanged context line looks like a header.
    let patch = "*** Begin Patch\n*** Update File: docs/howto.md\n@@\n *** Update File: src/api.rs\n-old\n+new\n*** End Patch\n";
    let input = HookInput::parse(
        &json!({ "tool_name": "apply_patch", "tool_input": { "patch": patch } }).to_string(),
    );
    assert_eq!(edited_files(&input, root, root), vec!["docs/howto.md"]);
}
