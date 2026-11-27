//! The work queue through the real binary and the MCP tools: the race for `next`, the
//! paths that must never claim a proposed item, the owner's terminal, the delegate's key,
//! and the shapes of every answer. Test names and numbers follow the queue spec's test plan.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another::mcp::AnotherMcp;
#[cfg(unix)]
use another_core::QueuePriority;
use another_core::{
    AdmitItem, DoneItem, EditItem, PointerKind, Program, ProposeItem, QueueCaller, QueueFilter,
    QueueItem, QueueKind, QueueState, RegisterAgent, ReleaseItem, Store,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

type Client = RunningService<RoleClient, ClientConfig>;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn command(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    };
    cmd.args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_ADMIT_KEY")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: Store,
}

fn register(store: &Store, name: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap().unguarded_repair();
        for name in ["ann", "bob", "cy", "arch"] {
            register(&store, name);
        }
        Fx {
            _dir: dir,
            home,
            store,
        }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        command(&self.home, args, env)
            .output()
            .expect("run another")
    }

    /// Run with `--json`; return the exit code and what was printed on stdout.
    fn json(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all, env);
        let stdout = text(&out.stdout);
        let v = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?} printed no JSON ({e}): {stdout:?} {:?}",
                text(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), v)
    }

    /// A command that must fail: its exit code and its error code.
    fn fails(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        let (code, v) = self.json(args, env);
        let error = v["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("{args:?} did not fail: {v}"))
            .to_string();
        (code, error)
    }

    fn message(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let (_, v) = self.json(args, env);
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn propose(&self, who: &str, title: &str, kind: QueueKind) -> QueueItem {
        self.store
            .queue_propose(
                &QueueCaller::agent(who),
                ProposeItem {
                    title: title.into(),
                    scope: Some("M1".into()),
                    done_when: Some("the tests pass".into()),
                    kind,
                    ..Default::default()
                },
            )
            .unwrap()
    }

    fn ready(&self, title: &str) -> QueueItem {
        let item = self.propose("ann", title, QueueKind::Work);
        self.store
            .queue_admit(&QueueCaller::Owner, &item.label, AdmitItem::default())
            .unwrap()
    }

    fn item(&self, item: &str) -> QueueItem {
        self.store.queue_item(item).unwrap()
    }

    /// Make `arch` a delegate; return its key.
    fn delegate_key(&self) -> String {
        self.store
            .queue_delegate_add(&QueueCaller::Owner, "arch")
            .unwrap()
            .key
    }

    fn events(&self, kind: &str) -> usize {
        self.store
            .events(100_000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .count()
    }

    /// Everything the queue holds, as text: equal snapshots mean nothing changed.
    fn snapshot(&self) -> String {
        let mut items = Vec::new();
        for state in [
            QueueState::Proposed,
            QueueState::Ready,
            QueueState::Claimed,
            QueueState::Done,
        ] {
            items.extend(
                self.store
                    .queue_list(QueueFilter {
                        state: Some(state),
                        limit: 1000,
                        ..Default::default()
                    })
                    .unwrap(),
            );
        }
        let full: Vec<QueueItem> = items.iter().map(|i| self.item(&i.item_id)).collect();
        json!({
            "items": full,
            "limits": self.store.queue_limits().unwrap(),
            "delegates": self.store.queue_delegates().unwrap(),
            "grace": self.store.queue_grace().unwrap(),
        })
        .to_string()
    }
}

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::legacy_repair(store)
            .serve(server_io)
            .await
            .unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().expect("object args")),
        )
        .await
        .expect("tool call")
}

async fn call_ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

async fn call_err(client: &Client, tool: &str, args: Value) -> String {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_str(&text).unwrap();
    body["error"]["code"].as_str().unwrap().to_string()
}

/// Run the binary on a pseudo-terminal with `typed` as the keys pressed. Returns what the
/// terminal showed. `script` is the standard tool for this on macOS and Linux.
#[cfg(unix)]
fn in_terminal(home: &Path, args: &[&str], typed: &str) -> String {
    in_terminal_then(home, args, typed, || {})
}

/// [`in_terminal`], running `at_prompt` while the prompt is on the screen, before any key
/// is pressed.
#[cfg(unix)]
fn in_terminal_then(home: &Path, args: &[&str], typed: &str, at_prompt: impl FnOnce()) -> String {
    use std::io::Write;
    let mut at_prompt = Some(at_prompt);
    let exe = env!("CARGO_BIN_EXE_another");
    let mut cmd = Command::new("script");
    if cfg!(target_os = "linux") {
        let quoted: Vec<String> = std::iter::once(exe)
            .chain(args.iter().copied())
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect();
        cmd.args(["-qec", &quoted.join(" "), "/dev/null"]);
    } else {
        cmd.args(["-q", "/dev/null", exe]).args(args);
    }
    let mut child = cmd
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_ADMIT_KEY")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run script(1)");
    // Type only once the prompt is on the screen, as a person would, and keep the keyboard
    // open until the command has finished. Empty `typed` closes it at once: end of input.
    let mut keyboard = child.stdin.take();
    let mut screen = child.stdout.take().unwrap();
    let mut shown = Vec::new();
    let mut byte = [0u8; 1];
    if typed.is_empty() {
        keyboard = None;
    }
    while let Ok(1) = std::io::Read::read(&mut screen, &mut byte) {
        shown.push(byte[0]);
        if shown.ends_with(b"Type yes to confirm: ") {
            if let Some(act) = at_prompt.take() {
                act();
            }
            if let Some(keys) = keyboard.as_mut() {
                keys.write_all(typed.as_bytes()).unwrap();
                keys.flush().unwrap();
            }
        }
    }
    drop(keyboard);
    let out = child.wait_with_output().unwrap();
    format!("{}{}", text(&shown), text(&out.stderr))
}

/// Review R1, P2-1: the owner's yes applies only to the item as the prompt showed it. An
/// edit that lands while the prompt is open makes the write fail, for every owner action
/// on an item.
#[cfg(unix)]
#[test]
fn the_owners_yes_applies_only_to_what_was_shown() {
    let fx = Fx::new();
    let key = fx.delegate_key();
    let arch = QueueCaller::Agent {
        agent: "arch".into(),
        admit_key: Some(key),
    };
    let item = fx.propose("ann", "shown to the owner", QueueKind::Work);
    let q = item.label.as_str();
    let edit_now = |done_when: &str| {
        fx.store
            .queue_edit(
                &arch,
                q,
                EditItem {
                    done_when: Some(done_when.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    };
    let refused = format!("{q} changed while you were deciding");

    // Admit: a delegate edits the done condition while the prompt is open.
    let shown = in_terminal_then(&fx.home, &["queue", "admit", q, "--owner"], "yes\n", || {
        edit_now("something the owner never saw")
    });
    assert!(shown.contains("the tests pass"), "{shown}");
    assert!(!shown.contains("never saw"), "{shown}");
    assert!(shown.contains(&refused), "{shown}");
    let now = fx.item(q);
    assert_eq!(now.state, QueueState::Proposed, "{shown}");
    assert!(now.admitted.is_none());
    assert_eq!(fx.events("queue.admitted"), 0);
    // A note is a change too: the owner has not read it.
    let shown = in_terminal_then(&fx.home, &["queue", "admit", q, "--owner"], "yes\n", || {
        fx.store
            .queue_note("bob", q, "wait, this is wrong")
            .unwrap();
    });
    assert!(shown.contains(&refused), "{shown}");
    assert_eq!(fx.item(q).state, QueueState::Proposed);

    // Reject and edit are guarded the same way.
    let shown = in_terminal_then(
        &fx.home,
        &["queue", "reject", q, "--reason", "no", "--owner"],
        "yes\n",
        || edit_now("changed before the reject"),
    );
    assert!(shown.contains(&refused), "{shown}");
    assert_eq!(fx.item(q).state, QueueState::Proposed);
    let shown = in_terminal_then(
        &fx.home,
        &["queue", "edit", q, "--priority", "high", "--owner"],
        "yes\n",
        || edit_now("changed before the edit"),
    );
    assert!(shown.contains(&refused), "{shown}");
    assert_eq!(fx.item(q).priority, QueuePriority::Normal);

    // With nothing changed in between, the yes admits what was shown.
    let shown = in_terminal(&fx.home, &["queue", "admit", q, "--owner"], "yes\n");
    assert!(shown.contains("changed before the edit"), "{shown}");
    let now = fx.item(q);
    assert_eq!(now.state, QueueState::Ready, "{shown}");
    assert_eq!(
        now.done_condition.as_deref(),
        Some("changed before the edit")
    );

    // Release: the claim the owner was shown is given back and taken by someone else.
    fx.store.queue_next("bob").unwrap().claimed.unwrap();
    let shown = in_terminal_then(
        &fx.home,
        &["queue", "release", q, "--reason", "back", "--owner"],
        "yes\n",
        || {
            fx.store
                .queue_release(&QueueCaller::agent("bob"), q, ReleaseItem::default())
                .unwrap();
            fx.store.queue_next("cy").unwrap().claimed.unwrap();
        },
    );
    assert!(shown.contains("claimed by      bob"), "{shown}");
    assert!(shown.contains(&refused), "{shown}");
    assert_eq!(fx.item(q).claim.unwrap().agent, "cy", "cy keeps its claim");

    // The command's own values are checked before the owner is asked anything, and what
    // is printed at the prompt carries no escape sequence.
    for args in [
        vec!["queue", "grace", "1h", "--owner"],
        vec!["queue", "grace", "soon", "--owner"],
        vec!["queue", "limit", "set", "bob", "99", "--owner"],
        vec!["queue", "limit", "default", "21", "--owner"],
        vec![
            "queue",
            "reject",
            q,
            "--reason",
            "x\u{1b}[2K\rAdmit",
            "--owner",
        ],
        vec!["queue", "edit", q, "--scope", "not a scope id", "--owner"],
        vec!["queue", "propose", "--title", "bad\u{1b}[2J", "--owner"],
    ] {
        let shown = in_terminal(&fx.home, &args, "yes\n");
        assert!(!shown.contains("Type yes to confirm"), "{args:?}: {shown}");
        assert!(!shown.contains('\u{1b}'), "{args:?}: {shown:?}");
    }
    assert_eq!(fx.store.queue_grace().unwrap().grace_seconds, 86_400);
    assert_eq!(fx.store.queue_limits().unwrap().default_limit, 1);
}

// T2
#[test]
fn fifty_processes_racing_for_next_never_share_an_item() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap().unguarded_repair();
    let names: Vec<String> = (0..50).map(|i| format!("w{i:02}")).collect();
    for name in &names {
        register(&store, name);
    }
    let mut items = BTreeSet::new();
    for i in 0..20 {
        let item = store
            .queue_propose(
                &QueueCaller::Owner,
                ProposeItem {
                    title: format!("item {i}"),
                    scope: Some("M1".into()),
                    done_when: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .queue_admit(&QueueCaller::Owner, &item.label, AdmitItem::default())
            .unwrap();
        items.insert(item.item_id);
    }
    let spawn = |name: &str| {
        command(&home, &["--json", "queue", "next", "--agent", name], &[])
            .spawn()
            .expect("spawn another")
    };
    let mut running: Vec<(String, std::process::Child)> =
        names.iter().map(|n| (n.clone(), spawn(n))).collect();
    let mut claimed: Vec<String> = Vec::new();
    let mut empty = 0;
    while let Some((name, child)) = running.pop() {
        let out = child.wait_with_output().unwrap();
        let v: Value = serde_json::from_str(&text(&out.stdout))
            .unwrap_or_else(|e| panic!("{name}: {e}: {:?}", text(&out.stderr)));
        if v["error"]["code"] == "db" {
            // The write lock was not obtained in time: nothing was claimed; ask again.
            running.push((name.clone(), spawn(&name)));
            continue;
        }
        assert!(v.get("error").is_none(), "{name}: {v}");
        assert_eq!(out.status.code(), Some(0), "{name}: {v}");
        assert!(v["refused"].is_null(), "{name}: {v}");
        match v["claimed"]["item_id"].as_str() {
            Some(id) => claimed.push(id.to_string()),
            None => empty += 1,
        }
    }
    let distinct: BTreeSet<String> = claimed.iter().cloned().collect();
    assert_eq!(claimed.len(), 20, "twenty claims");
    assert_eq!(
        distinct, items,
        "the claims are disjoint and cover the twenty items"
    );
    assert_eq!(empty, 30);
    let audit = store.queue_audit(None).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    assert_eq!(audit.claims, 20);
}

// T4
#[tokio::test]
async fn no_cli_or_mcp_path_claims_a_proposed_item() {
    let fx = Fx::new();
    let item = fx.propose("ann", "not admitted", QueueKind::Work);
    let q = item.label.as_str();
    let id = item.item_id.as_str();
    let bob = ["--agent", "bob"];

    // The command line.
    let (code, v) = fx.json(&["queue", "next", "--agent", "bob"], &[]);
    assert_eq!(code, 0);
    assert_eq!(v, json!({ "claimed": null, "refused": null }));
    for target in [q, id] {
        let done = [
            "queue",
            "done",
            target,
            "--summary",
            "s",
            "--commit",
            "abc1234",
        ];
        assert_eq!(
            fx.fails(&[&done[..], &bob[..]].concat(), &[]),
            (3, "conflict".into())
        );
        let release = ["queue", "release", target];
        assert_eq!(
            fx.fails(&[&release[..], &bob[..]].concat(), &[]),
            (3, "conflict".into())
        );
        let handoff = ["queue", "release", target, "--to", "cy"];
        assert_eq!(
            fx.fails(&[&handoff[..], &bob[..]].concat(), &[]),
            (3, "conflict".into())
        );
    }
    // A queue item is not a request.
    assert_eq!(
        fx.fails(&["request", "claim", id, "--agent", "bob"], &[]),
        (1, "not_found".into())
    );
    // An agent admits nothing, with no key, with a made-up key, or by claiming to be the owner.
    let admit = ["queue", "admit", q, "--agent", "bob"];
    assert_eq!(fx.fails(&admit, &[]), (1, "forbidden".into()));
    let fake = format!("adk_{}", "0".repeat(64));
    assert_eq!(
        fx.fails(&admit, &[("ANOTHER_ADMIT_KEY", &fake)]),
        (1, "forbidden".into())
    );
    assert_eq!(
        fx.fails(&["queue", "admit", q, "--owner"], &[]),
        (1, "forbidden".into())
    );

    // The MCP tools.
    let client = in_process(Store::open(&fx.home).unwrap().unguarded_repair()).await;
    let next = call_ok(&client, "queue_next", json!({ "agent": "bob" })).await;
    assert_eq!(next, json!({ "claimed": null, "refused": null }));
    for target in [q, id] {
        for (action, extra) in [
            ("release", json!({})),
            ("handoff", json!({ "to": "cy" })),
            (
                "done",
                json!({ "summary": "s", "pointer_kind": "commit", "pointer": "abc1234" }),
            ),
        ] {
            let mut args = json!({ "agent": "bob", "item": target, "action": action });
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(
                call_err(&client, "queue_update", args).await,
                "conflict",
                "{action}"
            );
        }
        // A note changes no state.
        let noted = call_ok(
            &client,
            "queue_update",
            json!({ "agent": "bob", "item": target, "action": "note", "note": "seen" }),
        )
        .await;
        assert_eq!(noted["state"], "proposed");
    }
    for action in ["claim", "complete", "release"] {
        assert_eq!(
            call_err(
                &client,
                "claim_request",
                json!({ "agent": "bob", "request_id": id, "action": action })
            )
            .await,
            "not_found"
        );
    }
    // No MCP tool admits, rejects, edits, sets a limit, names a delegate or changes the grace.
    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .filter(|n| n.starts_with("queue"))
        .collect();
    assert_eq!(tools.len(), 4, "{tools:?}");
    for name in ["queue_propose", "queue_next", "queue_update", "queue_list"] {
        assert!(
            tools.iter().any(|t| t == name),
            "{name} missing from {tools:?}"
        );
    }
    client.cancel().await.unwrap();

    assert_eq!(fx.item(q).state, QueueState::Proposed);
    assert!(fx.item(q).claim.is_none() && fx.item(q).admitted.is_none());
    assert_eq!(fx.events("queue.claimed"), 0);
    assert_eq!(fx.events("queue.admitted"), 0);
}

/// Every owner action, as command lines that end in `--owner`.
fn owner_actions(item: &str, claimed: &str) -> Vec<Vec<String>> {
    let s = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
    vec![
        s(&["queue", "propose", "--title", "by the owner", "--owner"]),
        s(&["queue", "admit", item, "--owner"]),
        s(&["queue", "reject", item, "--reason", "no", "--owner"]),
        s(&["queue", "edit", item, "--scope", "M2", "--owner"]),
        s(&["queue", "release", claimed, "--reason", "back", "--owner"]),
        s(&["queue", "limit", "set", "bob", "3", "--owner"]),
        s(&["queue", "limit", "default", "3", "--owner"]),
        s(&["queue", "delegate", "add", "bob", "--owner"]),
        s(&["queue", "delegate", "remove", "arch", "--owner"]),
        s(&["queue", "grace", "3h", "--owner"]),
    ]
}

// T17
#[test]
fn owner_actions_need_a_terminal() {
    let fx = Fx::new();
    let key = fx.delegate_key();
    let item = fx.propose("ann", "waits for the owner", QueueKind::Work);
    let held = fx.ready("held by bob");
    fx.store.queue_next("bob").unwrap().claimed.unwrap();
    let before = fx.snapshot();
    for action in owner_actions(&item.label, &held.label) {
        let args: Vec<&str> = action.iter().map(String::as_str).collect();
        assert_eq!(fx.fails(&args, &[]), (1, "forbidden".into()), "{args:?}");
        assert_eq!(
            fx.message(&args, &[]),
            "owner actions need an interactive terminal",
            "{args:?}"
        );
        // A key and a name do not stand in for the terminal.
        let env = [
            ("ANOTHER_AGENT", "arch"),
            ("ANOTHER_ADMIT_KEY", key.as_str()),
        ];
        assert_eq!(fx.fails(&args, &env), (1, "forbidden".into()), "{args:?}");
    }
    assert_eq!(fx.snapshot(), before, "nothing changed");

    #[cfg(unix)]
    {
        let q = item.label.as_str();
        // The prompt shows what is about to happen; anything but yes changes nothing.
        for typed in ["no\n", "\n", "yes please\n", ""] {
            let shown = in_terminal(&fx.home, &["queue", "admit", q, "--owner"], typed);
            assert!(shown.contains("Type yes to confirm:"), "{typed:?}: {shown}");
            assert_eq!(fx.item(q).state, QueueState::Proposed, "{typed:?}: {shown}");
        }
        let shown = in_terminal(&fx.home, &["queue", "admit", q, "--owner"], "yes\n");
        for needle in [q, "waits for the owner", "M1", "the tests pass", "ann"] {
            assert!(shown.contains(needle), "{needle:?} missing from {shown}");
        }
        let admitted = fx.item(q);
        assert_eq!(admitted.state, QueueState::Ready, "{shown}");
        let approval = admitted.admitted.unwrap();
        assert_eq!(
            (approval.authority.as_str(), approval.by.as_str()),
            ("owner", "owner")
        );
        let event = fx
            .store
            .events(100_000)
            .unwrap()
            .into_iter()
            .rfind(|e| e.kind == "queue.admitted")
            .unwrap();
        assert_eq!(event.payload["via"], "terminal");

        // `y` is a yes too. A security item is admitted at high.
        let leak = fx
            .store
            .queue_propose(
                &QueueCaller::agent("bob"),
                ProposeItem {
                    title: "a leak".into(),
                    kind: QueueKind::Security,
                    ..Default::default()
                },
            )
            .unwrap();
        let shown = in_terminal(
            &fx.home,
            &[
                "queue",
                "admit",
                &leak.label,
                "--scope",
                "M1",
                "--done-when",
                "closed",
                "--owner",
            ],
            "y\n",
        );
        let leak = fx.item(&leak.label);
        assert_eq!(leak.state, QueueState::Ready, "{shown}");
        assert_eq!(leak.priority, QueuePriority::High);
        // The other owner actions go through the same prompt.
        let shown = in_terminal(
            &fx.home,
            &["queue", "limit", "set", "bob", "3", "--owner"],
            "yes\n",
        );
        let limits = fx.store.queue_limits().unwrap();
        assert!(
            limits
                .agents
                .iter()
                .any(|l| l.agent == "bob" && l.limit == 3),
            "{shown}"
        );
        let shown = in_terminal(&fx.home, &["queue", "grace", "3h", "--owner"], "no\n");
        assert_eq!(
            fx.store.queue_grace().unwrap().grace_seconds,
            86_400,
            "{shown}"
        );
        let shown = in_terminal(&fx.home, &["queue", "grace", "3h", "--owner"], "yes\n");
        assert_eq!(
            fx.store.queue_grace().unwrap().grace_seconds,
            3 * 3600,
            "{shown}"
        );
        let shown = in_terminal(
            &fx.home,
            &["queue", "delegate", "add", "cy", "--owner"],
            "yes\n",
        );
        assert!(shown.contains("adk_"), "the key is printed once: {shown}");
        assert!(fx
            .store
            .queue_delegates()
            .unwrap()
            .iter()
            .any(|d| d.agent == "cy"));
    }
}

// T18
#[test]
fn only_a_keyed_delegate_admits() {
    let fx = Fx::new();
    let key = fx.delegate_key();
    assert!(key.starts_with("adk_") && key.len() == 68, "{key}");
    let a = fx.propose("ann", "first", QueueKind::Work);
    let b = fx.propose("ann", "second", QueueKind::Work);
    let c = fx.propose("ann", "third", QueueKind::Work);
    let arch = [("ANOTHER_AGENT", "arch")];
    let keyed = [
        ("ANOTHER_AGENT", "arch"),
        ("ANOTHER_ADMIT_KEY", key.as_str()),
    ];
    let wrong_key = format!("adk_{}", "f".repeat(64));
    let wrong = [
        ("ANOTHER_AGENT", "arch"),
        ("ANOTHER_ADMIT_KEY", wrong_key.as_str()),
    ];
    // The key belongs to arch: under another name it opens nothing.
    let stolen = [
        ("ANOTHER_AGENT", "bob"),
        ("ANOTHER_ADMIT_KEY", key.as_str()),
    ];
    let mut seen = String::new();
    let before = fx.snapshot();
    for env in [&arch[..], &wrong[..], &stolen[..]] {
        for args in [
            vec!["queue", "admit", a.label.as_str()],
            vec!["queue", "reject", a.label.as_str(), "--reason", "no"],
            vec!["queue", "edit", a.label.as_str(), "--scope", "M2"],
        ] {
            let out = fx.run(&[&["--json"][..], &args[..]].concat(), env);
            seen.push_str(&text(&out.stdout));
            seen.push_str(&text(&out.stderr));
            assert_eq!(
                fx.fails(&args, env),
                (1, "forbidden".into()),
                "{args:?} {env:?}"
            );
        }
    }
    assert_eq!(fx.snapshot(), before);

    // With its key the delegate admits, rejects, edits and releases another agent's claim.
    let (code, v) = fx.json(&["queue", "admit", &a.label, "--priority", "high"], &keyed);
    assert_eq!(code, 0, "{v}");
    seen.push_str(&v.to_string());
    assert_eq!(v["state"], "ready");
    assert_eq!(v["priority"], "high");
    assert_eq!(v["admitted"]["authority"], "delegate");
    assert_eq!(v["admitted"]["by"], "arch");
    assert_eq!(v["admitted"]["self_admitted"], false);
    let (code, v) = fx.json(
        &["queue", "reject", &b.label, "--reason", "not now"],
        &keyed,
    );
    assert_eq!(code, 0, "{v}");
    seen.push_str(&v.to_string());
    assert_eq!(v["state"], "done");
    assert_eq!(v["result"]["outcome"], "rejected");
    let (code, v) = fx.json(
        &["queue", "edit", &c.label, "--done-when", "CI is green"],
        &keyed,
    );
    assert_eq!(code, 0, "{v}");
    seen.push_str(&v.to_string());
    assert_eq!(v["done_condition"], "CI is green");
    let (code, v) = fx.json(&["queue", "next", "--agent", "bob"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["claimed"]["label"], a.label.as_str());
    let (code, v) = fx.json(
        &["queue", "release", &a.label, "--reason", "needed"],
        &keyed,
    );
    assert_eq!(code, 0, "{v}");
    seen.push_str(&v.to_string());
    assert_eq!(v["state"], "ready");
    assert_eq!(v["skip_agent"], "bob");
    let human = fx.run(&["queue", "show", &a.label], &keyed);
    assert!(text(&human.stdout).contains("admitted by arch (delegate) at "));
    seen.push_str(&text(&human.stdout));

    // A removed delegation: the old key is refused. A new one gets a new key.
    fx.store
        .queue_delegate_remove(&QueueCaller::Owner, "arch")
        .unwrap();
    assert_eq!(
        fx.fails(&["queue", "admit", &c.label], &keyed),
        (1, "forbidden".into())
    );
    let again = fx.delegate_key();
    assert_ne!(again, key);
    assert_eq!(
        fx.fails(&["queue", "admit", &c.label], &keyed),
        (1, "forbidden".into()),
        "the earlier key stays dead"
    );

    // The key appears nowhere: not in output, events, history or the lists.
    for args in [
        vec!["--json", "events", "--limit", "1000"],
        vec!["--json", "queue", "show", a.label.as_str()],
        vec!["--json", "queue", "list", "--state", "done"],
        vec!["--json", "queue", "delegate", "list"],
        vec!["--json", "activity", "--kinds", "queue", "--limit", "500"],
        vec!["queue", "list"],
    ] {
        let out = fx.run(&args, &keyed);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        seen.push_str(&text(&out.stdout));
        seen.push_str(&text(&out.stderr));
    }
    for secret in [&key, &again] {
        assert!(
            !seen.contains(secret.as_str()),
            "a delegate key was printed"
        );
        assert!(!seen.contains(&secret[4..]), "a delegate key was printed");
    }
    let db = std::fs::read(fx.home.join(another_core::DB_FILE)).unwrap();
    let db = String::from_utf8_lossy(&db);
    assert!(
        !db.contains(&key[4..]) && !db.contains(&again[4..]),
        "only the hash is stored"
    );
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .map(String::as_str)
        .collect()
}

const ITEM_KEYS: [&str; 18] = [
    "item_id",
    "seq",
    "label",
    "node_id",
    "state",
    "kind",
    "title",
    "body",
    "scope",
    "done_condition",
    "priority",
    "for_agent",
    "skip_agent",
    "proposed_by",
    "proposed_at",
    "admitted",
    "claim",
    "result",
];

// T19
#[tokio::test]
async fn cli_and_mcp_shapes() {
    let fx = Fx::new();
    let key = fx.delegate_key();
    let keyed = [
        ("ANOTHER_AGENT", "arch"),
        ("ANOTHER_ADMIT_KEY", key.as_str()),
    ];
    let ann = [("ANOTHER_AGENT", "ann")];

    // Success is 0, and every command answers in JSON.
    let (code, v) = fx.json(
        &[
            "queue",
            "propose",
            "--title",
            "Write the page",
            "--body",
            "docs/QUEUE.md",
            "--scope",
            "M1",
            "--done-when",
            "it is merged",
            "--kind",
            "work",
        ],
        &ann,
    );
    assert_eq!(code, 0, "{v}");
    let with_history: Vec<&str> = ITEM_KEYS.iter().copied().chain(["history"]).collect();
    assert_eq!(keys(&v), with_history);
    assert_eq!(v["label"], "Q1");
    assert_eq!(v["state"], "proposed");
    assert_eq!(v["proposed_by"], "ann");
    assert_eq!(v["priority"], "normal");
    assert_eq!(
        (&v["admitted"], &v["claim"], &v["result"]),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    assert_eq!(
        keys(&v["history"][0]),
        ["at", "kind", "actor", "note", "detail"]
    );
    // A body from standard input.
    let mut child = command(
        &fx.home,
        &[
            "--json",
            "queue",
            "propose",
            "--title",
            "From stdin",
            "--body",
            "-",
            "--agent",
            "ann",
        ],
        &[],
    )
    .stdin(Stdio::piped())
    .spawn()
    .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"line one\nline two\n")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(v["body"], "line one\nline two\n");

    // invalid, exit 1.
    let admit_bare = ["queue", "admit", "Q2"];
    assert_eq!(fx.fails(&admit_bare, &keyed), (1, "invalid".into()));
    assert_eq!(
        fx.message(&admit_bare, &keyed),
        "Q2 cannot be admitted: it has no scope. Pass --scope."
    );
    assert_eq!(
        fx.message(&["queue", "admit", "Q2", "--scope", "M1"], &keyed),
        "Q2 cannot be admitted: it has no done condition. Pass --done-when."
    );
    assert_eq!(
        fx.fails(
            &["queue", "propose", "--title", "bad \u{1b}[2J title"],
            &ann
        ),
        (1, "invalid".into())
    );
    assert_eq!(
        fx.fails(&["queue", "admit", "Q1", "--for", "arch"], &keyed),
        (1, "invalid".into()),
        "--for names the caller"
    );
    // not_found, exit 1.
    assert_eq!(
        fx.fails(&["queue", "show", "Q99"], &[]),
        (1, "not_found".into())
    );
    assert_eq!(
        fx.fails(&["queue", "next", "--agent", "nobody"], &[]),
        (1, "not_found".into())
    );
    assert_eq!(
        fx.fails(&["queue", "admit", "Q1", "--for", "nobody"], &keyed),
        (1, "not_found".into())
    );

    let (code, v) = fx.json(&["queue", "admit", "Q1", "--for", "ann"], &keyed);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        keys(&v["admitted"]),
        ["authority", "by", "at", "self_admitted", "edited_after"]
    );
    assert_eq!(v["for_agent"], "ann");
    fx.ready("another one");

    // forbidden, exit 1.
    assert_eq!(
        fx.fails(&["queue", "admit", "Q2"], &ann),
        (1, "forbidden".into())
    );
    assert_eq!(
        fx.fails(&["queue", "edit", "Q1", "--kind", "security"], &keyed),
        (1, "forbidden".into()),
        "a delegate raises nothing on a ready item"
    );
    for owner_only in [
        vec!["queue", "limit", "set", "bob", "3"],
        vec!["queue", "limit", "default", "3"],
        vec!["queue", "delegate", "add", "bob"],
        vec!["queue", "delegate", "remove", "arch"],
        vec!["queue", "grace", "3h"],
    ] {
        assert_eq!(
            fx.fails(&owner_only, &ann),
            (1, "forbidden".into()),
            "{owner_only:?}"
        );
    }

    // usage, exit 2, before anything is read or written.
    let before = fx.snapshot();
    for usage in [
        vec!["queue", "done", "Q1", "--summary", "s"],
        vec![
            "queue",
            "done",
            "Q1",
            "--summary",
            "s",
            "--pr",
            "1",
            "--commit",
            "abc1234",
        ],
        vec!["queue", "release", "Q1", "--to", "bob", "--reason", "x"],
        vec!["queue", "next", "--owner"],
        vec![
            "queue",
            "done",
            "Q1",
            "--summary",
            "s",
            "--pr",
            "1",
            "--owner",
        ],
        vec!["queue", "note", "Q1", "--body", "n", "--owner"],
        vec!["queue", "audit", "--owner"],
        vec!["queue", "list", "--state", "open"],
        vec!["queue", "propose", "--title", "t", "--kind", "urgent"],
        vec!["queue", "admit", "Q2", "--priority", "urgent"],
        vec!["queue", "edit", "Q2", "--for", "bob", "--for-anyone"],
    ] {
        let out = fx.run(&usage, &ann);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{usage:?}: {}",
            text(&out.stderr)
        );
    }
    assert_eq!(fx.snapshot(), before);

    // next: claimed is 0, at the limit is 3, nothing ready is 0.
    let (code, v) = fx.json(&["queue", "next"], &ann);
    assert_eq!(code, 0, "{v}");
    assert_eq!(keys(&v), ["claimed", "refused"]);
    assert_eq!(keys(&v["claimed"]), with_history);
    assert_eq!(v["claimed"]["label"], "Q1");
    assert_eq!(
        keys(&v["claimed"]["claim"]),
        ["agent", "at", "session_bound"]
    );
    assert_eq!(v["claimed"]["claim"]["session_bound"], false);
    assert_eq!(v["refused"], Value::Null);
    let (code, v) = fx.json(&["queue", "next"], &ann);
    assert_eq!(code, 3, "{v}");
    assert_eq!(v["claimed"], Value::Null);
    assert_eq!(keys(&v["refused"]), ["reason", "limit", "held", "message"]);
    assert_eq!(v["refused"]["reason"], "at_limit");
    assert_eq!(v["refused"]["limit"], 1);
    assert_eq!(v["refused"]["held"], json!(["Q1"]));
    // The text says why, and what to do.
    let out = fx.run(&["queue", "next"], &ann);
    assert_eq!(out.status.code(), Some(3));
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    for needle in [
        "ann holds 1 of its limit of 1",
        "Q1",
        "another queue done",
        "another queue release",
        "ANOTHER_AGENT",
    ] {
        assert!(said.contains(needle), "{needle:?} missing from {said}");
    }
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "cy", 0)
        .unwrap();
    let (code, v) = fx.json(&["queue", "next", "--agent", "cy"], &[]);
    assert_eq!(code, 3, "{v}");
    assert_eq!(v["refused"]["reason"], "no_seat");
    let out = fx.run(&["queue", "next", "--agent", "cy"], &[]);
    assert!(format!("{}{}", text(&out.stdout), text(&out.stderr))
        .contains("another queue limit set cy 1 --owner"));

    // conflict, exit 3: the item is not in the state the transition needs.
    assert_eq!(
        fx.fails(&["queue", "admit", "Q1"], &keyed),
        (3, "conflict".into())
    );
    let said = fx.message(&["queue", "reject", "Q1", "--reason", "no"], &keyed);
    assert!(said.contains("claimed by ann"), "{said}");
    assert_eq!(
        fx.fails(&["queue", "release", "Q2", "--agent", "bob"], &[]),
        (3, "conflict".into())
    );
    // forbidden: done or release by an agent that is not the claimer.
    assert_eq!(
        fx.fails(
            &[
                "queue",
                "done",
                "Q1",
                "--summary",
                "s",
                "--pr",
                "1",
                "--agent",
                "bob"
            ],
            &[]
        ),
        (1, "forbidden".into())
    );
    assert_eq!(
        fx.fails(&["queue", "release", "Q1", "--agent", "bob"], &[]),
        (1, "forbidden".into())
    );
    // invalid: a bad pointer.
    assert_eq!(
        fx.fails(
            &[
                "queue",
                "done",
                "Q1",
                "--summary",
                "s",
                "--commit",
                "nothex"
            ],
            &ann
        ),
        (1, "invalid".into())
    );

    // note, show, list, done.
    let (code, v) = fx.json(&["queue", "note", "Q1", "--body", "half way"], &ann);
    assert_eq!(code, 0, "{v}");
    let (code, v) = fx.json(&["queue", "show", "Q1"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(keys(&v), with_history);
    assert_eq!(v["history"].as_array().unwrap().len(), 4);
    let (code, v) = fx.json(&["queue", "list"], &[]);
    assert_eq!(code, 0, "{v}");
    let listed = v.as_array().expect("a list");
    assert_eq!(listed.len(), 2, "ready and claimed: {v}");
    assert_eq!(keys(&listed[0]), ITEM_KEYS, "no history in a list");
    let (_, v) = fx.json(&["queue", "list", "--state", "proposed"], &[]);
    assert_eq!(v.as_array().unwrap().len(), 1);
    let (_, v) = fx.json(&["queue", "list", "--mine"], &ann);
    assert_eq!(
        v.as_array().unwrap().len(),
        3,
        "what ann proposed or holds: {v}"
    );
    let (_, v) = fx.json(&["queue", "list", "--scope", "M9"], &[]);
    assert_eq!(v, json!([]));
    let (code, v) = fx.json(
        &["queue", "done", "Q1", "--summary", "Merged", "--pr", "#12"],
        &ann,
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        keys(&v["result"]),
        ["outcome", "summary", "pointer_kind", "pointer", "by", "at"]
    );
    assert_eq!(v["result"]["pointer"], "#12");
    let shown = text(&fx.run(&["queue", "show", "Q1"], &[]).stdout);
    for needle in [
        "Write the page",
        "it is merged",
        "Merged",
        "#12",
        "admitted by arch (delegate) at",
        "half way",
    ] {
        assert!(shown.contains(needle), "{needle:?} missing from {shown}");
    }
    let listed = text(&fx.run(&["queue", "list", "--state", "done"], &[]).stdout);
    assert!(
        listed.contains("Merged") && listed.contains("#12"),
        "{listed}"
    );

    // The read commands: limits, delegates, grace, the owner list, the audit.
    let (code, v) = fx.json(&["queue", "limit", "list"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(keys(&v), ["default_limit", "agents"]);
    let (code, v) = fx.json(&["queue", "delegate", "list"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v[0]["agent"], "arch");
    let (code, v) = fx.json(&["queue", "grace"], &[]);
    assert_eq!((code, &v), (0, &json!({ "grace_seconds": 86_400 })));
    let (code, v) = fx.json(&["queue", "list", "--needs-owner"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        keys(&v),
        [
            "decide_now",
            "awaiting",
            "quiet_claims",
            "nobody_can_take",
            "failed_notices",
            "self_admitted"
        ]
    );
    assert_eq!(keys(&v["self_admitted"]), ["recent", "total"]);
    assert_eq!(v["awaiting"][0]["label"], "Q2");
    let human = text(&fx.run(&["queue", "list", "--needs-owner"], &[]).stdout);
    assert!(
        human.contains("Awaiting a decision") && human.contains("Q2"),
        "{human}"
    );
    let (code, v) = fx.json(&["queue", "audit"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(
        keys(&v),
        [
            "ok",
            "findings",
            "items",
            "claims",
            "releases",
            "completions",
            "claimers"
        ]
    );
    let (code, _) = fx.json(&["queue", "audit", "--since", "1h"], &[]);
    assert_eq!(code, 0);
    // `queue audit` exits 1 on any finding.
    let db = rusqlite::Connection::open(fx.home.join(another_core::DB_FILE)).unwrap();
    db.execute(
        "UPDATE queue_items SET state = 'ready', outcome = NULL WHERE seq = 1",
        [],
    )
    .unwrap();
    let (code, v) = fx.json(&["queue", "audit"], &[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["findings"][0]["kind"], "loss");
    assert_eq!(keys(&v["findings"][0]), ["kind", "item", "at", "detail"]);
    db.execute(
        "UPDATE queue_items SET state = 'done', outcome = 'completed' WHERE seq = 1",
        [],
    )
    .unwrap();

    // The owner raising the kind of a ready item a delegate admitted.
    #[cfg(unix)]
    {
        let own = fx.propose("ann", "admitted by the delegate", QueueKind::Work);
        let (code, _) = fx.json(&["queue", "admit", &own.label], &keyed);
        assert_eq!(code, 0);
        let shown = in_terminal(
            &fx.home,
            &["queue", "edit", &own.label, "--kind", "security", "--owner"],
            "yes\n",
        );
        let expected = format!(
            "{} was admitted by a delegate. Reject it and propose it again as security.",
            own.label
        );
        assert!(shown.contains(&expected), "{shown}");
        assert_eq!(fx.item(&own.label).kind, QueueKind::Work);
    }

    // The MCP tools: the three results of queue_next are results, not tool errors.
    let client = in_process(Store::open(&fx.home).unwrap().unguarded_repair()).await;
    let made = call_ok(
        &client,
        "queue_propose",
        json!({ "agent": "bob", "title": "From MCP", "scope": "M1", "done_when": "done", "kind": "data_loss" }),
    )
    .await;
    assert_eq!(keys(&made), with_history);
    assert_eq!(made["kind"], "data_loss");
    assert_eq!(made["state"], "proposed");
    let empty = call_ok(&client, "queue_next", json!({ "agent": "cy" })).await;
    assert_eq!(
        empty["refused"]["reason"], "no_seat",
        "a refusal is a result"
    );
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "cy", 1)
        .unwrap();
    let mut taken = Vec::new();
    while let Some(label) = call_ok(&client, "queue_next", json!({ "agent": "bob" })).await
        ["claimed"]["label"]
        .as_str()
        .map(str::to_string)
    {
        taken.push(label.clone());
        call_ok(
            &client,
            "queue_update",
            json!({ "agent": "bob", "item": label, "action": "done", "summary": "ok", "pointer_kind": "file", "pointer": "notes.md" }),
        )
        .await;
    }
    assert!(!taken.is_empty());
    let nothing = call_ok(&client, "queue_next", json!({ "agent": "bob" })).await;
    assert_eq!(nothing, json!({ "claimed": null, "refused": null }));
    let last = fx.ready("one more");
    let claimed = call_ok(&client, "queue_next", json!({ "agent": "bob" })).await;
    assert_eq!(keys(&claimed), ["claimed", "refused"]);
    assert_eq!(claimed["claimed"]["label"], last.label.as_str());
    assert_eq!(keys(&claimed["claimed"]), with_history);
    fx.ready("and another");
    let refused = call_ok(&client, "queue_next", json!({ "agent": "bob" })).await;
    assert_eq!(refused["claimed"], Value::Null);
    assert_eq!(
        keys(&refused["refused"]),
        ["reason", "limit", "held", "message"]
    );
    assert_eq!(refused["refused"]["reason"], "at_limit");
    // queue_update: note, handoff, release; queue_list.
    let noted = call_ok(
        &client,
        "queue_update",
        json!({ "agent": "cy", "item": last.label, "action": "note", "note": "a note" }),
    )
    .await;
    assert_eq!(noted["state"], "claimed");
    let handed = call_ok(
        &client,
        "queue_update",
        json!({ "agent": "bob", "item": last.label, "action": "handoff", "to": "cy", "note": "yours" }),
    )
    .await;
    assert_eq!(handed["for_agent"], "cy");
    let taken = call_ok(&client, "queue_next", json!({ "agent": "cy" })).await;
    assert_eq!(taken["claimed"]["label"], last.label.as_str());
    // An MCP caller releases its own claim only: it has no key to present.
    assert_eq!(
        call_err(
            &client,
            "queue_update",
            json!({ "agent": "bob", "item": last.label, "action": "release" })
        )
        .await,
        "forbidden"
    );
    assert_eq!(
        call_err(
            &client,
            "queue_update",
            json!({ "agent": "arch", "item": last.label, "action": "release" })
        )
        .await,
        "forbidden"
    );
    let released = call_ok(
        &client,
        "queue_update",
        json!({ "agent": "cy", "item": last.label, "action": "release", "note": "not mine" }),
    )
    .await;
    assert_eq!(released["state"], "ready");
    assert_eq!(
        call_err(
            &client,
            "queue_update",
            json!({ "agent": "cy", "item": last.label, "action": "done" })
        )
        .await,
        "invalid",
        "done needs a summary and a pointer"
    );
    let list = call_ok(&client, "queue_list", json!({})).await;
    assert_eq!(keys(&list), ["items"]);
    assert_eq!(keys(&list["items"][0]), ITEM_KEYS);
    let one = call_ok(&client, "queue_list", json!({ "item": last.label })).await;
    assert_eq!(one["items"].as_array().unwrap().len(), 1);
    assert_eq!(keys(&one["items"][0]), with_history);
    let mine = call_ok(
        &client,
        "queue_list",
        json!({ "agent": "bob", "mine": true, "state": "proposed" }),
    )
    .await;
    assert_eq!(mine["items"][0]["title"], "From MCP");
    assert_eq!(
        call_err(&client, "queue_list", json!({ "item": "Q999" })).await,
        "not_found"
    );
    client.cancel().await.unwrap();
}

// T27
#[test]
fn a_delegate_key_is_refused_for_every_owner_only_action() {
    let fx = Fx::new();
    let key = fx.delegate_key();
    let keyed = [
        ("ANOTHER_AGENT", "arch"),
        ("ANOTHER_ADMIT_KEY", key.as_str()),
    ];
    // One proposed and one claimed item of each fast-lane kind.
    let mut fast = Vec::new();
    for kind in [QueueKind::Security, QueueKind::DataLoss] {
        let proposed = fx.propose("ann", "proposed", kind);
        let ready = fx.propose("ann", "ready", kind);
        fx.store
            .queue_admit(
                &QueueCaller::Owner,
                &ready.label,
                AdmitItem {
                    for_agent: Some("ann".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let claimed = fx.propose("ann", "claimed", kind);
        fx.store
            .queue_admit(
                &QueueCaller::Owner,
                &claimed.label,
                AdmitItem {
                    for_agent: Some("bob".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        fast.push((proposed.label, ready.label, claimed.label));
    }
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "bob", 2)
        .unwrap();
    for _ in 0..2 {
        fx.store.queue_next("bob").unwrap().claimed.unwrap();
    }
    // The key works: the delegate decides a work item.
    let work = fx.propose("ann", "ordinary", QueueKind::Work);
    let (code, v) = fx.json(&["queue", "admit", &work.label], &keyed);
    assert_eq!(code, 0, "{v}");

    let before = fx.snapshot();
    let mut refused: Vec<Vec<String>> = Vec::new();
    let s = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
    refused.push(s(&["queue", "limit", "set", "bob", "5"]));
    refused.push(s(&["queue", "limit", "set", "arch", "5"]));
    refused.push(s(&["queue", "limit", "default", "5"]));
    refused.push(s(&["queue", "delegate", "add", "bob"]));
    refused.push(s(&["queue", "delegate", "remove", "arch"]));
    refused.push(s(&["queue", "grace", "3h"]));
    for (proposed, ready, claimed) in &fast {
        refused.push(s(&["queue", "admit", proposed]));
        refused.push(s(&["queue", "admit", proposed, "--priority", "normal"]));
        refused.push(s(&["queue", "reject", proposed, "--reason", "no"]));
        refused.push(s(&["queue", "reject", ready, "--reason", "no"]));
        for target in [proposed, ready] {
            refused.push(s(&["queue", "edit", target, "--scope", "M2"]));
            refused.push(s(&["queue", "edit", target, "--done-when", "later"]));
            refused.push(s(&["queue", "edit", target, "--priority", "normal"]));
            refused.push(s(&["queue", "edit", target, "--for", "cy"]));
            refused.push(s(&["queue", "edit", target, "--for-anyone"]));
            // Lowering the kind is the owner's too.
            refused.push(s(&["queue", "edit", target, "--kind", "work"]));
        }
        refused.push(s(&["queue", "release", claimed, "--reason", "back"]));
    }
    for args in &refused {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(fx.fails(&args, &keyed), (1, "forbidden".into()), "{args:?}");
    }
    assert_eq!(fx.snapshot(), before, "a refused action changes nothing");
    let said = fx.message(&["queue", "admit", &fast[0].0], &keyed);
    assert_eq!(
        said,
        "security and data-loss items are decided by the owner personally"
    );
    // The same through the library, where no command line stands between.
    let arch = QueueCaller::Agent {
        agent: "arch".into(),
        admit_key: Some(key.clone()),
    };
    let forbidden = |r: another_core::Result<()>| assert_eq!(r.unwrap_err().code(), "forbidden");
    forbidden(fx.store.queue_set_limit(&arch, "bob", 5).map(|_| ()));
    forbidden(fx.store.queue_set_default_limit(&arch, 5).map(|_| ()));
    forbidden(fx.store.queue_delegate_add(&arch, "bob").map(|_| ()));
    forbidden(fx.store.queue_delegate_remove(&arch, "arch").map(|_| ()));
    forbidden(fx.store.queue_set_grace(&arch, 7200).map(|_| ()));
    for (proposed, ready, claimed) in &fast {
        forbidden(
            fx.store
                .queue_admit(&arch, proposed, AdmitItem::default())
                .map(|_| ()),
        );
        forbidden(fx.store.queue_reject(&arch, ready, "no").map(|_| ()));
        let lower = EditItem {
            kind: Some(QueueKind::Work),
            ..Default::default()
        };
        forbidden(fx.store.queue_edit(&arch, ready, lower).map(|_| ()));
        let back = ReleaseItem {
            reason: Some("back".into()),
            ..Default::default()
        };
        forbidden(fx.store.queue_release(&arch, claimed, back).map(|_| ()));
    }
    assert_eq!(fx.snapshot(), before);
    // The claimer still finishes its own fast-lane item.
    fx.store
        .queue_done(
            "bob",
            &fast[0].2,
            DoneItem {
                summary: "closed".into(),
                pointer_kind: PointerKind::Commit,
                pointer: "abc1234".into(),
            },
        )
        .unwrap();
}

/// The queue's migration under the upgrade rules (D87, D88). This test binary is a
/// development build: on a home a release build made, one schema behind, every queue
/// command refuses with the reason and the fix and writes nothing; told to migrate, it
/// takes a backup first, and the queue then works through normal, quick and read-only opens.
#[test]
fn queue_commands_follow_the_upgrade_rules() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh");
    assert!(command(&fresh, &["init"], &[])
        .output()
        .unwrap()
        .status
        .success());
    let version = |db: &Path| -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    let current = version(&fresh.join(another_core::DB_FILE));
    // The queue is schema v11; later schemas follow it.
    let before_queue = 10;
    // A home as the installed release build left it, before the queue existed.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let db = home.join(another_core::DB_FILE);
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        another_core::apply_schema(&c, before_queue).unwrap();
        let queue_tables: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'queue_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(queue_tables, 0, "the schema before this one has no queue");
    }
    let record = home.join(another_core::ORIGIN_FILE);
    std::fs::write(
        &record,
        format!(
            r#"{{"created":{{"version":"0.1.0","build":"release","schema":{},"at":"2026-11-01T00:00:00.000Z"}}}}"#,
            before_queue
        ),
    )
    .unwrap();
    let before = (std::fs::read(&db).unwrap(), std::fs::read(&record).unwrap());

    for args in [
        vec!["queue", "list"],
        vec!["queue", "list", "--needs-owner"],
        vec!["queue", "show", "Q1"],
        vec!["queue", "next", "--agent", "ann"],
        vec!["queue", "propose", "--title", "t", "--agent", "ann"],
        vec!["queue", "audit"],
        vec!["queue", "grace"],
        vec!["queue", "admit", "Q1", "--owner"],
    ] {
        let out = command(&home, &args, &[]).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let said = text(&out.stderr);
        // Every command, owner actions included, says why the home is refused and how to
        // proceed: the home is opened before anything else.
        for needle in [
            "this development build of another",
            "will not migrate the database",
            &format!("v{} to v{current}", before_queue),
            "--dev-migrate",
        ] {
            assert!(
                said.contains(needle),
                "{args:?}: {needle:?} missing from {said}"
            );
        }
    }
    assert_eq!(
        (std::fs::read(&db).unwrap(), std::fs::read(&record).unwrap()),
        before,
        "a refused queue command wrote nothing"
    );
    assert!(!home.join(another_core::BACKUP_DIR).exists());

    // Told to migrate: a backup of the old schema first, then the queue works.
    let out = command(&home, &["--dev-migrate", "--json", "queue", "list"], &[])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), "[]");
    assert_eq!(version(&db), current);
    let backups: Vec<PathBuf> = std::fs::read_dir(home.join(another_core::BACKUP_DIR))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        version(&backups[0]),
        before_queue,
        "the backup is the home before the queue"
    );
    assert!(text(&std::fs::read(&record).unwrap()).contains("\"migrated\""));

    let run = |args: &[&str]| {
        let out = command(&home, args, &[]).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    run(&["agent", "register", "ann", "--program", "claude"]);
    let made: Value = serde_json::from_str(&run(&[
        "--json",
        "queue",
        "propose",
        "--title",
        "after the upgrade",
        "--agent",
        "ann",
    ]))
    .unwrap();
    assert_eq!(made["label"], "Q1");
    assert!(run(&["queue", "list", "--state", "proposed"]).contains("after the upgrade"));
    assert!(run(&["queue", "audit"]).contains("no findings"));
    // The reads that open the database without migrating or writing still work with the
    // queue tables in the schema, and the queue reads work through such opens.
    run(&["board"]);
    for store in [
        Store::open_quick(&home, std::time::Duration::from_secs(5)).unwrap(),
        Store::open_read_only(&home, std::time::Duration::from_secs(5)).unwrap(),
    ] {
        assert_eq!(store.queue_item("Q1").unwrap().title, "after the upgrade");
        assert_eq!(
            store
                .queue_list(QueueFilter {
                    state: Some(QueueState::Proposed),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.queue_needs_owner().unwrap().awaiting.len(), 1);
        assert!(store.queue_audit(None).unwrap().ok);
    }
    assert_eq!(
        std::fs::read_dir(home.join(another_core::BACKUP_DIR))
            .unwrap()
            .count(),
        1
    );
}

// Results and the lapse rule (scope id M2) through the real binary: the hooks, the CLI and
// MCP heartbeats, `another sweep` and the serve sweeper.

impl Fx {
    /// Run a hook of `client` as `agent`, with the session id the client would send.
    fn hook(&self, event: &str, client: &str, session: Option<&str>, agent: &str) {
        use std::io::Write;
        let work = self.home.parent().unwrap().join("work");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        let mut input = json!({ "cwd": work, "tool_name": "Bash", "tool_input": {} });
        if let Some(session) = session {
            input["session_id"] = json!(session);
        }
        let mut child = command(
            &self.home,
            &["hook", "run", event, "--client", client],
            &[("ANOTHER_AGENT", agent)],
        )
        // A hook needs a home directory for the client configs; keep it inside the test.
        .env("HOME", self.home.parent().unwrap())
        .env_remove("GROK_HOOK_EVENT")
        // Tests may run inside a multiplexer; keep its pane out of captured targets.
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_BIN_PATH")
        .env_remove("TMUX_PANE")
        .env_remove("CMUX_SURFACE_ID")
        .env_remove("TERM_PROGRAM")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "a hook always exits 0");
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home.join(another_core::DB_FILE)).unwrap()
    }

    /// The `queue_sessions` rows, as `agent name/session id`.
    fn session_rows(&self) -> Vec<String> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT a.name || '/' || s.session_id FROM queue_sessions s
                 JOIN agents a ON a.agent_id = s.agent_id ORDER BY 1",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Make it `hours` later for `who`: its claims and its last sign of life move back.
    fn hours_pass(&self, who: &str, hours: i64) {
        let ms = hours * 3_600_000;
        let id = self.store.agent(who).unwrap().agent_id;
        let db = self.db();
        db.execute(
            "UPDATE queue_items SET claimed_at = claimed_at - ?2 WHERE claimer_id = ?1",
            rusqlite::params![id, ms],
        )
        .unwrap();
        db.execute(
            "UPDATE agents SET last_seen_at = last_seen_at - ?2 WHERE agent_id = ?1",
            rusqlite::params![id, ms],
        )
        .unwrap();
    }
}

// T20, T21 and T36 through the hooks, the CLI and the MCP server.
#[tokio::test]
async fn only_the_claiming_sessions_end_hook_releases_through_the_binary() {
    let fx = Fx::new();
    let item = fx.ready("claimed by a hooked session");
    fx.hook("session-start", "claude", Some("s1"), "ann");
    assert_eq!(
        fx.session_rows(),
        ["ann/s1"],
        "hook log: {:?}",
        std::fs::read_to_string(fx.home.join("logs").join("hooks.log")).unwrap_or_default()
    );
    let (code, next) = fx.json(&["queue", "next", "--agent", "ann"], &[]);
    assert_eq!(code, 0);
    assert_eq!(next["claimed"]["label"], item.label.as_str());
    assert_eq!(next["claimed"]["claim"]["session_bound"], true);
    let claimed = |why: &str| {
        assert_eq!(fx.item(&item.label).state, QueueState::Claimed, "{why}");
        assert_eq!(fx.events("queue.released"), 0, "{why}");
    };

    // No end but the claiming session's own releases the claim.
    let out = fx.run(&["heartbeat", "--agent", "ann", "--end"], &[]);
    assert!(out.status.success());
    claimed("another heartbeat --end");
    let client = in_process(fx.store.clone()).await;
    call_ok(&client, "heartbeat", json!({ "agent": "ann", "end": true })).await;
    claimed("the MCP heartbeat with end");
    fx.hook("session-end", "claude", None, "ann");
    claimed("an end hook with no session id");
    fx.hook("session-end", "claude", Some("s2"), "ann");
    claimed("the end hook of another session");
    fx.hook("session-end", "codex", Some("s1"), "ann");
    claimed("the end hook of a client whose session ids are not relied on");

    // A beat after a tool call is a sign of its own session: a second session's first
    // beat is not held back by the first one's stamp, inside the same minute.
    fx.hook("post-tool", "claude", Some("s1"), "ann");
    fx.hook("post-tool", "claude", Some("s3"), "ann");
    assert_eq!(fx.session_rows(), ["ann/s1", "ann/s3"]);
    // A session id made of path parts is a row and a stamp inside the state directory.
    fx.hook("post-tool", "claude", Some("../../escape/s4"), "ann");
    assert_eq!(
        fx.session_rows(),
        ["ann/../../escape/s4", "ann/s1", "ann/s3"]
    );
    let state = fx.home.join("hooks").join("state");
    let stamps = std::fs::read_dir(&state)
        .into_iter()
        .flatten()
        .filter(|e| {
            let path = e.as_ref().unwrap().path();
            path.is_file() && path.extension().is_some_and(|x| x == "beat")
        })
        .count();
    assert_eq!(stamps, 0, "presence does not need file renewal stamps");
    assert!(!fx.home.join("escape").exists());
    assert!(!fx.home.parent().unwrap().join("escape").exists());

    // Codex sends a session id too, but it is not on the list: no row, an unbound claim,
    // and its end hook releases nothing.
    let second = fx.ready("claimed under Codex hooks");
    for event in ["session-start", "prompt", "post-tool"] {
        fx.hook(event, "codex", Some("c1"), "bob");
    }
    assert!(fx.session_rows().iter().all(|r| !r.starts_with("bob/")));
    let (_, next) = fx.json(&["queue", "next", "--agent", "bob"], &[]);
    assert_eq!(next["claimed"]["label"], second.label.as_str());
    assert_eq!(next["claimed"]["claim"]["session_bound"], false);
    fx.hook("session-end", "codex", Some("c1"), "bob");
    assert_eq!(fx.item(&second.label).state, QueueState::Claimed);
    claimed("a Codex session of another agent ended");

    // The end hook of the session that made the claim returns the item and says so.
    fx.hook("session-end", "claude", Some("s1"), "ann");
    let after = fx.item(&item.label);
    assert_eq!(after.state, QueueState::Ready);
    assert_eq!(fx.events("queue.released"), 1);
    let history = after.history.unwrap();
    let last = history.last().unwrap();
    assert_eq!(last.kind, "queue.released");
    assert_eq!(last.actor, "system");
    assert_eq!(last.detail.as_ref().unwrap()["reason"], "session_end");
    let (_, inbox) = fx.json(&["inbox", "--agent", "ann", "--unread"], &[]);
    let subjects: Vec<&str> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, [format!("Claim released: {}", item.label)]);
    assert!(fx.store.queue_audit(None).unwrap().ok);
}

// T24 through `another sweep` and the sweeper of `another serve`.
#[tokio::test]
async fn sweep_and_the_serve_sweeper_release_after_the_grace_period() {
    let fx = Fx::new();
    let item = fx.ready("left behind");
    let (_, next) = fx.json(&["queue", "next", "--agent", "ann"], &[]);
    assert_eq!(next["claimed"]["label"], item.label.as_str());

    // 23 hours of silence: `another sweep` releases nothing.
    fx.hours_pass("ann", 23);
    let (code, swept) = fx.json(&["sweep"], &[]);
    assert_eq!(code, 0);
    assert_eq!(swept["released_claims"], json!([]));
    assert_eq!(swept["expired_holds"], 0);
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    // Past 24 hours it does, and says which.
    fx.hours_pass("ann", 2);
    let (code, swept) = fx.json(&["sweep"], &[]);
    assert_eq!(code, 0);
    assert_eq!(swept["released_claims"], json!([item.label]));
    let (_, shown) = fx.json(&["queue", "show", &item.label], &[]);
    assert_eq!(shown["state"], "ready");
    let last = shown["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["kind"], "queue.released");
    assert_eq!(last["detail"]["reason"], "grace");
    let out = fx.run(&["sweep"], &[]);
    assert!(!text(&out.stdout).contains("queue claim"), "nothing more");

    // The sweeper every `another serve` runs does the same.
    let (_, next) = fx.json(&["queue", "next", "--agent", "ann"], &[]);
    assert_eq!(next["claimed"]["label"], item.label.as_str());
    let stop = tokio_util::sync::CancellationToken::new();
    another::serve::spawn_sweeper(
        fx.store.clone(),
        std::time::Duration::from_millis(20),
        stop.clone(),
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        fx.item(&item.label).state,
        QueueState::Claimed,
        "a fresh claim is left alone"
    );
    fx.hours_pass("ann", 25);
    let mut released = false;
    for _ in 0..500 {
        if fx.item(&item.label).state == QueueState::Ready {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    stop.cancel();
    assert!(released, "the sweeper released the overdue claim");
    assert_eq!(fx.events("queue.released"), 2);
    assert!(fx.store.queue_audit(None).unwrap().ok);
}
