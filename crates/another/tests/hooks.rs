//! Hook packs: install/uninstall against a temporary HOME, and `another hook run`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

// Compare the event/client arguments in both shell and encoded PowerShell commands.
fn command_text(command: &str) -> String {
    #[cfg(windows)]
    if let Some((_, token)) = command.split_once("-EncodedCommand ") {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(token)
            .unwrap();
        assert_eq!(bytes.len() % 2, 0);
        let words: Vec<_> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        let decoded = String::from_utf16(&words).unwrap();
        assert!(decoded.starts_with("& '"));
        let args: Vec<_> = decoded.rsplitn(3, " '").collect();
        assert_eq!(args.len(), 3);
        return format!(
            "{} {} --client {}",
            args[2],
            args[1].strip_suffix('\'').unwrap(),
            args[0].strip_suffix('\'').unwrap()
        );
    }
    command.to_owned()
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn script(&self) -> PathBuf {
        self.another_home.join("hooks").join(another::hooks::MARKER)
    }
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let another_home = home.join(".another");
        let repo = dir.path().join("work").join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let env = Env {
            _dir: dir,
            home,
            another_home,
            repo,
        };
        // Hooks never create a home: it exists before any session starts.
        env.ok(&["init"]);
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
            // Tests may run inside a multiplexer; keep its pane out of captured targets.
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

    fn hook(&self, event: &str, client: &str, stdin: &Value, extra_env: &[(&str, &str)]) -> String {
        let mut cmd = self.cmd(&["hook", "run", event, "--client", client]);
        for (k, v) in extra_env {
            cmd.env(k, v);
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

    fn read(&self, rel: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home.join(rel)).unwrap()).unwrap()
    }

    fn backups(&self, rel: &str) -> usize {
        let path = self.home.join(rel);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.another-backup-"))
            })
            .count()
    }

    fn write(&self, rel: &str, value: &Value) {
        let path = self.home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }
}

fn commands(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(c)) = m.get("command") {
                    out.push(command_text(c));
                }
                m.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

fn ours(v: &Value) -> usize {
    commands(v)
        .iter()
        .filter(|c| c.contains(another::hooks::MARKER))
        .count()
}

/// Entries Another installs in one client config: session start, prompt, session end and
/// post-tool everywhere, plus an advisory edit hook except for Cursor.
fn installed(rel: &str) -> usize {
    if rel.contains(".cursor") {
        4
    } else {
        5
    }
}

#[test]
fn strict_pack_is_explicit_and_switching_back_restores_advisory_pack() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "claude"]);
    let default = env.read(".claude/settings.json");
    assert_eq!(default["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert!(commands(&default)
        .iter()
        .any(|s| s.contains("pre-advisory-edit")));
    env.ok(&[
        "hook",
        "install",
        "--client",
        "claude",
        "--strict-controlled",
    ]);
    let strict = env.read(".claude/settings.json");
    assert_eq!(strict["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    assert!(strict["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["matcher"] == "^mcp__another__controlled_edit$"));
    env.ok(&["hook", "install", "--client", "claude"]);
    assert_eq!(env.read(".claude/settings.json"), default);
    for client in ["codex", "grok", "cursor", "all"] {
        assert!(!env
            .run(&["hook", "install", "--client", client, "--strict-controlled"])
            .status
            .success());
        assert_eq!(env.read(".claude/settings.json"), default);
    }
}

#[test]
fn default_advisory_route_allows_new_and_scratch_files_but_names_conflicts() {
    let env = Env::new();
    let input = json!({"session_id":"advisory-session","cwd":env.repo});
    env.hook("session-start", "claude", &input, &[]);
    let edit = |path: PathBuf| json!({"session_id":"advisory-session","cwd":env.repo,"tool_name":"Write","tool_input":{"file_path":path}});
    assert!(env
        .hook("pre-edit", "claude", &edit(env.repo.join("new.rs")), &[])
        .is_empty());
    assert!(
        !env.repo.join("new.rs").exists(),
        "advisory hook must not write the file"
    );
    assert!(env
        .hook(
            "pre-advisory-edit",
            "claude",
            &edit(env.home.join("scratch.txt")),
            &[]
        )
        .is_empty());
    let repo = env.repo.to_str().unwrap();
    env.ok(&[
        "agent",
        "register",
        "other",
        "--program",
        "other",
        "--project",
        repo,
    ]);
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "other@shop",
        "--project",
        repo,
        "held.rs",
    ]);
    let refusal = env.hook(
        "pre-advisory-edit",
        "claude",
        &edit(env.repo.join("held.rs")),
        &[],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&refusal).unwrap()["hookSpecificOutput"]
            ["permissionDecision"],
        "deny"
    );
    assert!(refusal.contains("other@shop"));
    let legacy = env.hook("pre-edit", "claude", &edit(env.repo.join("held.rs")), &[]);
    assert_eq!(
        serde_json::from_str::<Value>(&legacy).unwrap()["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
}

#[test]
fn claude_install_preserves_existing_config_and_is_idempotent() {
    let env = Env::new();
    let original = json!({
        "model": "some-model",
        "permissions": { "allow": ["Bash(ls)"] },
        "hooks": {
            "SessionStart": [{ "hooks": [{ "type": "command", "command": "echo mine" }] }],
            "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }]
        }
    });
    env.write(".claude/settings.json", &original);

    // Dry run writes nothing.
    let dry = env.ok(&[
        "--json",
        "hook",
        "install",
        "--client",
        "claude",
        "--dry-run",
    ]);
    let dry: Value = serde_json::from_str(&dry).unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(env.read(".claude/settings.json"), original);
    assert!(!env.script().exists());

    env.ok(&["hook", "install", "--client", "claude"]);
    let after = env.read(".claude/settings.json");
    assert_eq!(after["model"], "some-model");
    assert_eq!(after["permissions"], original["permissions"]);
    // The user's own PreToolUse group is untouched and stays first; ours is appended with
    // a matcher for the edit tools.
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre[0], original["hooks"]["PreToolUse"][0]);
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[1]["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
    assert!(
        command_text(pre[1]["hooks"][0]["command"].as_str().unwrap())
            .ends_with("pre-advisory-edit --client claude")
    );
    let post = after["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post.len(), 1);
    assert!(
        post[0].get("matcher").is_none(),
        "post-tool runs after every tool"
    );
    assert!(
        command_text(post[0]["hooks"][0]["command"].as_str().unwrap())
            .ends_with("post-tool --client claude")
    );
    let start = after["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        start[0]["hooks"][0]["command"], "echo mine",
        "existing hook kept first"
    );
    assert_eq!(
        ours(&after),
        5,
        "SessionStart, UserPromptSubmit, SessionEnd, PostToolUse and advisory PreToolUse"
    );
    assert!(commands(&after)
        .iter()
        .any(|c| c.ends_with("session-start --client claude")));
    assert_eq!(env.backups(".claude/settings.json"), 1);
    let script = env.script();
    let text = std::fs::read_to_string(&script).unwrap();
    #[cfg(unix)]
    assert!(text.starts_with("#!/bin/sh"));
    #[cfg(windows)]
    assert!(text.contains("[Console]::In.ReadToEnd()"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }

    // Second install: no change, no new backup.
    let again: Value =
        serde_json::from_str(&env.ok(&["--json", "hook", "install", "--client", "claude"]))
            .unwrap();
    assert!(again["changes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["change"] == "unchanged"));
    assert_eq!(env.read(".claude/settings.json"), after);
    assert_eq!(env.backups(".claude/settings.json"), 1);

    // Uninstall restores the original content exactly.
    env.ok(&["hook", "uninstall", "--client", "claude"]);
    assert_eq!(env.read(".claude/settings.json"), original);
    assert!(!script.exists(), "script removed when no client uses it");
}

#[test]
fn refuses_to_touch_invalid_or_foreign_files() {
    let env = Env::new();
    let path = env.home.join(".codex/hooks.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let out = env.run(&["hook", "install", "--client", "codex,claude"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    // All-or-nothing: claude was not configured either.
    assert!(!env.home.join(".claude/settings.json").exists());

    env.write(
        ".grok/hooks/another.json",
        &json!({ "hooks": { "Stop": [] }, "owner": "someone" }),
    );
    let out = env.run(&["hook", "install", "--client", "grok"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not written by another"));
}

#[test]
fn installs_every_client_format() {
    let env = Env::new();
    env.write(
        ".cursor/hooks.json",
        &json!({ "version": 1, "hooks": { "afterFileEdit": [{ "command": "fmt.sh" }] } }),
    );
    let out: Value =
        serde_json::from_str(&env.ok(&["--json", "hook", "install", "--client", "all"])).unwrap();
    assert!(out["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("/hooks")));

    let codex = env.read(".codex/hooks.json");
    assert_eq!(ours(&codex), 5);
    assert!(command_text(
        codex["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
    )
    .ends_with("prompt --client codex"));
    // Codex edits files through apply_patch.
    assert_eq!(
        codex["hooks"]["PreToolUse"][0]["matcher"],
        "apply_patch|Edit|Write"
    );

    let grok = env.read(".grok/hooks/another.json");
    assert_eq!(ours(&grok), 5);
    assert_eq!(
        grok["hooks"]["SessionEnd"][0]["hooks"][0]["type"],
        "command"
    );
    assert_eq!(
        grok["hooks"]["PreToolUse"][0]["matcher"],
        "Edit|Write|MultiEdit|NotebookEdit"
    );

    let cursor = env.read(".cursor/hooks.json");
    assert_eq!(cursor["version"], 1);
    assert_eq!(ours(&cursor), 4);
    assert_eq!(cursor["hooks"]["afterFileEdit"][0]["command"], "fmt.sh");
    assert!(command_text(
        cursor["hooks"]["sessionStart"][0]["command"]
            .as_str()
            .unwrap()
    )
    .ends_with("session-start --client cursor"));
    assert!(cursor["hooks"]["beforeSubmitPrompt"].is_array());
    assert!(command_text(
        cursor["hooks"]["postToolUse"][0]["command"]
            .as_str()
            .unwrap()
    )
    .ends_with("post-tool --client cursor"));
    assert!(
        cursor["hooks"].get("preToolUse").is_none(),
        "no edit guard for Cursor"
    );

    let status: Value = serde_json::from_str(&env.ok(&["--json", "hook", "status"])).unwrap();
    assert!(status
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["installed"] == true));

    // Uninstalling one client keeps the shared script for the others.
    env.ok(&["hook", "uninstall", "--client", "grok"]);
    assert!(!env.home.join(".grok/hooks/another.json").exists());
    assert!(env.script().exists());
    env.ok(&["hook", "uninstall", "--client", "all"]);
    assert_eq!(
        env.read(".cursor/hooks.json"),
        json!({ "version": 1, "hooks": { "afterFileEdit": [{ "command": "fmt.sh" }] } })
    );
    assert!(!env.script().exists());
}

fn stdin_for(repo: &Path, session: &str) -> Value {
    json!({ "session_id": session, "cwd": repo.join("src"), "hook_event_name": "SessionStart" })
}

#[test]
fn hook_run_records_sessions_and_injects_a_capped_digest() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();

    // First session start registers a distinct actor from the git root of cwd.
    let digest = env.hook("session-start", "claude", &stdin_for(&env.repo, "s1"), &[]);
    let actor = digest
        .strip_prefix("Another: ")
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    assert!(actor.starts_with("claude-s") && actor.ends_with("@shop"));
    assert!(digest.contains("has 0 unread"), "{digest}");

    // Another agent mails it; the next prompt mentions it exactly once.
    env.ok(&[
        "agent",
        "register",
        "codex",
        "--program",
        "codex",
        "--project",
        repo,
    ]);
    env.ok(&[
        "send",
        "--from",
        "codex@shop",
        "--to",
        actor,
        "--subject",
        "Rebase please",
        "-i",
        "urgent",
        "--body",
        "secret body",
    ]);
    let notice = env.hook("prompt", "claude", &stdin_for(&env.repo, "s1"), &[]);
    assert!(
        notice.contains(&format!("1 new message(s) for {actor} (1 urgent)")),
        "{notice}"
    );
    assert!(notice.contains("Rebase please"));
    assert!(!notice.contains("secret body"));
    assert_eq!(
        env.hook("prompt", "claude", &stdin_for(&env.repo, "s1"), &[]),
        ""
    );

    // A long backlog is capped.
    for i in 0..40 {
        env.ok(&[
            "send",
            "--from",
            "codex@shop",
            "--to",
            actor,
            "--subject",
            &format!("{i} {}", "x".repeat(200)),
        ]);
    }
    let second_digest = env.hook("session-start", "claude", &stdin_for(&env.repo, "s1"), &[]);
    assert!(
        second_digest.chars().count() <= 1501,
        "{}",
        second_digest.len()
    );
    assert!(
        second_digest.contains("41 unread (1 urgent)"),
        "{second_digest}"
    );

    // Unfenced hook End leaves holds intact; explicit heartbeat End releases them.
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        actor,
        "--project",
        repo,
        "src/**",
    ]);
    assert_eq!(
        env.hook("session-end", "claude", &stdin_for(&env.repo, "s1"), &[]),
        ""
    );
    let holds: Value = serde_json::from_str(&env.ok(&["--json", "holds"])).unwrap();
    assert_eq!(holds.as_array().unwrap().len(), 1);
    env.ok(&["heartbeat", "--agent", actor, "--start"]);
    env.ok(&["heartbeat", "--agent", actor, "--end"]);
    let holds: Value = serde_json::from_str(&env.ok(&["--json", "holds"])).unwrap();
    assert!(holds.as_array().unwrap().is_empty());
    let feed: Value =
        serde_json::from_str(&env.ok(&["--json", "activity", "--kinds", "session"])).unwrap();
    let summaries: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["summary"].as_str().unwrap())
        .collect();
    assert!(summaries.contains(&format!("{actor} started a session").as_str()));
    assert!(summaries.contains(&format!("{actor} ended a session").as_str()));
}

#[test]
fn cursor_and_grok_specifics() {
    let env = Env::new();
    let start = env.hook(
        "session-start",
        "cursor",
        &json!({ "conversation_id": "c1", "workspace_roots": [env.repo] }),
        &[],
    );
    let v: Value = serde_json::from_str(start.trim()).unwrap();
    assert!(v["additional_context"]
        .as_str()
        .unwrap()
        .contains("cursor-s"));
    let prompt = env.hook(
        "prompt",
        "cursor",
        &json!({ "conversation_id": "c1", "workspace_roots": [env.repo] }),
        &[],
    );
    assert_eq!(
        serde_json::from_str::<Value>(prompt.trim()).unwrap()["continue"],
        true
    );

    // Grok running Claude's hook file acts as grok when no Grok pack is installed...
    let grok_stdin = json!({ "sessionId": "g1", "workspaceRoot": env.repo });
    let out = env.hook(
        "session-start",
        "claude",
        &grok_stdin,
        &[("GROK_HOOK_EVENT", "SessionStart")],
    );
    assert!(out.contains("grok-s"), "{out}");
    // ...and stands down once the Grok pack is installed, so events run once.
    env.ok(&["hook", "install", "--client", "grok"]);
    let out = env.hook(
        "session-start",
        "claude",
        &grok_stdin,
        &[("GROK_HOOK_EVENT", "SessionStart")],
    );
    assert_eq!(out, "");
}

#[test]
fn hook_run_fails_open() {
    let env = Env::new();
    // Garbage stdin, an explicit unknown agent, and an unwritable home all exit 0. A
    // session that cannot be identified says so and starts nothing.
    let out = env.hook(
        "session-start",
        "codex",
        &json!("garbage"),
        &[("ANOTHER_AGENT", "nobody-registered")],
    );
    assert!(
        out.starts_with("Another cannot identify this session"),
        "{out}"
    );
    assert_eq!(env.ok(&["--json", "agent", "list"]).trim(), "[]");
    let mut cmd = env.cmd(&["hook", "run", "prompt", "--client", "claude"]);
    let status = cmd
        .env("ANOTHER_HOME", "/dev/null/not-a-dir")
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn installed_wrapper_script_runs_under_sh() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "codex"]);
    let codex = env.read(".codex/hooks.json");
    let command = codex["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    // Run exactly what Codex would run, with a minimal environment.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .env_clear()
        .env("HOME", &env.home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_for(&env.repo, "x").to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Another: codex-s") && text.contains("@shop has"),
        "{text}"
    );
    // ANOTHER_HOME baked in at install time was used.
    assert!(env.another_home.join("another.db").exists());
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
}

#[cfg(unix)]
#[test]
fn install_and_uninstall_preserve_config_permissions() {
    let env = Env::new();
    // Existing private configs for every client that shares a file with other settings.
    let fixtures = [
        (".claude/settings.json", json!({ "model": "x" }), 0o600),
        (".codex/hooks.json", json!({ "hooks": {} }), 0o600),
        (
            ".cursor/hooks.json",
            json!({ "version": 1, "hooks": {} }),
            0o640,
        ),
    ];
    for (rel, value, m) in &fixtures {
        env.write(rel, value);
        set_mode(&env.home.join(rel), *m);
    }

    env.ok(&["hook", "install", "--client", "all"]);
    for (rel, _, m) in &fixtures {
        let path = env.home.join(rel);
        assert_eq!(ours(&env.read(rel)), installed(rel), "{rel} installed");
        assert_eq!(mode(&path), *m, "{rel} keeps its mode after install");
    }
    // A config Another creates is private.
    assert_eq!(mode(&env.home.join(".grok/hooks/another.json")), 0o600);
    // The wrapper script is executable.
    assert_eq!(mode(&env.script()) & 0o111, 0o111);

    env.ok(&["hook", "uninstall", "--client", "all"]);
    for (rel, value, m) in &fixtures {
        let path = env.home.join(rel);
        let after = env.read(rel);
        assert_eq!(ours(&after), 0, "{rel} cleaned");
        for (k, v) in value.as_object().unwrap() {
            if k != "hooks" {
                assert_eq!(&after[k], v, "{rel} keeps {k}");
            }
        }
        assert_eq!(mode(&path), *m, "{rel} keeps its mode after uninstall");
    }
    // No temp files left behind.
    for (rel, _, _) in &fixtures {
        let tmp = env.home.join(rel).with_extension("another-tmp");
        assert!(!tmp.exists(), "{}", tmp.display());
    }
}

#[cfg(unix)]
#[test]
fn new_client_configs_are_private_and_symlinks_survive() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "claude,codex,cursor"]);
    for rel in [
        ".claude/settings.json",
        ".codex/hooks.json",
        ".cursor/hooks.json",
    ] {
        assert_eq!(mode(&env.home.join(rel)), 0o600, "{rel}");
    }

    // A settings file symlinked from a dotfiles repo is edited in place, not replaced.
    let env = Env::new();
    let dotfiles = env.home.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("claude-settings.json");
    std::fs::write(&real, "{\"theme\": \"dark\"}").unwrap();
    set_mode(&real, 0o600);
    std::fs::create_dir_all(env.home.join(".claude")).unwrap();
    let link = env.home.join(".claude/settings.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    env.ok(&["hook", "install", "--client", "claude"]);
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(ours(&v), installed(".claude/settings.json"));
    assert_eq!(mode(&real), 0o600);
    env.ok(&["hook", "uninstall", "--client", "claude"]);
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(mode(&real), 0o600);
}

/// Hook entries written by development builds from before the rename (DECISIONS D35).
fn legacy_command(event: &str, client: &str) -> String {
    format!("'/old/.wyres/hooks/wyres-hook.sh' {event} --client {client}")
}

#[test]
fn install_and_uninstall_clean_up_pre_rename_hooks() {
    let env = Env::new();
    let claude = json!({
        "hooks": {
            "SessionStart": [
                { "hooks": [{ "type": "command", "command": "echo mine" }] },
                { "hooks": [{ "type": "command", "command": legacy_command("session-start", "claude") }] }
            ],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": legacy_command("prompt", "claude") }] }]
        }
    });
    env.write(".claude/settings.json", &claude);
    env.write(
        ".cursor/hooks.json",
        &json!({ "version": 1, "hooks": { "sessionStart": [{ "command": legacy_command("session-start", "cursor") }] } }),
    );
    env.write(
        ".grok/hooks/wyres.json",
        &json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": legacy_command("session-start", "grok") }] }] } }),
    );

    // Install migrates old entries in place: one current entry per event, no old ones.
    env.ok(&["hook", "install", "--client", "claude,cursor,grok"]);
    for rel in [
        ".claude/settings.json",
        ".cursor/hooks.json",
        ".grok/hooks/another.json",
    ] {
        let v = env.read(rel);
        let cmds = commands(&v);
        assert_eq!(ours(&v), installed(rel), "{rel}: {cmds:?}");
        assert!(
            !cmds.iter().any(|c| c.contains("wyres-hook.sh")),
            "{rel}: {cmds:?}"
        );
    }
    assert!(commands(&env.read(".claude/settings.json")).contains(&"echo mine".to_string()));
    assert!(
        !env.home.join(".grok/hooks/wyres.json").exists(),
        "old Grok file removed"
    );
    assert_eq!(
        env.backups(".grok/hooks/wyres.json"),
        1,
        "and backed up first"
    );

    // Uninstall alone also removes old entries left anywhere.
    let env = Env::new();
    env.write(".claude/settings.json", &claude);
    env.write(
        ".grok/hooks/wyres.json",
        &json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": legacy_command("session-start", "grok") }] }] } }),
    );
    env.ok(&["hook", "uninstall", "--client", "all"]);
    let v = env.read(".claude/settings.json");
    assert_eq!(commands(&v), vec!["echo mine".to_string()]);
    assert!(v["hooks"].get("UserPromptSubmit").is_none());
    assert!(!env.home.join(".grok/hooks/wyres.json").exists());
}

const CODEX_LABELS: [(&str, &str); 5] = [
    ("SessionStart", "Another: registering this session"),
    ("UserPromptSubmit", "Another: checking for new messages"),
    ("PostToolUse", "Another: delivering messages mid-task"),
    ("PreToolUse", "Another: checking advisory file holds"),
    ("SessionEnd", "Another: closing this session"),
];
const CODEX_DESCRIPTION: &str =
    "Another coordination hooks (toanother.one): messages and file holds between coding agents";

/// Another's handler for one event in a matcher-group config.
fn our_handler(config: &Value, event: &str) -> Value {
    config["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["hooks"].as_array().unwrap().iter())
        .find(|h| command_text(h["command"].as_str().unwrap()).contains(another::hooks::MARKER))
        .unwrap_or_else(|| panic!("no Another hook for {event}: {config}"))
        .clone()
}

fn has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(m) => m.contains_key(key) || m.values().any(|x| has_key(x, key)),
        Value::Array(a) => a.iter().any(|x| has_key(x, key)),
        _ => false,
    }
}

fn install_changes(env: &Env, client: &str) -> Vec<String> {
    let out: Value =
        serde_json::from_str(&env.ok(&["--json", "hook", "install", "--client", client])).unwrap();
    let expected = env.home.join(".codex").join("hooks.json");
    out["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| Path::new(c["path"].as_str().unwrap()) == expected)
        .map(|c| c["change"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn codex_install_and_status_distinguish_configuration_from_trust() {
    let env = Env::new();
    let client_config = env.home.join(".codex/config.toml");
    std::fs::create_dir_all(client_config.parent().unwrap()).unwrap();
    std::fs::write(&client_config, "unrelated_setting = true\n").unwrap();
    let install = env.ok(&["hook", "install", "--client", "codex"]);
    assert!(install.contains("Codex trust is not checked"));
    assert!(install.contains("review and trust them yourself in Codex /hooks"));
    assert!(install.contains("In Codex's default trust flow"));
    assert!(install.contains("--dangerously-bypass-hook-trust"));
    assert!(install.contains("Another does not recommend it"));
    let before = std::fs::read(env.home.join(".codex/hooks.json")).unwrap();
    let status = env.ok(&["hook", "status"]);
    assert!(status.contains("Codex trust is not checked"));
    assert!(status.contains("no automatic mail or edit-hold coverage"));
    assert!(status.contains("--dangerously-bypass-hook-trust"));
    let json: Value = serde_json::from_str(&env.ok(&["--json", "hook", "status"])).unwrap();
    let codex = json
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["client"] == "codex")
        .unwrap();
    assert_eq!(codex["installed"], true);
    assert!(codex["trust_note"]
        .as_str()
        .unwrap()
        .contains("Another does not grant trust"));
    assert_eq!(
        std::fs::read(env.home.join(".codex/hooks.json")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(client_config).unwrap(),
        "unrelated_setting = true\n"
    );
}

#[test]
fn codex_hooks_are_named_for_the_trust_dialog() {
    let env = Env::new();
    let herdr = json!({ "hooks": [{ "type": "command", "command": "herdr hook session-start --agent codex", "timeout": 5 }] });
    env.write(
        ".codex/hooks.json",
        &json!({ "hooks": { "SessionStart": [herdr.clone()] } }),
    );
    assert_eq!(install_changes(&env, "codex"), ["update"]);
    let after = env.read(".codex/hooks.json");
    for (event, label) in CODEX_LABELS {
        let h = our_handler(&after, event);
        assert_eq!(h["statusMessage"], label, "{event}");
        assert_eq!(h["type"], "command");
        assert!(h["timeout"].is_number());
    }
    assert_eq!(after["description"], CODEX_DESCRIPTION);
    // The Herdr hook is kept, first and unchanged.
    assert_eq!(after["hooks"]["SessionStart"][0], herdr);
    assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(env.backups(".codex/hooks.json"), 1);

    // Installing again changes nothing and writes no backup.
    assert_eq!(install_changes(&env, "codex"), ["unchanged"]);
    assert_eq!(env.read(".codex/hooks.json"), after);
    assert_eq!(env.backups(".codex/hooks.json"), 1);

    // Uninstall removes only Another's hooks; the description and Herdr's hook stay.
    env.ok(&["hook", "uninstall", "--client", "codex"]);
    let left = env.read(".codex/hooks.json");
    assert_eq!(ours(&left), 0);
    assert_eq!(left["description"], CODEX_DESCRIPTION);
    assert_eq!(left["hooks"]["SessionStart"], json!([herdr]));
}

#[test]
fn codex_fresh_install_writes_labels_and_description() {
    let env = Env::new();
    assert_eq!(install_changes(&env, "codex"), ["create"]);
    let after = env.read(".codex/hooks.json");
    assert_eq!(ours(&after), 5);
    for (event, label) in CODEX_LABELS {
        assert_eq!(our_handler(&after, event)["statusMessage"], label);
    }
    assert_eq!(after["description"], CODEX_DESCRIPTION);
    assert_eq!(install_changes(&env, "codex"), ["unchanged"]);
}

#[test]
fn codex_keeps_a_description_the_user_set() {
    let env = Env::new();
    env.write(
        ".codex/hooks.json",
        &json!({ "description": "My own hooks", "hooks": {} }),
    );
    env.ok(&["hook", "install", "--client", "codex"]);
    assert_eq!(env.read(".codex/hooks.json")["description"], "My own hooks");
    // An empty or null description counts as missing.
    for blank in [json!(""), json!(null)] {
        env.write(
            ".codex/hooks.json",
            &json!({ "description": blank, "hooks": {} }),
        );
        env.ok(&["hook", "install", "--client", "codex"]);
        assert_eq!(
            env.read(".codex/hooks.json")["description"],
            CODEX_DESCRIPTION
        );
    }
}

#[test]
fn codex_refresh_labels_hooks_installed_before_labels() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "codex"]);
    // Strip the labels and description, as an install from before them left the file.
    let mut old = env.read(".codex/hooks.json");
    old.as_object_mut().unwrap().remove("description");
    for (event, _) in CODEX_LABELS {
        for g in old["hooks"][event].as_array_mut().unwrap() {
            for h in g["hooks"].as_array_mut().unwrap() {
                h.as_object_mut().unwrap().remove("statusMessage");
            }
        }
    }
    env.write(".codex/hooks.json", &old);
    assert!(!has_key(&old, "statusMessage"));
    assert_eq!(install_changes(&env, "codex"), ["update"]);
    let after = env.read(".codex/hooks.json");
    assert_eq!(ours(&after), 5);
    for (event, label) in CODEX_LABELS {
        assert_eq!(our_handler(&after, event)["statusMessage"], label);
    }
    assert_eq!(after["description"], CODEX_DESCRIPTION);
}

#[test]
fn only_codex_gets_labels_and_a_description() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "all"]);
    for rel in [
        ".claude/settings.json",
        ".grok/hooks/another.json",
        ".cursor/hooks.json",
    ] {
        let v = env.read(rel);
        assert!(!has_key(&v, "statusMessage"), "{rel}: {v}");
        assert!(v.get("description").is_none(), "{rel}: {v}");
    }
    assert!(has_key(&env.read(".codex/hooks.json"), "statusMessage"));
}
