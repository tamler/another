//! What a person reads: the `another` binary without `--json`. The other CLI tests read the JSON
//! form, so these check that the text says what happened, and that secrets and bodies stay out
//! of the places they should stay out of.
//!
//! Nothing here scans or signals real processes: `orphans` runs with `--no-scan`, `reap` without
//! `--confirm`, and `wake` with `--dry-run`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::Store;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    user: PathBuf,
}

fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let (home, user) = (dir.path().join("another"), dir.path().join("user"));
    std::fs::create_dir_all(&user).unwrap();
    let h = Home {
        _dir: dir,
        home,
        user,
    };
    h.text(&["init"]);
    h
}

impl Home {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", &self.user)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LANE")
            .env_remove("ANOTHER_LOG")
            .env_remove("ANOTHER_SERVER")
            .env_remove("TMUX")
            .env_remove("CMUX_SOCKET_PATH")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Stdout of a command that must succeed.
    fn text(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.text(&all)).unwrap()
    }

    fn agents(&self) {
        for (name, program, project) in [
            ("claude", "claude", "/work/shop"),
            ("codex", "codex", "/work/shop"),
        ] {
            self.text(&[
                "agent",
                "register",
                name,
                "--program",
                program,
                "--project",
                project,
            ]);
        }
    }
}

fn has(text: &str, wanted: &[&str]) {
    for w in wanted {
        assert!(text.contains(w), "missing {w:?} in:\n{text}");
    }
}

#[test]
fn status_agents_and_nodes_read_plainly() {
    let h = home();
    let empty = h.text(&["status"]);
    has(&empty, &["node ", "database", "agents    0"]);
    assert!(!empty.contains("last seen"), "{empty}");
    h.agents();
    has(
        &h.text(&["status"]),
        &["agents    2", "claude@shop", "last seen"],
    );
    has(&h.text(&["agent", "list"]), &["codex@shop", "codex"]);
    has(&h.text(&["agent", "show", "claude@shop"]), &["claude@shop"]);
    has(&h.text(&["node", "name", "laptop"]), &["laptop"]);
    has(&h.text(&["node", "show"]), &["laptop"]);
    has(&h.text(&["peers"]), &["not paired with any node"]);
}

#[test]
fn mail_threads_and_requests_read_plainly() {
    let h = home();
    h.agents();
    let plain = h.json(&[
        "send",
        "--from",
        "claude@shop",
        "--to",
        "codex@shop",
        "--subject",
        "plan",
        "--body",
        "the plan",
    ]);
    let (mail, thread) = (
        plain["message_id"].as_str().unwrap().to_string(),
        plain["thread_id"].as_str().unwrap().to_string(),
    );
    let sent = h.text(&[
        "send",
        "--from",
        "codex@shop",
        "--to",
        "claude@shop",
        "--cc",
        "codex@shop",
        "--subject",
        "urgent thing",
        "--body",
        "now",
        "--importance",
        "urgent",
        "--ack",
        "--request",
    ]);
    has(
        &sent,
        &[
            "sent ",
            "to claude@shop",
            "(cc codex@shop)",
            "opened request",
        ],
    );

    let inbox = h.text(&["inbox", "--agent", "claude@shop"]);
    has(
        &inbox,
        &[
            "claude@shop: 1 unread (1 urgent), 1 awaiting ack, 1 request(s)",
            "!! new",
            "urgent thing",
            "requests:",
        ],
    );
    let quiet = h.text(&["inbox", "--agent", "codex@shop", "--unread"]);
    has(&quiet, &["  new", "plan"]);
    assert!(!quiet.contains("the plan"), "no bodies in a list: {quiet}");

    let urgent_id = h.json(&["inbox", "--agent", "claude@shop"])["messages"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    h.text(&["inbox", "--agent", "claude@shop", "--limit", "1"]);
    has(
        &h.text(&[
            "reply",
            &mail,
            "--from",
            "codex@shop",
            "--body",
            "looks good",
        ]),
        &["sent "],
    );
    has(
        &h.text(&["ack", "--agent", "claude@shop", &urgent_id]),
        &["1 updated, 0 unchanged"],
    );

    has(
        &h.text(&["threads", "--agent", "codex@shop"]),
        &["plan", &thread],
    );
    let shown = h.text(&["thread", &thread]);
    has(&shown, &["plan", "the plan", "looks good"]);
    has(&h.text(&["thread", &urgent_id]), &["urgent thing", "now"]);

    let requests = h.text(&["requests"]);
    has(&requests, &["open", "urgent thing"]);
    let rid = h.json(&["requests"])[0]["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    has(
        &h.text(&["request", "--agent", "claude@shop", "claim", &rid]),
        &[&rid, "claimed"],
    );
    has(
        &h.text(&["request", "--agent", "claude@shop", "complete", &rid]),
        &["done"],
    );
}

#[test]
fn holds_and_their_conflicts_read_plainly() {
    let h = home();
    h.agents();
    let acquire = |agent: &str, path: &str, extra: &[&str]| {
        let mut args = vec![
            "hold",
            "acquire",
            "--agent",
            agent,
            "--project",
            "shop",
            path,
        ];
        args.extend_from_slice(extra);
        h.run(&args)
    };
    let first = acquire("claude@shop", "src/**", &["--reason", "refactor"]);
    assert!(first.status.success());
    has(&String::from_utf8_lossy(&first.stdout), &["src/**"]);

    // Conflict: exit code 3, and it names who has what, until when.
    let clash = acquire("codex@shop", "src/main.rs", &[]);
    assert_eq!(clash.status.code(), Some(3));
    has(
        &String::from_utf8_lossy(&clash.stdout),
        &[
            "conflict: src/main.rs is held by claude@shop",
            "src/**",
            "exclusive until",
        ],
    );
    let check = h.run(&[
        "hold",
        "check",
        "--agent",
        "codex@shop",
        "--project",
        "shop",
        "src/main.rs",
    ]);
    assert_eq!(check.status.code(), Some(3));
    let free = h.text(&[
        "hold",
        "check",
        "--agent",
        "codex@shop",
        "--project",
        "shop",
        "docs/readme.md",
    ]);
    assert!(!free.contains("conflict:"), "{free}");

    has(&h.text(&["holds"]), &["src/**", "claude@shop", "refactor"]);
    has(&h.text(&["holds", "--by-lane"]), &["src/**", "claude@shop"]);
    let beat = h.text(&["heartbeat", "--agent", "claude@shop"]);
    has(&beat, &["claude@shop"]);
    has(
        &h.text(&["hold", "release", "--agent", "claude@shop"]),
        &["released"],
    );
    assert!(!h.text(&["holds"]).contains("src/**"));
    let ended = h.text(&["heartbeat", "--agent", "claude@shop", "--end"]);
    has(&ended, &["claude@shop", "offline"]);
}

#[test]
fn resources_orphans_and_reaping_read_plainly() {
    let h = home();
    h.agents();
    let reg = h.text(&[
        "resource",
        "register",
        "--agent",
        "claude@shop",
        "model_server",
        "ollama@11434",
        "--project",
        "shop",
        "--purpose",
        "local llm",
        "--ttl",
        "60",
        "--meta",
        "backend=metal",
    ]);
    has(&reg, &["registered", "ollama@11434", "claude@shop"]);
    has(
        &h.text(&["resources", "--query", "ollama"]),
        &["ollama@11434", "claude@shop"],
    );
    assert!(!h.text(&["resources", "--kind", "gpu"]).contains("ollama"));
    let target = "model_server:ollama@11434";
    has(
        &h.text(&["borrow", "--agent", "codex@shop", target]),
        &["borrowed"],
    );
    has(
        &h.text(&["resource", "heartbeat", "--agent", "codex@shop"]),
        &["ollama@11434"],
    );
    // The owner leaving hands it to the borrower.
    has(
        &h.text(&["release", "--agent", "claude@shop", target]),
        &["transferred", "ollama@11434", "codex@shop"],
    );
    has(
        &h.text(&["release", "--agent", "codex@shop", target]),
        &["released"],
    );

    // No lease left: it's an orphan candidate, reported without scanning processes.
    let orphans = h.text(&["orphans", "--no-scan"]);
    has(&orphans, &["lapsed_lease", "pid -", "port 11434"]);
    // Without --confirm nothing happens, and it says what stands in the way.
    let plan = h.text(&["reap", target]);
    has(&plan, &["target ", "ollama@11434"]);
    assert!(
        plan.contains("blocked:") || plan.contains("eligible; nothing was done"),
        "{plan}"
    );
    let unknown = h.run(&["reap", "model_server:never-registered"]);
    assert!(!unknown.status.success());
}

#[test]
fn an_empty_home_says_there_is_nothing() {
    let h = home();
    h.agents();
    has(&h.text(&["orphans", "--no-scan"]), &["no orphans found"]);
    has(&h.text(&["agent", "remotes"]), &["no remote agents"]);
    has(&h.text(&["wake", "list"]), &["no wake targets"]);
    let tokens = h.text(&["token", "list"]);
    assert!(
        tokens.trim().is_empty() || tokens.contains("no tokens"),
        "{tokens}"
    );
    let audit = h.text(&["audit"]);
    assert!(audit.trim().is_empty() || audit.contains("no "), "{audit}");
}

#[test]
fn tokens_print_their_secret_once() {
    let h = home();
    h.agents();
    let created = h.text(&[
        "token",
        "create",
        "--agent",
        "claude@shop",
        "--project",
        "/work/shop",
        "--label",
        "ci",
        "--expires",
        "1h",
    ]);
    let secret = h.json(&[
        "token",
        "create",
        "--agent",
        "codex@shop",
        "--project",
        "/work/shop",
    ])["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let first_secret = h.json(&[
        "token",
        "create",
        "--agent",
        "claude@shop",
        "--project",
        "/work/shop",
    ])["secret"]
        .as_str()
        .unwrap()
        .to_string();
    has(&created, &["claude@shop", "/work/shop"]);
    assert_eq!(
        created.split_whitespace().filter(|w| w.len() > 30).count(),
        1,
        "the human form prints one secret: {created}"
    );
    let listed = h.text(&["token", "list"]);
    has(&listed, &["claude@shop", "codex@shop", "/work/shop", "ci"]);
    assert!(!listed.contains(&secret), "list shows no secrets: {listed}");
    assert!(!listed.contains(&first_secret), "{listed}");
    let id = h.json(&["token", "list"])[0]["token_id"]
        .as_str()
        .unwrap()
        .to_string();
    h.text(&["token", "revoke", &id]);
    has(&h.text(&["token", "list"]), &["revoked"]);
}

#[test]
fn lanes_read_plainly_through_their_life() {
    let h = home();
    h.agents();
    let created = h.text(&[
        "lane",
        "create",
        "train",
        "--agent",
        "codex@shop",
        "--project",
        "shop",
        "--lead",
        "claude@shop",
        "--charter",
        "Ship the trainer.",
    ]);
    has(&created, &["train", "planned", "claude@shop"]);
    let list = h.text(&["lane", "list"]);
    has(&list, &["train", "planned", "claude@shop"]);
    let mail = h.json(&[
        "send",
        "--from",
        "claude@shop",
        "--to",
        "codex@shop",
        "--subject",
        "s",
        "--body",
        "b",
    ]);
    has(
        &h.text(&[
            "lane",
            "update",
            "train",
            "--agent",
            "codex@shop",
            "--status",
            "working",
            "--link-thread",
            mail["thread_id"].as_str().unwrap(),
        ]),
        &["working"],
    );
    has(
        &h.text(&[
            "lane",
            "report",
            "train",
            "--agent",
            "claude@shop",
            "--summary",
            "halfway there",
            "--progress",
            "50",
            "--link",
            "https://example.test/1",
            "--artifact",
            "out.bin",
        ]),
        &["reported on lane (status): halfway there"],
    );
    let shown = h.text(&["lane", "show", "train"]);
    has(
        &shown,
        &[
            "train",
            "Ship the trainer.",
            "working",
            "halfway there",
            "50%",
        ],
    );

    // The export: stdout, a file, and a directory.
    has(
        &h.text(&["lane", "export", "train"]),
        &["train", "halfway there"],
    );
    let out = h.user.join("lane.md");
    h.text(&["lane", "export", "train", "--out", out.to_str().unwrap()]);
    has(&std::fs::read_to_string(&out).unwrap(), &["halfway there"]);
    let dir = h.user.join("lanes");
    std::fs::create_dir_all(&dir).unwrap();
    h.text(&["lane", "export", "train", "--dir", dir.to_str().unwrap()]);
    assert!(dir.join("train-STATUS.md").exists());

    has(
        &h.text(&[
            "lane",
            "done",
            "train",
            "--agent",
            "claude@shop",
            "--outcome",
            "merged",
            "--pr",
            "https://example.test/pr/1",
            "--artifact",
            "final.bin",
        ]),
        &["reported on lane (done)", "merged"],
    );
    assert!(!h.text(&["lane", "list"]).contains("train"));
    has(&h.text(&["lane", "list", "--all"]), &["train", "done"]);

    h.text(&[
        "lane",
        "create",
        "side",
        "--agent",
        "codex@shop",
        "--project",
        "shop",
        "--lead",
        "codex@shop",
    ]);
    has(
        &h.text(&[
            "lane",
            "close",
            "side",
            "--agent",
            "codex@shop",
            "--abandon",
        ]),
        &["abandoned"],
    );
}

#[test]
fn activity_events_and_summaries_read_plainly() {
    let h = home();
    h.agents();
    h.text(&[
        "send",
        "--from",
        "claude@shop",
        "--to",
        "codex@shop",
        "--subject",
        "plan",
        "--body",
        "the secret plan",
    ]);
    let feed = h.text(&["activity", "--limit", "20"]);
    has(&feed, &["claude@shop", "plan"]);
    assert!(!feed.contains("the secret plan"), "no bodies: {feed}");
    has(
        &h.text(&["activity", "--kinds", "mail", "--since", "1h"]),
        &["plan"],
    );
    assert!(!h.run(&["activity", "--kinds", "nonsense"]).status.success());
    assert!(!h.run(&["activity", "--since", "whenever"]).status.success());
    has(&h.text(&["activity", "--summary"]), &["claude@shop"]);
    has(&h.text(&["events", "--limit", "5"]), &["message.sent"]);
    let swept = h.text(&["sweep"]);
    assert!(!swept.is_empty());
}

#[test]
fn wake_shows_what_it_would_do_without_doing_it() {
    let h = home();
    h.agents();
    h.text(&[
        "wake",
        "set",
        "--agent",
        "claude@shop",
        "--via",
        "tmux",
        "--target",
        "work:1.0",
    ]);
    has(
        &h.text(&["wake", "list"]),
        &["claude@shop", "tmux", "work:1.0"],
    );
    // No unread mail: skipped, even when asked.
    has(
        &h.text(&["wake", "--agent", "claude@shop", "--dry-run"]),
        &["skipped claude@shop"],
    );
    h.text(&[
        "send",
        "--from",
        "codex@shop",
        "--to",
        "claude@shop",
        "--subject",
        "s",
        "--body",
        "b",
    ]);
    let plan = h.text(&["wake", "--agent", "claude@shop", "--dry-run"]);
    has(&plan, &["would run:", "tmux", "work:1.0"]);
    let everyone = h.text(&["wake", "--all", "--dry-run"]);
    has(&everyone, &["would run:"]);
    h.text(&["wake", "clear", "--agent", "claude@shop"]);
    has(&h.text(&["wake", "list"]), &["no wake targets"]);
}

#[test]
fn hook_status_and_webhooks_read_plainly() {
    let h = home();
    h.agents();
    std::fs::create_dir_all(h.user.join(".codex")).unwrap();
    has(&h.text(&["hook", "status"]), &["codex"]);
    let installed = h.text(&["hook", "install", "--client", "codex"]);
    has(&installed, &["codex"]);
    has(&h.text(&["hook", "status"]), &["codex"]);
    has(
        &h.text(&["hook", "uninstall", "--client", "codex"]),
        &["codex"],
    );

    let added = h.text(&[
        "webhook",
        "add",
        "--url",
        "http://127.0.0.1:9/hook",
        "--events",
        "lane_status",
        "--project",
        "shop",
        "--description",
        "phone",
    ]);
    has(&added, &["secret", "127.0.0.1:9/hook"]);
    let listed = h.text(&["webhook", "list"]);
    has(&listed, &["127.0.0.1:9/hook", "lane_status"]);
    has(&h.text(&["webhook", "dead"]), &["no "]);
    has(&h.text(&["webhook", "deliveries"]), &["no "]);
    let id = h.json(&["webhook", "list"])[0]["webhook_id"]
        .as_str()
        .unwrap()
        .to_string();
    h.text(&["webhook", "remove", &id]);
    has(&h.text(&["webhook", "list"]), &["no "]);
}

// ---------------------------------------------------------------------------------------
// Against a real hub: invites, join, `another remote`, kick.

struct Hub {
    url: String,
    stop: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

fn serve(home: &Path) -> Hub {
    let store = Store::open(home).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let stop = CancellationToken::new();
    let app = http_router(
        store,
        stop.clone(),
        &HttpOptions {
            auth: true,
            federation: true,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let cancelled = stop.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { cancelled.cancelled_owned().await })
        .await
        .unwrap();
    });
    Hub {
        url: format!("http://{addr}"),
        stop,
        _rt: rt,
    }
}

#[test]
fn joining_a_hub_and_working_remotely_read_plainly() {
    let hub_home = home();
    hub_home.text(&["node", "name", "hub"]);
    hub_home.agents();
    hub_home.text(&["agent", "register", "pat", "--program", "human"]);
    let hub = serve(&hub_home.home);
    hub_home.text(&[
        "lane",
        "create",
        "train",
        "--agent",
        "codex@shop",
        "--project",
        "shop",
        "--lead",
        "codex@shop",
        "--status",
        "working",
    ]);

    let invite = hub_home.text(&[
        "invite",
        "create",
        "--agent",
        "runner",
        "--program",
        "codex",
        "--project",
        "/work/shop",
        "--lane",
        "train",
        "--hub-url",
        &hub.url,
        "--print-snippet",
    ]);
    has(
        &invite,
        &[
            "runner (codex)",
            "lane train",
            "0/1 uses",
            "code (single use, shown once): anj_",
            "another join \"$HUB\" \"$CODE\"",
        ],
    );
    let code = hub_home.json(&[
        "invite",
        "create",
        "--agent",
        "runner2",
        "--program",
        "codex",
        "--project",
        "/work/shop",
        "--lane",
        "train",
    ])["code"]
        .as_str()
        .unwrap()
        .to_string();
    let invites = hub_home.text(&["invite", "list"]);
    has(&invites, &["runner", "runner2", "/work/shop"]);
    assert!(
        !invites.contains(&code),
        "the list shows no codes: {invites}"
    );

    // The joining machine: the text tells it how to connect, without writing any config.
    let spoke = home();
    let joined = spoke.text(&["join", &hub.url, &code]);
    has(
        &joined,
        &[
            "joined ",
            "as runner2",
            "(lane train@shop)",
            "profile: ",
            "env: ",
            "MCP config (not written",
            "claude mcp add",
            "Codex (~/.codex/config.toml):",
        ],
    );

    has(
        &spoke.text(&["remote", "status"]),
        &["runner2@shop", "lane train", "0 unread"],
    );
    has(
        &spoke.text(&["remote", "heartbeat"]),
        &["runner2@shop is ", "0 hold(s) renewed"],
    );
    has(
        &spoke.text(&["remote", "lanes"]),
        &["working", "train", "shop", "codex@shop"],
    );
    has(
        &spoke.text(&["remote", "report", "--summary", "started"]),
        &["reported to lane as runner2@shop"],
    );
    has(
        &spoke.text(&["remote", "decisions"]),
        &["no open decisions in your scope"],
    );

    // A decision on the hub shows its options by number; a worker can't answer it.
    let asked = hub_home.json(&[
        "decision",
        "ask",
        "--agent",
        "codex@shop",
        "--question",
        "Ship it?",
        "--option",
        "yes=Ship it",
        "--option",
        "no=Hold off",
    ]);
    let decision = asked["decision_id"].as_str().unwrap();
    has(
        &spoke.text(&["remote", "decisions"]),
        &[decision, "Ship it?", "1. Ship it (yes)", "2. Hold off (no)"],
    );
    assert!(!spoke
        .run(&["remote", "answer", decision, "yes"])
        .status
        .success());

    has(
        &hub_home.text(&["agent", "remotes"]),
        &["runner2@shop", "online", "codex", "lane train"],
    );

    // Leaving forgets the profile, and it can't be used again.
    has(
        &spoke.text(&["remote", "leave"]),
        &[
            "left the hub as runner2@shop",
            "token(s) revoked",
            "profile forgotten",
        ],
    );
    assert!(!spoke.run(&["remote", "status"]).status.success());
    has(
        &hub_home.text(&["agent", "remotes"]),
        &["runner2@shop", "left: "],
    );

    // Kicking, on the first agent's invite.
    let kept = hub_home.json(&[
        "invite",
        "create",
        "--agent",
        "runner3",
        "--program",
        "codex",
        "--project",
        "/work/shop",
        "--persistent",
    ]);
    let again = home();
    again.text(&["join", &hub.url, kept["code"].as_str().unwrap()]);
    has(
        &hub_home.text(&["agent", "kick", "runner3@shop", "--reason", "done here"]),
        &[
            "runner3@shop removed",
            "token(s) revoked",
            "hold(s) released",
        ],
    );
    let spare = hub_home.json(&["invite", "list"]);
    let open = spare
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["agent"] == "runner")
        .unwrap()["invite_id"]
        .as_str()
        .unwrap()
        .to_string();
    hub_home.text(&["invite", "revoke", &open]);
    hub.stop.cancel();
}

#[test]
fn pairing_and_syncing_read_plainly() {
    let hub_home = home();
    hub_home.text(&["node", "name", "hub"]);
    hub_home.agents();
    let hub = serve(&hub_home.home);
    let code = hub_home.text(&[
        "pair",
        "invite",
        "--url",
        &hub.url,
        "--perms",
        "read_feed,send_mail",
        "--expires",
        "1h",
    ]);
    let code = code
        .lines()
        .find_map(|l| {
            l.split_whitespace()
                .find(|w| w.starts_with("another-invite:"))
        })
        .unwrap_or_else(|| panic!("no code in {code}"))
        .to_string();

    let spoke = home();
    let unnamed = spoke.run(&["pair", "join", &code]);
    assert!(!unnamed.status.success(), "unnamed nodes can't pair");
    spoke.text(&["node", "name", "laptop"]);
    has(&spoke.text(&["pair", "join", &code]), &["hub"]);
    spoke.text(&[
        "agent",
        "register",
        "bob",
        "--program",
        "codex",
        "--project",
        "/src/lab",
    ]);
    let sync = spoke.text(&["sync"]);
    has(&sync, &["pushed", "applied"]);
    let peers = hub_home.text(&["peers"]);
    has(&peers, &["laptop", "spoke", "read_feed"]);
    has(&spoke.text(&["peers"]), &["hub", &hub.url]);
    has(&hub_home.text(&["pair", "revoke", "laptop"]), &["laptop"]);
    has(&hub_home.text(&["peers"]), &["revoked"]);
    assert!(!spoke.run(&["sync"]).status.success());
    hub.stop.cancel();
}
