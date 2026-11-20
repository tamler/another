//! A hold pattern that cannot be read, through the real binary (Q30): the edit hook stops
//! the edit with a reason instead of dying or letting it through unheld, `hold acquire`
//! says whose hold it is and how to clear it, and a pattern over the limit is refused.
#![cfg(unix)]

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

    use std::net::SocketAddr;
    use std::process::{Command, Output, Stdio};

    use another::fed;
    use another::serve::{http_router, HttpOptions};
    use another_core::*;
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;

    struct Node {
        dir: tempfile::TempDir,
        store: Store,
    }

    fn node(name: &str) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set_node_name(name).unwrap();
        Node { dir, store }
    }

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
        let hub = node("hub");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ct = CancellationToken::new();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let opts = HttpOptions {
            federation: true,
            bound_port: addr.port(),
            ..Default::default()
        };
        let app = http_router(hub.store.clone(), ct.clone(), &opts);
        let stop = ct.clone();
        rt.spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await
            .unwrap();
        });
        let url = format!("http://{addr}");

        let mallory = node("mallory");
        let victim = node("victim");
        for spoke in [&mallory, &victim] {
            let code = hub.store.create_invite(&url, &Perm::ALL, 3600).unwrap();
            fed::join(&spoke.store, &code).unwrap();
        }
        let mal_shop = in_shop(&mallory, "mal");
        let shop = in_shop(&victim, "alice");
        mallory
            .store
            .acquire_holds(AcquireHolds {
                agent: "mal".into(),
                project: mal_shop,
                paths: vec!["src/x.rs".into()],
                ttl_seconds: None,
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
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
        ct.cancel();
    }
}
