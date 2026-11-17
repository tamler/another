//! Holds block only inside the checkout they were taken in, and a holder frees a file by
//! path (D92). Driven through the real binary: `another hook run` as a client would call
//! it, and the `hold` commands.
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    another_home: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        Env {
            another_home: home.join(".another"),
            home,
            root,
            _dir: dir,
        }
    }

    /// A repository with an origin at `root/<rel>`.
    fn repo(&self, rel: &str) -> PathBuf {
        let main = self.root.join(rel);
        let name = main.file_name().unwrap().to_string_lossy().into_owned();
        write(
            &main.join(".git/config"),
            &format!("[remote \"origin\"]\n\turl = git@example.com:team/{name}.git\n"),
        );
        std::fs::create_dir_all(main.join("src")).unwrap();
        main
    }

    /// A linked worktree of `main` at `at`, as `git worktree add` lays it out.
    fn worktree(&self, main: &Path, at: &Path) -> PathBuf {
        let name = at.file_name().unwrap().to_string_lossy().into_owned();
        let wt_git = main.join(".git/worktrees").join(&name);
        write(&at.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        std::fs::create_dir_all(at.join("src")).unwrap();
        at.to_path_buf()
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

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.cmd(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run_in(dir, args);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_in(&self.root, args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    /// Run a command that is expected to fail; returns its exit code and stderr.
    fn fails_in(&self, dir: &Path, args: &[&str]) -> (i32, String) {
        let out = self.run_in(dir, args);
        assert!(!out.status.success(), "another {args:?} should fail");
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
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
        let out: Output = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook run must always exit 0");
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, client: &str, dir: &Path) -> String {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": dir });
        self.hook("session-start", client, &stdin)
    }

    /// The pre-edit hook for one file; "" when the edit is allowed.
    fn edit(&self, client: &str, dir: &Path, file: &str) -> String {
        self.hook(
            "pre-edit",
            client,
            &tool(dir, "Edit", json!({ "file_path": file })),
        )
    }

    /// Any other tool call: the post-tool hook, whose keepalive renews aging holds.
    fn tool_call(&self, client: &str, dir: &Path) -> String {
        self.hook(
            "post-tool",
            client,
            &tool(dir, "Bash", json!({ "command": "true" })),
        )
    }

    fn holds(&self) -> Vec<Value> {
        self.json(&["holds"]).as_array().unwrap().clone()
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.json(&["events", "--limit", "10000"])
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == kind)
            .cloned()
            .collect()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.another_home.join("another.db")).unwrap()
    }

    /// Let `minutes` of clock pass for every live hold. The binary reads the system clock,
    /// so the holds are moved into the past instead, and the once-a-minute keepalive
    /// stamps are removed so the next tool call beats again.
    fn pass_minutes(&self, minutes: i64) {
        self.db()
            .execute(
                "UPDATE holds SET renewed_at = renewed_at - ?1, expires_at = expires_at - ?1
                 WHERE released_at IS NULL",
                [minutes * 60_000],
            )
            .unwrap();
        let state = self.another_home.join("hooks").join("state");
        for entry in std::fs::read_dir(state).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|e| e == "beat") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
    }

    fn project_slugs(&self) -> Vec<String> {
        self.json(&["project", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["slug"].as_str().unwrap().to_string())
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

fn denial(out: &str) -> String {
    let v: Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("expected a denial, got {out:?}: {e}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string()
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

const SHARED_FILES: [&str; 5] = [
    "src/store.rs",
    "src/lib.rs",
    "src/cli.rs",
    "README.md",
    "docs/DECISIONS.md",
];

/// A1. The failure of 17 Nov 2026: three workers in three worktrees of one repository,
/// each on its own branch, all editing the same shared files. On main the second and third
/// were stopped by the first one's holds for as long as its session made tool calls.
#[test]
fn three_workers_in_three_worktrees_never_block_each_other() {
    let env = Env::new();
    let main = env.repo("work/another");
    let seats = [
        ("claude", env.worktree(&main, &env.root.join("work/wt-a"))),
        ("codex", env.worktree(&main, &env.root.join("work/wt-b"))),
        ("grok", env.worktree(&main, &env.root.join("work/wt-c"))),
    ];
    for (client, dir) in &seats {
        env.start(client, dir);
    }
    // Interleaved: every worker edits each file before anyone moves to the next one.
    for file in SHARED_FILES {
        for (client, dir) in &seats {
            assert_eq!(env.edit(client, dir, file), "", "{client} editing {file}");
        }
    }
    // An hour of work: each session keeps making tool calls, which renews its holds, and
    // goes back to the shared files now and then.
    for round in 0..4 {
        env.pass_minutes(16);
        for (client, dir) in &seats {
            assert_eq!(env.tool_call(client, dir), "");
            let file = SHARED_FILES[round % SHARED_FILES.len()];
            assert_eq!(env.edit(client, dir, file), "", "{client} editing {file}");
        }
    }
    assert!(env.events("hold.blocked").is_empty());
    assert!(env.events("hold.waiting").is_empty());
    let holds = env.holds();
    assert_eq!(holds.len(), 15, "{holds:?}");
    for (client, dir) in &seats {
        let slug = dir.file_name().unwrap().to_string_lossy().into_owned();
        let own: Vec<&Value> = holds
            .iter()
            .filter(|h| h["agent"] == format!("{client}@{slug}"))
            .collect();
        assert_eq!(own.len(), 5, "{client}: {holds:?}");
        assert!(own.iter().all(|h| h["project"] == slug.as_str()), "{own:?}");
    }
    assert!(
        !env.events("hold.renewed").is_empty(),
        "the sessions' tool calls kept the holds alive, as on main"
    );
}

/// A2. Two clones of one repository, both on the same branch, are two checkouts.
#[test]
fn two_clones_on_one_branch_do_not_block() {
    let env = Env::new();
    let a = env.repo("clones/a/shop");
    let b = env.repo("clones/b/shop");
    for clone in [&a, &b] {
        write(&clone.join(".git/HEAD"), "ref: refs/heads/main\n");
    }
    env.start("claude", &a);
    env.start("codex", &b);
    let projects = env.json(&["project", "list"]);
    let slug_of = |dir: &Path| -> String {
        projects
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["abs_path"] == s(dir).as_str())
            .unwrap()["slug"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (claude, codex) = (
        format!("claude@{}", slug_of(&a)),
        format!("codex@{}", slug_of(&b)),
    );
    assert_eq!(env.edit("claude", &a, "src/a.rs"), "");
    assert_eq!(env.edit("codex", &b, "src/a.rs"), "");
    // A deliberate claim on an area stays inside its clone too, for `acquire` and `check`.
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        &claude,
        "--project",
        &s(&a),
        "src/**",
    ]);
    assert_eq!(env.edit("codex", &b, "src/b.rs"), "");
    let checked = env.ok(&[
        "hold",
        "check",
        "--agent",
        &codex,
        "--project",
        &s(&b),
        "src/**",
    ]);
    assert_eq!(checked.trim(), "no conflicts");
    assert!(env.events("hold.blocked").is_empty());
    assert_eq!(env.holds().len(), 4);
}

/// A3. Inside one checkout nothing changed: the second agent is stopped, a hold lasts 1800
/// seconds, and the holder's tool calls keep renewing it. Only the text is new.
#[test]
fn two_agents_in_one_checkout_block_on_the_same_file() {
    let env = Env::new();
    let shop = env.repo("work/shop");
    env.start("claude", &shop);
    env.start("codex", &shop);
    assert_eq!(env.edit("claude", &shop, "src/api/routes.rs"), "");
    let hold = env.holds().remove(0);
    assert_eq!(hold["agent"], "claude@shop");
    assert_eq!(hold["ttl_seconds"], 1800);
    let (id, until) = (
        hold["hold_id"].as_str().unwrap(),
        hold["expires_at"].as_str().unwrap(),
    );
    let reason = denial(&env.edit("codex", &shop, "src/api/routes.rs"));
    assert_eq!(
        reason,
        format!(
            "Another stopped this edit: src/api/routes.rs is held by claude@shop in this \
             checkout (exclusive hold on shop:src/api/routes.rs, \"editing (taken by the edit \
             hook)\", until {until} unless renewed). Work on other files; one message reaches \
             your Another inbox when it is free. To ask for it now: another hold \
             request-release --agent codex@shop {id}. When you finish with a file yourself, \
             free it: another hold release --agent codex@shop <path>"
        )
    );
    assert!(env.edit("codex", &shop, "src/api/routes.rs").contains(
        "Another: this edit is still blocked. src/api/routes.rs is held by claude@shop."
    ));
    assert_eq!(env.events("hold.blocked").len(), 1);
    // A different file passes.
    assert_eq!(env.edit("codex", &shop, "src/api/other.rs"), "");

    // Renewal as on main: nothing before half the hold's life, then every tool call of the
    // holder's session renews it, so the second agent stays stopped for as long as the
    // holder works.
    env.pass_minutes(10);
    assert_eq!(env.tool_call("claude", &shop), "");
    assert!(env.events("hold.renewed").is_empty());
    let expiry = |env: &Env| -> String {
        env.holds()
            .iter()
            .find(|h| h["hold_id"] == id)
            .unwrap_or_else(|| panic!("hold {id} is gone"))["expires_at"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for _ in 0..4 {
        env.pass_minutes(16);
        let before = expiry(&env);
        assert_eq!(env.tool_call("claude", &shop), "");
        assert!(expiry(&env) > before, "the keepalive renewed the hold");
        assert!(env
            .edit("codex", &shop, "src/api/routes.rs")
            .contains("still blocked"));
    }
    // And it lapses 30 minutes after the holder's last renewal, as on main.
    env.pass_minutes(31);
    assert_eq!(env.edit("codex", &shop, "src/api/routes.rs"), "");
}

/// A5. A hold never reaches into a checkout nested inside its own: a worktree under
/// `.claude/worktrees/` (where a subagent works) or a submodule.
#[test]
fn an_outer_hold_does_not_reach_a_nested_checkout() {
    let env = Env::new();
    let app = env.repo("work/app");
    let wt = env.worktree(&app, &app.join(".claude/worktrees/x"));
    let sub = app.join("vendor/lib");
    write(&sub.join(".git"), "gitdir: ../../.git/modules/lib\n");
    write(
        &app.join(".git/modules/lib/config"),
        "[remote \"origin\"]\n\turl = git@example.com:team/lib.git\n",
    );
    env.start("codex", &app);
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "codex@app",
        "--project",
        &s(&app),
        "--repo-wide",
        "**/*.rs",
        ".claude/**",
        "vendor/**",
    ]);
    // The subagent's worktree: its agent is named from the worktree's directory.
    env.start("claude", &wt);
    assert_eq!(env.edit("claude", &wt, "src/a.rs"), "");
    let held = env.holds();
    let own = held.iter().find(|h| h["agent"] == "claude@x").unwrap();
    assert_eq!(own["project"], "x");
    assert_eq!(own["pattern"], "src/a.rs");
    // The same file by its path from the main checkout is still the worktree's file.
    assert_eq!(
        env.edit("claude", &app, ".claude/worktrees/x/src/b.rs"),
        "",
        "an edit from the main checkout's directory into the worktree"
    );
    // A superproject hold does not apply inside a submodule.
    env.start("grok", &sub);
    assert_eq!(env.edit("grok", &sub, "src/lib.rs"), "");
    // And nothing of the worktree reaches the main checkout.
    env.start("grok", &app);
    assert!(
        denial(&env.edit("grok", &app, "src/a.rs")).contains("held by codex@app in this checkout")
    );
    assert_eq!(env.events("hold.blocked").len(), 1);
    // Two agents inside the nested checkout still block each other.
    env.start("codex", &wt);
    let reason = denial(&env.edit("codex", &wt, "src/a.rs"));
    assert!(
        reason.contains("src/a.rs is held by claude@x in this checkout"),
        "{reason}"
    );
}

/// A6. A hold whose pattern points into another checkout would protect nothing there, so
/// it is refused with the command that takes it in the right place.
#[test]
fn a_hold_that_points_into_another_checkout_is_refused() {
    let env = Env::new();
    let parent = env.root.join("Projects");
    let app = env.repo("Projects/test/app");
    env.ok(&[
        "agent",
        "register",
        "alice",
        "--program",
        "claude",
        "--project",
        &s(&parent),
    ]);
    let here = ["--agent", "alice@projects", "--project", &s(&parent)];
    let refused = |verb: &str, pattern: &str| -> String {
        let mut args = vec!["hold", verb];
        args.extend_from_slice(&here);
        args.push(pattern);
        let (code, stderr) = env.fails_in(&env.root, &args);
        assert_eq!(code, 1, "{stderr}");
        assert!(env.holds().is_empty(), "nothing is stored");
        stderr
    };
    let app_s = s(&app);
    let expected = format!(
        "another: test/app/src/** is inside another checkout ({app_s}); a hold taken here \
         would not apply to its files. Take it there: another hold acquire --agent \
         alice@projects --project {app_s} 'src/**'\n"
    );
    assert_eq!(refused("acquire", "test/app/src/**"), expected);
    // `hold check` would answer wrongly too, so it says the same.
    assert_eq!(refused("check", "test/app/src/**"), expected);
    // A literal path, and a directory that does not exist yet, are judged the same way.
    assert!(refused("acquire", "test/app/src/a.rs").ends_with(&format!(
        "--agent alice@projects --project {app_s} 'src/a.rs'\n"
    )));
    assert!(
        refused("acquire", "test/app/new/dir/**").ends_with(&format!(
            "--agent alice@projects --project {app_s} 'new/dir/**'\n"
        ))
    );
    // A claim on the whole repository from above it: the command carries --repo-wide, so
    // it is never one that is refused in turn.
    for whole in ["test/app", "test/app/**"] {
        assert!(
            refused("acquire", whole).ends_with(&format!(
                "Take it there: another hold acquire --agent alice@projects --project {app_s} \
                 --repo-wide '**'\n"
            )),
            "{whole}"
        );
    }
    // The commands it names are granted.
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "alice@projects",
        "--project",
        &app_s,
        "src/**",
    ]);
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "alice@projects",
        "--project",
        &app_s,
        "--repo-wide",
        "**",
    ]);
    let held = env.holds();
    assert_eq!(held.len(), 2);
    assert!(held.iter().all(|h| h["project"] == "app"), "{held:?}");
    // A pattern that starts with a wildcard has no literal directory to judge: granted, and
    // it covers files of its own checkout only.
    env.ok(&["hold", "release", "--agent", "alice@projects"]);
    let mut args = vec!["hold", "acquire"];
    args.extend_from_slice(&here);
    args.push("*/app/src/*.rs");
    env.ok(&args);
    assert_eq!(env.holds().len(), 1);
    env.start("codex", &app);
    assert_eq!(
        env.edit("codex", &app, "src/a.rs"),
        "",
        "the parent's wildcard hold does not apply inside the repository"
    );
}

/// A7. Release by path starts from the file, as the edit hook does: whatever directory the
/// caller is in, and whichever project of the checkout the hold was filed under.
#[test]
fn release_by_path_is_resolved_from_the_file() {
    let env = Env::new();
    let app = env.repo("work/app");
    let wt = env.worktree(&app, &env.root.join("work/app-wt"));
    let notes = env.root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    env.ok(&["project", "ensure", &s(&notes)]);
    env.start("claude", &app);
    env.start("codex", &app);
    let me = ["hold", "release", "--agent", "claude@app"];
    // Releases in `dir`; returns stdout and stderr. No project is ever registered by it.
    let release = |dir: &Path, rest: &[&str]| -> (String, String) {
        let before = env.project_slugs();
        let mut args = me.to_vec();
        args.extend_from_slice(rest);
        let out = env.run_in(dir, &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.project_slugs(), before, "{args:?} registered a project");
        (
            String::from_utf8(out.stdout).unwrap().trim().to_string(),
            String::from_utf8(out.stderr).unwrap().trim().to_string(),
        )
    };
    let patterns = |agent: &str| -> Vec<String> {
        let mut out: Vec<String> = env
            .holds()
            .iter()
            .filter(|h| h["agent"] == agent)
            .map(|h| h["pattern"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    };
    let acquire = |agent: &str, rest: &[&str]| {
        let mut args = vec!["hold", "acquire", "--agent", agent, "--project"];
        let dir = s(&app);
        args.push(&dir);
        args.extend_from_slice(rest);
        env.ok(&args);
    };

    // By file. The waiter is told the file is free, and the event says why it was released.
    assert_eq!(env.edit("claude", &app, "src/a.rs"), "");
    denial(&env.edit("codex", &app, "src/a.rs"));
    assert_eq!(
        release(&app, &["src/a.rs"]),
        ("released 1 hold(s)".into(), String::new())
    );
    assert!(context(&env.tool_call("codex", &app)).contains("Hold free: src/a.rs"));
    let released = env.events("hold.released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["payload"]["reason"], "done");

    // By directory: the files below it and the globs whose literal directory is below it.
    // A glob with no literal directory, and a hold elsewhere, stay.
    for file in ["src/api/x.rs", "src/api/deep/y.rs", "docs/a.md"] {
        write(&app.join(file), "");
        assert_eq!(env.edit("claude", &app, file), "");
    }
    acquire("claude@app", &["src/api/v2/**", "*.txt"]);
    assert_eq!(release(&app, &["src/api"]).0, "released 3 hold(s)");
    assert_eq!(patterns("claude@app"), ["*.txt", "docs/a.md"]);
    // Nor does the top of the checkout take a glob with no literal directory with it.
    assert_eq!(release(&app, &["."]).0, "released 1 hold(s)");
    assert_eq!(patterns("claude@app"), ["*.txt"]);
    assert_eq!(env.edit("claude", &app, "docs/a.md"), "");

    // From a subdirectory that is not a registered project, with a relative path.
    assert_eq!(env.edit("claude", &app, "src/api/z.rs"), "");
    assert_eq!(
        release(&app.join("src/api"), &["z.rs"]).0,
        "released 1 hold(s)"
    );
    assert_eq!(env.edit("claude", &app, "docs/b.md"), "");
    assert_eq!(
        release(&app.join("src/api"), &["../../docs/b.md"]).0,
        "released 1 hold(s)"
    );

    // Through a symlinked spelling of the checkout.
    let alias = env.root.join("alias");
    std::os::unix::fs::symlink(&app, &alias).unwrap();
    assert_eq!(env.edit("claude", &app, "src/b.rs"), "");
    assert_eq!(
        release(&env.root, &[&s(&alias.join("src/b.rs"))]).0,
        "released 1 hold(s)"
    );

    // By absolute path into another checkout, where the hook filed the caller's hold.
    assert_eq!(env.edit("claude", &app, &s(&wt.join("src/c.rs"))), "");
    assert!(env
        .holds()
        .iter()
        .any(|h| h["project"] == "app-wt" && h["agent"] == "claude@app"));
    assert_eq!(
        release(&app, &[&s(&wt.join("src/c.rs"))]).0,
        "released 1 hold(s)"
    );

    // Under a registered directory outside git.
    assert_eq!(env.edit("claude", &app, &s(&notes.join("todo.md"))), "");
    assert!(env.holds().iter().any(|h| h["project"] == "notes"));
    assert_eq!(release(&notes, &["todo.md"]).0, "released 1 hold(s)");
    assert_eq!(patterns("claude@app"), ["*.txt", "docs/a.md"]);

    // Another agent's hold on the same path is untouched, and the answer names the full
    // path and the checkout.
    assert_eq!(env.edit("codex", &app, "src/d.rs"), "");
    assert_eq!(
        release(&app, &["src/d.rs"]),
        (
            "released 0 hold(s)".into(),
            format!(
                "another: nothing of yours is held at {} (checkout {})",
                s(&app.join("src/d.rs")),
                s(&app)
            )
        )
    );
    assert_eq!(patterns("codex@app"), ["src/d.rs"]);

    // A file in no checkout.
    let scratch = env.root.join("scratch/x.rs");
    assert_eq!(
        release(&app, &[&s(&scratch)]),
        (
            "released 0 hold(s)".into(),
            format!(
                "another: {} is in no checkout; nothing is held there",
                s(&scratch)
            )
        )
    );

    // A path inside a wider hold of the caller's is not carved out of it.
    acquire("claude@app", &["src/wide/**"]);
    let wide = env
        .holds()
        .into_iter()
        .find(|h| h["pattern"] == "src/wide/**")
        .unwrap();
    let wide_id = wide["hold_id"].as_str().unwrap();
    assert_eq!(
        release(&app, &["src/wide/a.rs"]),
        (
            "released 0 hold(s)".into(),
            format!(
                "another: src/wide/a.rs is inside your hold src/wide/** [{wide_id}]; release \
                 that hold to free it"
            )
        )
    );

    // An argument shaped like a hold id is an id, also when no such hold exists.
    assert_eq!(release(&app, &[wide_id]).0, "released 1 hold(s)");
    let (code, stderr) = env.fails_in(&app, &[&me[..], &[wide_id]].concat());
    assert_eq!(code, 1);
    assert!(stderr.contains("no live hold"), "{stderr}");

    // An existing file is a path whatever its name contains: `src/[id].tsx` releases its
    // own hold, not the one on `src/i.tsx` a glob reading of it would name.
    for file in ["src/[id].tsx", "src/i.tsx"] {
        write(&app.join(file), "");
        assert_eq!(env.edit("claude", &app, file), "");
    }
    assert_eq!(release(&app, &["src/[id].tsx"]).0, "released 1 hold(s)");
    assert!(patterns("claude@app").contains(&"src/i.tsx".to_string()));

    // A glob argument names an own hold with exactly that pattern, in the project at or
    // above the directory; with no project there it says what to give instead.
    acquire("claude@app", &["docs/*.md"]);
    assert_eq!(
        release(&app.join("src"), &["docs/*.md"]).0,
        "released 1 hold(s)"
    );
    let nowhere = env.root.join("nowhere");
    std::fs::create_dir_all(&nowhere).unwrap();
    let (code, stderr) = env.fails_in(&nowhere, &[&me[..], &["src/*.zz"]].concat());
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        "another: no project here; give a file, a directory or a hold id\n"
    );

    // `--project` only says where relative paths start. An unregistered directory there
    // registers nothing; an unknown slug is an error.
    let unregistered = env.root.join("unregistered");
    std::fs::create_dir_all(&unregistered).unwrap();
    assert_eq!(
        release(&app, &["--project", &s(&unregistered), "x.rs"]),
        (
            "released 0 hold(s)".into(),
            format!(
                "another: {} is in no checkout; nothing is held there",
                s(&unregistered.join("x.rs"))
            )
        )
    );
    assert_eq!(
        release(&env.root, &["--project", &s(&app), "src/i.tsx"]).0,
        "released 1 hold(s)"
    );
    let (code, stderr) = env.fails_in(
        &app,
        &[&me[..], &["--project", "no-such-slug", "x.rs"]].concat(),
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("unknown project"), "{stderr}");

    // With no argument every hold under the name goes, as before.
    assert_eq!(release(&app, &[]).0, "released 2 hold(s)");
    assert!(patterns("claude@app").is_empty());
    let reasons: Vec<String> = env
        .events("hold.released")
        .iter()
        .map(|e| e["payload"]["reason"].as_str().unwrap().to_string())
        .collect();
    // Ten releases by path above; the id, the glob and the release of everything are not
    // "done with this file".
    assert_eq!(reasons.iter().filter(|r| *r == "done").count(), 10);
    assert_eq!(reasons.iter().filter(|r| *r == "released").count(), 3);
}

/// A10. What agents are told: at session start, when stopped, and when asked to release.
#[test]
fn texts() {
    let env = Env::new();
    let shop = env.repo("work/shop");
    env.start("claude", &shop);
    env.start("codex", &shop);
    // A full inbox: the hold line is the second line, so the cap never cuts it.
    for n in 0..12 {
        env.ok(&[
            "send",
            "--from",
            "codex@shop",
            "--to",
            "claude@shop",
            "--subject",
            &format!("{n} {}", "long subject ".repeat(18)),
            "--body",
            "body",
            "--request",
        ]);
    }
    for (client, agent) in [("claude", "claude@shop"), ("codex", "codex@shop")] {
        let digest = env.start(client, &shop);
        assert_eq!(
            digest.lines().nth(1).unwrap(),
            format!(
                "Files you edit here are held for you while this session works on them. Done \
                 with one? Free it for others: another hold release --agent {agent} <path>"
            ),
            "{digest}"
        );
        let length = digest.trim_end().chars().count();
        assert!(length <= 1500, "{length}");
        if client == "claude" {
            assert_eq!(length, 1500, "the inbox filled the digest: {digest}");
        }
    }
    // Cursor has no edit guard, so nothing is held for it and the line is left out.
    env.start("cursor", &shop);
    assert!(!env.start("cursor", &shop).contains("are held for you"));

    // The denial, exactly: a deliberate hold with a reason.
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "claude@shop",
        "--project",
        &s(&shop),
        "--reason",
        "router refactor",
        "src/api/**",
    ]);
    let hold = env.holds().remove(0);
    let (id, until) = (
        hold["hold_id"].as_str().unwrap(),
        hold["expires_at"].as_str().unwrap(),
    );
    let reason = denial(&env.edit("codex", &shop, "src/api/routes.rs"));
    assert_eq!(
        reason,
        format!(
            "Another stopped this edit: src/api/routes.rs is held by claude@shop in this \
             checkout (exclusive hold on shop:src/api/**, \"router refactor\", until {until} \
             unless renewed). Work on other files; one message reaches your Another inbox \
             when it is free. To ask for it now: another hold request-release --agent \
             codex@shop {id}. When you finish with a file yourself, free it: another hold \
             release --agent codex@shop <path>"
        )
    );
    assert!(!reason.to_lowercase().contains("worktree"), "{reason}");
    assert!(!reason.contains('\u{2014}') && !reason.contains('\u{2013}'));

    // The mail a release request sends the holder names the command, by hold id.
    let mail = env.json(&["hold", "request-release", "--agent", "codex@shop", id]);
    assert_eq!(
        mail["body"].as_str().unwrap(),
        format!(
            "codex@shop is waiting for shop:src/api/**. If you are done with it, free it now: \
             another hold release --agent claude@shop {id}. Another closes this request when \
             the hold ends."
        )
    );
}
