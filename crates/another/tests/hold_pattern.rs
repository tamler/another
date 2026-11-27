//! A hold pattern that cannot be read, through the real binary (Q30): the edit hook stops
//! the edit with a reason instead of dying or letting it through unheld, `hold acquire`
//! says whose hold it is and how to clear it, and a pattern over the limit is refused.
#![cfg(unix)]

#[path = "common/fed_hub.rs"]
mod fed_hub;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let repo = root.join("work").join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        Env {
            another_home: home.join(".another"),
            home,
            repo,
            _dir: dir,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .current_dir(&self.repo)
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

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    /// A session start for `client`: creates the home and registers `<client>@shop`.
    fn start(&self, client: &str) {
        let stdin = json!({ "session_id": format!("{client}-s1"), "cwd": self.repo });
        let out = self.hook("session-start", client, &stdin);
        assert!(out.status.success(), "{}", text(&out.stderr));
    }

    /// The pre-edit hook for one file, as a client calls it. The whole output, so a test
    /// can tell a denial from a hook that died.
    fn edit(&self, client: &str, file: &str) -> Output {
        let stdin = json!({
            "session_id": format!("{client}-s1"),
            "cwd": self.repo,
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": file },
        });
        self.hook("pre-edit", client, &stdin)
    }

    fn hook(&self, event: &str, client: &str, stdin: &Value) -> Output {
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
        child.wait_with_output().unwrap()
    }

    fn holds(&self) -> Vec<Value> {
        let out = self.run(&["--json", "holds"]);
        assert!(out.status.success());
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn plant(&self, hold_id: &str, pattern: &str) {
        let db = rusqlite::Connection::open(self.another_home.join("another.db")).unwrap();
        let n = db
            .execute(
                "UPDATE holds SET pattern = ?2 WHERE hold_id = ?1",
                (hold_id, pattern),
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The hook's answer must be a denial: exit 0 and the deny decision with its reason.
fn denial(out: &Output) -> String {
    assert!(
        out.status.success(),
        "the hook died instead of answering (exit {:?}, {} bytes of stderr starting {:?})",
        out.status.code(),
        out.stderr.len(),
        text(&out.stderr[..out.stderr.len().min(200)])
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "expected a denial, the hook printed {:?}: {e}",
            text(&out.stdout)
        )
    });
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string()
}

fn file(env: &Env, rel: &str) -> String {
    Path::new(&env.repo)
        .join(rel)
        .to_string_lossy()
        .into_owned()
}

/// Rules C and E through the hook and the CLI: another agent's stored pattern of 2 MiB in
/// this checkout. The edit is denied with the holder, its node and the way to clear the
/// hold; `hold acquire` refuses the same way; releasing the hold by id frees the project.
#[test]
fn the_edit_hook_denies_beside_a_stored_pattern_it_cannot_read() {
    let env = Env::new();
    env.start("codex");
    env.start("claude");
    // Codex takes an ordinary hold through its own hook, then the row is rewritten as an
    // older build could have stored it.
    let first = env.edit("codex", &file(&env, "docs/x.md"));
    assert!(first.status.success() && first.stdout.is_empty());
    let holds = env.holds();
    assert_eq!(holds.len(), 1, "{holds:?}");
    let id = holds[0]["hold_id"].as_str().unwrap().to_string();
    let holder = holds[0]["agent"].as_str().unwrap().to_string();
    env.plant(&id, &format!("src/{}", "*a".repeat(1 << 20)));

    let out = env.edit("claude", &file(&env, "src/a.rs"));
    let reason = denial(&out);
    assert!(
        out.stderr.len() < 2000,
        "{} bytes of stderr",
        out.stderr.len()
    );
    assert!(
        reason.starts_with(&format!(
            "Another stopped this edit: src/a.rs is blocked: hold {id} of {holder} on this node ("
        )),
        "{reason}"
    );
    assert!(
        reason.contains(
            "has a pattern that could not be read (\"src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…\" \
             (2097156 bytes)), so it counts as overlapping every path in project shop. To \
             clear it: another hold release --agent"
        ),
        "{reason}"
    );
    assert!(reason.len() < 1100, "{} bytes", reason.len());
    assert_eq!(env.holds().len(), 1, "the denied edit took no hold");

    // The same refusal from `hold acquire`, exit 3 like any conflict.
    let out = env.run(&["hold", "acquire", "--agent", "claude@shop", "src/b.rs"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    let printed = text(&out.stdout);
    assert!(
        printed.starts_with(&format!(
            "nothing acquired\nconflict: src/b.rs: hold {id} of {holder} on this node ("
        )),
        "{printed}"
    );
    assert!(printed.len() < 700, "{} bytes", printed.len());

    // Cleared as the text says; then the edit is held as usual.
    let out = env.run(&["hold", "release", "--agent", &holder, &id]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = env.edit("claude", &file(&env, "src/a.rs"));
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert_eq!(env.holds().len(), 1);
}

/// Rule D through the CLI: `hold check` and release by path refuse a pattern over the
/// limit with the same words as `hold acquire` (D99), and so is one of 100 KB, near the
/// longest single argument Linux passes to a program (larger ones are covered in
/// `another-core/tests/hold_pattern_limit.rs`). The edit hook's own long path is still
/// held (D99).
#[test]
fn a_pattern_over_the_limit_is_refused_by_every_hold_command_and_the_hook_still_holds() {
    let env = Env::new();
    env.start("claude");
    let first = env.edit("claude", &file(&env, "src/a.rs"));
    assert!(first.status.success() && first.stdout.is_empty());
    let pattern = format!("src/{}b", "*a".repeat(2046));
    for args in [
        vec!["hold", "acquire", "--agent", "claude@shop", &pattern],
        vec!["hold", "check", "--agent", "claude@shop", &pattern],
        vec!["hold", "release", "--agent", "claude@shop", &pattern],
    ] {
        let out = env.run(&args);
        assert_eq!(out.status.code(), Some(1), "{:?}", &args[..2]);
        assert_eq!(
            text(&out.stderr).trim(),
            "another: hold pattern is 4097 bytes; the limit is 4096",
            "{:?}",
            &args[..2]
        );
    }
    let big = format!("src/{}", "*a".repeat(50_000));
    let out = env.run(&["hold", "check", "--agent", "claude@shop", &big]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr).trim(),
        "another: hold pattern is 100004 bytes; the limit is 4096"
    );

    // 1,600 brackets in short directory names: three bytes each once escaped.
    let long = format!("src/{}x.rs", "[[[[[[[[/".repeat(200));
    let out = env.edit("claude", &file(&env, &long));
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    let holds = env.holds();
    assert_eq!(holds.len(), 2, "{holds:?}");
    assert!(holds
        .iter()
        .any(|h| h["pattern"].as_str().unwrap().len() == 5008));
}

mod over_http {
    //! The finding end to end: a hub and two spokes over real loopback HTTP.

    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

    use another::fed;
    use another_core::*;
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};

    use super::fed_hub::{join, node, start_fed_hub_with, start_http_hub, Node};

    /// Register `agent` in a project directory called `shop` under the node's own home.
    fn in_shop(n: &Node, agent: &str) -> String {
        let shop = n.dir.path().join("work").join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let shop = shop.to_string_lossy().into_owned();
        n.store
            .register_agent(RegisterAgent {
                name: agent.into(),
                program: Program::Claude,
                project: Some(shop.clone()),
                model: None,
                description: None,
            })
            .unwrap();
        shop
    }

    /// Mallory's agent `mal` takes a real hold on `src/x.rs` in its own project `shop`.
    fn mallory_holds_a_file(mallory: &Node) {
        let project = in_shop(mallory, "mal");
        mallory
            .store
            .acquire_holds(AcquireHolds {
                agent: "mal".into(),
                project,
                paths: vec!["src/x.rs".into()],
                ttl_seconds: None,
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
    }

    /// Mallory takes `holds` real holds, then every one of its `hold.acquired` events gets
    /// the pattern `pattern(i)`, signed with mallory's own key: what a peer on a changed
    /// build can send. Each pattern here is inside the limit and compiles, so a build
    /// without the change would have written the same events.
    fn mallory_holds_globs(mallory: &Node, holds: usize, pattern: impl Fn(usize) -> String) {
        let project = in_shop(mallory, "mal");
        mallory
            .store
            .acquire_holds(AcquireHolds {
                agent: "mal".into(),
                project,
                paths: (0..holds).map(|i| format!("src/f{i}.rs")).collect(),
                ttl_seconds: None,
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        let mut c = Connection::open(mallory.dir.path().join(DB_FILE)).unwrap();
        let secret: String = c
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let key = crypto::decode_secret(&secret).unwrap();
        let events: Vec<(String, String, i64, String, String)> = c
            .prepare(
                "SELECT event_id, node_id, ts, actor, payload FROM events
                 WHERE kind = 'hold.acquired' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(events.len(), holds);
        let tx = c.transaction().unwrap();
        for (i, (event_id, node_id, ts, actor, payload)) in events.into_iter().enumerate() {
            let mut p: Value = serde_json::from_str(&payload).unwrap();
            p["pattern"] = json!(pattern(i));
            let payload = p.to_string();
            let sig = crypto::sign(
                &key,
                &crypto::event_message(&event_id, &node_id, ts, &actor, "hold.acquired", &payload),
            );
            tx.execute(
                "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
                params![event_id, payload, sig],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    /// Rewrite the pattern of the node's newest `hold.acquired` in its own log, signed with
    /// the node's own key: what a changed build, or a build before the limit, could send.
    fn grow_last_hold(n: &Node, pattern: &str) -> String {
        let c = Connection::open(n.dir.path().join(DB_FILE)).unwrap();
        let (event_id, node_id, ts, actor, payload): (String, String, i64, String, String) = c
            .query_row(
                "SELECT event_id, node_id, ts, actor, payload FROM events
                 WHERE kind = 'hold.acquired' ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        let mut p: Value = serde_json::from_str(&payload).unwrap();
        p["pattern"] = json!(pattern);
        let payload = p.to_string();
        let secret: String = c
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let key = crypto::decode_secret(&secret).unwrap();
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&event_id, &node_id, ts, &actor, "hold.acquired", &payload),
        );
        c.execute(
            "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
            params![event_id, payload, sig],
        )
        .unwrap();
        event_id
    }

    fn another(home: &Node, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .arg("--home")
            .arg(home.dir.path())
            .args(args)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// A paired spoke signs a hold in a project named like the victim's, with a glob of
    /// 7 MB (`src/` then `*a` repeated), and pushes it. The hub takes it and relays it; the
    /// victim's `another sync` says in one line that it was not applied, the audit log has
    /// the row, and `another hold acquire` in that project works.
    #[test]
    fn a_peers_seven_megabyte_pattern_is_reported_by_sync_and_hold_acquire_works() {
        let hub = start_http_hub();
        let mallory = node("mallory");
        let victim = node("victim");
        join(&hub, &mallory);
        join(&hub, &victim);
        mallory_holds_a_file(&mallory);
        let shop = in_shop(&victim, "alice");
        let pattern = format!("src/{}", "*a".repeat(3_500_000));
        let event_id = grow_last_hold(&mallory, &pattern);
        let pushed = fed::sync(&mallory.store).unwrap();
        assert!(
            pushed.push_rejected.is_empty(),
            "{:?}",
            pushed.push_rejected
        );

        let out = another(&victim, &["sync"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let printed = text(&out.stdout);

        // The command the finding broke, first: it exited 101 from a panic.
        let out = another(
            &victim,
            &[
                "hold",
                "acquire",
                "--agent",
                "alice",
                "--project",
                &shop,
                "src/a.rs",
            ],
        );
        assert!(
            out.status.success(),
            "hold acquire exited {:?} with {} bytes of stderr starting {:?}",
            out.status.code(),
            out.stderr.len(),
            text(&out.stderr[..out.stderr.len().min(160)])
        );
        assert!(
            !text(&out.stdout).contains("warning"),
            "{}",
            text(&out.stdout)
        );

        let line = format!(
            "not applied {event_id}: hold.acquired from node mallory ({}) was stored and \
             relayed but not applied as a hold: its pattern \
             \"src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…\" (7000004 bytes) is longer than the \
             limit of 4096 bytes",
            mallory.store.node_id()
        );
        assert!(
            printed.lines().any(|l| l == line),
            "sync printed:\n{printed}"
        );
        assert!(printed.len() < 1500, "{} bytes", printed.len());

        let audit = text(&another(&victim, &["audit"]).stdout);
        assert!(
            audit
                .lines()
                .any(|l| l.contains("refused") && l.contains("federation hold pattern")),
            "{audit}"
        );
    }

    /// One hook run for `client` in the victim's checkout, as the client calls it.
    fn hook(victim: &Node, repo: &Path, event: &str, client: &str, tool: Option<&str>) -> Output {
        let mut stdin = json!({ "session_id": format!("{client}-s1"), "cwd": repo });
        if let Some(file) = tool {
            stdin["hook_event_name"] = json!("PreToolUse");
            stdin["tool_name"] = json!("Edit");
            stdin["tool_input"] = json!({ "file_path": repo.join(file) });
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["hook", "run", event, "--client", client])
            .current_dir(repo)
            .env("HOME", victim.dir.path().join("home"))
            .env("ANOTHER_HOME", victim.dir.path())
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("TERM_PROGRAM")
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
        child.wait_with_output().unwrap()
    }

    /// Review R1 of Q30, P1-1, through the real binaries: a peer has 1,000 live holds with
    /// an 86 byte glob in a project named like the victim's. `another hold acquire` there
    /// used to compare all of them inside the write transaction, for minutes; every
    /// pre-edit hook in that time gave up on the locked database and let its edit through,
    /// one of a file another agent holds included. Now the hooks run while the acquire
    /// does keep denying that edit, and the acquire ends at once with one count.
    #[test]
    fn a_hook_still_guards_while_hold_acquire_meets_a_thousand_cheap_peer_holds() {
        let hub = start_http_hub();
        let mallory = node("mallory");
        let victim = node("victim");
        join(&hub, &mallory);
        join(&hub, &victim);
        mallory_holds_a_file(&mallory);
        fed::sync(&mallory.store).unwrap();

        let repo = std::fs::canonicalize(victim.dir.path())
            .unwrap()
            .join("work")
            .join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        for client in ["claude", "codex"] {
            let out = hook(&victim, &repo, "session-start", client, None);
            assert!(out.status.success(), "{}", text(&out.stderr));
        }
        let out = another(&victim, &["sync"]);
        assert!(out.status.success(), "{}", text(&out.stderr));

        // The peer's one real hold becomes 1,000 with the cheap hostile pattern, as 1,000
        // of its events would have stored them (each compiles, so each is applied).
        let cheap = format!("{}[!a][!b][!c][!d]", "{a,b}".repeat(14));
        {
            let mut db = Connection::open(victim.dir.path().join(DB_FILE)).unwrap();
            let theirs: String = db
                .query_row(
                    "SELECT hold_id FROM holds
                     WHERE node_id != (SELECT node_id FROM nodes WHERE is_local = 1)",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let tx = db.transaction().unwrap();
            tx.execute(
                "UPDATE holds SET pattern = ?2 WHERE hold_id = ?1",
                params![theirs, cheap],
            )
            .unwrap();
            for i in 1..1_000 {
                tx.execute(
                    "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive,
                       reason, ttl_seconds, created_at, renewed_at, expires_at)
                     SELECT ?2, node_id, project_id, agent_id, pattern, exclusive, reason,
                       ttl_seconds, created_at, renewed_at, expires_at
                     FROM holds WHERE hold_id = ?1",
                    params![theirs, format!("{theirs}-copy-{i}")],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        // Codex holds src/b.rs; an edit of it by Claude is a denial.
        let out = hook(&victim, &repo, "pre-edit", "codex", Some("src/b.rs"));
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
        let denied = |when: &str| {
            let out = hook(&victim, &repo, "pre-edit", "claude", Some("src/b.rs"));
            let answer = text(&out.stdout);
            assert!(
                out.status.success() && answer.contains("\"permissionDecision\":\"deny\""),
                "{when}: the hook let an edit of a held file through (exit {:?}, stdout {answer:?})",
                out.status.code()
            );
        };
        denied("before the acquire");

        let started = std::time::Instant::now();
        let repo_arg = repo.to_string_lossy().into_owned();
        let mut acquire = Running::start(
            &victim,
            &[
                "hold",
                "acquire",
                "--agent",
                "claude@shop",
                "--project",
                &repo_arg,
                "src/a.rs",
            ],
        );
        // Hooks while it runs: at least three, and for as long as it does.
        let mut runs = 0;
        while runs < 3 || acquire.running() {
            denied("during the acquire");
            runs += 1;
        }
        let out = acquire.finish();
        let took = started.elapsed();
        assert!(out.status.success(), "{}", text(&out.stderr));
        // A wide bound for a debug build on a loaded machine; it was 292 s in a release build.
        assert!(took < std::time::Duration::from_secs(30), "took {took:?}");
        let printed = text(&out.stdout);
        assert!(
            printed.lines().any(|l| l
                == "warning: 1000 hold(s) on other nodes were not compared with yours (1000 from \
                    mallory): comparing with other nodes' holds stops at a fixed amount of work, \
                    so these may overlap (holds are node-local; they do not block you)"),
            "{printed}"
        );
        assert_eq!(printed.matches("warning:").count(), 1, "{printed}");
        denied("after the acquire");
    }

    /// An `another` command of the victim's that runs while the test goes on. If the test
    /// fails before it has ended, it is stopped here and not left running after the test
    /// (on a head with the regression it ran for minutes, its home already deleted).
    struct Running(Option<std::process::Child>);

    impl Running {
        fn start(home: &Node, args: &[&str]) -> Running {
            Running(Some(
                Command::new(env!("CARGO_BIN_EXE_another"))
                    .arg("--home")
                    .arg(home.dir.path())
                    .args(args)
                    .env_remove("ANOTHER_AGENT")
                    .env_remove("ANOTHER_LOG")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            ))
        }

        fn running(&mut self) -> bool {
            let child = self.0.as_mut().expect("not finished");
            child.try_wait().unwrap().is_none()
        }

        fn finish(mut self) -> Output {
            let child = self.0.take().expect("not finished");
            child.wait_with_output().unwrap()
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Review R2 of Q30, P1-A, through the real binaries: a peer sends 2,000 holds whose
    /// patterns are 4 KB globs that are slow to compile. Storing them used to compile each
    /// one inside the write transaction (2 ms a glob in a release build), so a `sync` kept
    /// the database locked for seconds and most pre-edit hooks in that time let their edit
    /// through. Nothing is compiled when an event is stored now: the hooks run while the
    /// victim's `another sync` pulls them keep denying an edit of a file another agent
    /// holds, and the sync ends well inside a bound that is wide for a debug build.
    ///
    /// The hub here closes a pull page at 16 KiB, three or four of these events, where the
    /// build's page is 500 events. A debug build stores an event about ten times slower
    /// than a release build, so a page of 500 can keep the lock past the hook's 3 s bound
    /// in a debug build; the hook then denies the edit rather than allowing it. That is a
    /// recorded limit on a large pull, not what this test is about. A page of about 30
    /// events passed on the development machine and failed on a hosted Linux runner when
    /// the hook waited only 300 ms and failed open if it missed the gaps between pages.
    /// With a few events a page the lock is taken for a few milliseconds at a time unless
    /// storing an event compiles its pattern, which is what failed before. If a page still
    /// outlasts the bound, the hook's deny keeps this test from letting a held file through.
    #[test]
    fn a_hook_still_guards_while_sync_pulls_a_peers_slow_globs() {
        let hub = start_fed_hub_with(fed::Limits {
            pull_budget: 16 * 1024,
            ..fed::Limits::CURRENT
        });
        let mallory = node("mallory");
        let victim = node("victim");
        join(&hub, &mallory);
        join(&hub, &victim);
        mallory_holds_globs(&mallory, 2_000, |i| format!("{}{i:05}", "*?".repeat(2030)));
        fed::sync(&mallory.store).unwrap();

        let repo = std::fs::canonicalize(victim.dir.path())
            .unwrap()
            .join("work")
            .join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        for client in ["claude", "codex"] {
            let out = hook(&victim, &repo, "session-start", client, None);
            assert!(out.status.success(), "{}", text(&out.stderr));
        }
        let out = hook(&victim, &repo, "pre-edit", "codex", Some("src/b.rs"));
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
        let denied = |when: &str| {
            let out = hook(&victim, &repo, "pre-edit", "claude", Some("src/b.rs"));
            let answer = text(&out.stdout);
            assert!(
                out.status.success() && answer.contains("\"permissionDecision\":\"deny\""),
                "{when}: the hook let an edit of a held file through (exit {:?}, stdout {answer:?})",
                out.status.code()
            );
        };
        denied("before the sync");

        let started = std::time::Instant::now();
        let mut sync = Running::start(&victim, &["sync"]);
        let mut runs = 0;
        while runs < 3 || sync.running() {
            denied("during the sync");
            runs += 1;
        }
        let out = sync.finish();
        let took = started.elapsed();
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
        let stored: i64 = Connection::open(victim.dir.path().join(DB_FILE))
            .unwrap()
            .query_row(
                "SELECT count(*) FROM holds
                 WHERE node_id != (SELECT node_id FROM nodes WHERE is_local = 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 2_000, "every one is applied");
        denied("after the sync");
    }
}
