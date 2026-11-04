//! Remote agents end to end: a hub served over HTTP with --auth on a random loopback port,
//! and a second ANOTHER_HOME acting as the remote agent through the CLI: join with an
//! invite (MCP config written through fake clients), say hello to the lane, report, answer a
//! decision, see only its project, get kicked or leave. Never touches ~/.another or :7466.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Hub {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: Store,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("hub");
    let store = Store::open(&home).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (name, program, project) in [
        ("manager", Program::GrokBot, None),
        ("claude", Program::Claude, Some("/work/shop")),
        ("vaulter", Program::Codex, Some("/work/vault")),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    for (lane, project, lead) in [
        ("api", "shop", "claude@shop"),
        ("keys", "vault", "vaulter@vault"),
    ] {
        store
            .create_lane(
                NewLane {
                    agent: "manager".into(),
                    name: lane.into(),
                    project: project.into(),
                    lead: lead.into(),
                    charter: format!("{project} charter"),
                    status: Some(LaneStatus::Working),
                },
                None,
            )
            .unwrap();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
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
    Hub {
        _dir: dir,
        home,
        store,
        url: format!("http://{addr}"),
        ct,
        _rt: rt,
    }
}

fn another(home: &Path, user_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", user_home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LANE")
        .env_remove("ANOTHER_CLAUDE_BIN")
        .env_remove("ANOTHER_DECIDER")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(home: &Path, user_home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = another(home, user_home, &all);
    assert!(
        out.status.success(),
        "another {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn invite(h: &Hub, extra: &[&str]) -> Value {
    let mut args = vec![
        "invite",
        "create",
        "--agent",
        "cloud-grok",
        "--program",
        "grok_bot",
        "--project",
        "/work/shop",
        "--lane",
        "api@shop",
    ];
    args.extend_from_slice(extra);
    ok(&h.home, &h.home, &args)
}

#[test]
fn second_home_joins_reports_answers_and_is_kicked() {
    let h = hub();
    let dir = tempfile::tempdir().unwrap();
    let (b_home, b_user) = (dir.path().join("b-another"), dir.path().join("b-home"));
    std::fs::create_dir_all(b_user.join(".codex")).unwrap();
    // A fake `claude` that records the `mcp add` it was asked to run.
    let log = dir.path().join("claude.log");
    let fake = dir.path().join("claude");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" > '{}'\n", log.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let inv = invite(
        &h,
        &["--expires", "2h", "--hub-url", &h.url, "--print-snippet"],
    );
    let code = inv["code"].as_str().unwrap().to_string();
    let snippet = inv["snippet"].as_str().unwrap();
    assert!(snippet.contains(&code) && snippet.contains(&h.url));
    assert!(!snippet.contains("ano_"), "no bearer token in the snippet");
    let sh = Command::new("sh")
        .arg("-n")
        .arg("-c")
        .arg(snippet)
        .status()
        .unwrap();
    assert!(sh.success(), "snippet is valid sh");

    // Join from the second home.
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args([
            "--json",
            "join",
            &h.url,
            &code,
            "--write-mcp",
            "both",
            "--hello",
        ])
        .env("ANOTHER_HOME", &b_home)
        .env("HOME", &b_user)
        .env("ANOTHER_CLAUDE_BIN", &fake)
        .env("HOSTNAME", "cloud-box-7")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let joined: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(joined["agent"], "cloud-grok@shop");
    assert_eq!(joined["hello"], true);
    let claude_args = std::fs::read_to_string(&log).unwrap();
    assert!(
        claude_args.starts_with("mcp add --scope user --transport http another"),
        "{claude_args}"
    );
    assert!(
        claude_args.contains(&format!("{}/mcp", h.url))
            && claude_args.contains("Authorization: Bearer ano_")
    );
    let codex = std::fs::read_to_string(b_user.join(".codex/config.toml")).unwrap();
    assert!(
        codex.contains("[mcp_servers.another]")
            && codex.contains("bearer_token_env_var = \"ANOTHER_TOKEN\"")
    );
    assert!(
        !codex.contains("ano_"),
        "codex config references the env var, not the token"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["remote.json", "remote.env"] {
            let mode = std::fs::metadata(b_home.join(f))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{f}");
        }
    }
    let env = std::fs::read_to_string(b_home.join("remote.env")).unwrap();
    assert!(
        env.contains("export ANOTHER_AGENT='cloud-grok@shop'")
            && env.contains("export ANOTHER_LANE='api'")
    );

    // The hub saw the hello: a session start and a report in the lane, plus presence.
    let lane = h.store.lane("api@shop", None).unwrap();
    assert!(lane.reports[0]
        .summary
        .starts_with("joined from cloud-box-7"));
    let remotes = h.store.remote_agents().unwrap();
    assert_eq!(remotes[0].presence, Presence::Online);
    assert_eq!(remotes[0].client_hostname.as_deref(), Some("cloud-box-7"));

    // Reusing the code fails, from anywhere.
    let again = another(&dir.path().join("c"), &b_user, &["join", &h.url, &code]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("invalid or expired invite"));

    // Report into the lane; only its project is visible.
    ok(
        &b_home,
        &b_user,
        &[
            "remote",
            "report",
            "--summary",
            "Wired the client",
            "--status",
            "blocked",
        ],
    );
    assert_eq!(
        h.store.lane("api@shop", None).unwrap().lane.status,
        LaneStatus::Blocked
    );
    let lanes = ok(&b_home, &b_user, &["remote", "lanes"]);
    assert_eq!(lanes["lanes"].as_array().unwrap().len(), 1);
    assert!(!lanes.to_string().contains("keys"));

    // A decision in each project; the remote agent sees and answers only shop's.
    let opts = vec![
        DecisionOption {
            value: "ship".into(),
            label: "Ship".into(),
            description: None,
        },
        DecisionOption {
            value: "wait".into(),
            label: "Wait".into(),
            description: None,
        },
    ];
    let shop_d = h
        .store
        .ask_decision(
            NewDecision {
                agent: "claude@shop".into(),
                question: "Ship the API?".into(),
                options: opts.clone(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let vault_d = h
        .store
        .ask_decision(
            NewDecision {
                agent: "vaulter@vault".into(),
                question: "Rotate vault keys?".into(),
                options: opts,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let ds = ok(&b_home, &b_user, &["remote", "decisions"]);
    assert!(ds.to_string().contains("Ship the API?") && !ds.to_string().contains("vault"));
    let ans = ok(
        &b_home,
        &b_user,
        &[
            "remote",
            "answer",
            &shop_d.decision_id,
            "1",
            "--as",
            "owner",
        ],
    );
    assert_eq!(ans["answer"]["relayed_by"], "cloud-grok@shop");
    let denied = another(
        &b_home,
        &b_user,
        &["remote", "answer", &vault_d.decision_id, "1"],
    );
    assert!(!denied.status.success(), "out-of-scope decision");
    let inbox = h.store.inbox("claude@shop", InboxQuery::default()).unwrap();
    assert!(inbox
        .messages
        .iter()
        .any(|m| m.message.subject.starts_with("[decision] answered: ")));

    // Other surfaces with the same token stay inside shop.
    let token = serde_json::from_str::<Value>(
        &std::fs::read_to_string(b_home.join("remote.json")).unwrap(),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let get = |path: &str| -> (u16, String) {
        let r = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(None)
                .build(),
        )
        .get(&format!("{}{path}", h.url))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .unwrap();
        let status = r.status().as_u16();
        (status, r.into_body().read_to_string().unwrap())
    };
    for path in [
        "/activity?since=1h&limit=500",
        "/events/poll?since=1h&timeout=1",
        "/spend",
        "/summary?since=24h",
        "/decisions?all=true",
    ] {
        let (status, body) = get(path);
        assert_eq!(status, 200, "{path}: {body}");
        assert!(
            !body.contains("vault") && !body.contains("Rotate"),
            "{path} leaked: {body}"
        );
    }

    // Kick: the token stops working on the very next request.
    let k = ok(
        &h.home,
        &h.home,
        &["agent", "kick", "cloud-grok@shop", "--reason", "demo over"],
    );
    assert_eq!(k["tokens_revoked"], 1);
    let after = another(&b_home, &b_user, &["remote", "status"]);
    assert!(!after.status.success());
    assert!(String::from_utf8_lossy(&after.stderr).contains("401"));
    assert_eq!(get("/lanes").0, 401);
    let ops: Vec<(String, String)> = h
        .store
        .audit_log(200)
        .unwrap()
        .into_iter()
        .map(|a| (a.operation, a.outcome))
        .collect();
    for (op, outcome) in [
        ("POST /join", "ok"),
        ("POST /join", "denied"),
        ("POST /lanes/{lane}/report", "ok"),
        ("POST /agent/heartbeat", "ok"),
        ("POST /decisions/{id}/answer", "error"),
    ] {
        assert!(
            ops.contains(&(op.to_string(), outcome.to_string())),
            "missing {op} {outcome}: {ops:?}"
        );
    }
}

#[test]
fn leaving_and_bad_codes() {
    let h = hub();
    let dir = tempfile::tempdir().unwrap();
    let (b_home, b_user) = (dir.path().join("b"), dir.path().join("u"));
    std::fs::create_dir_all(&b_user).unwrap();
    let code = invite(&h, &[])["code"].as_str().unwrap().to_string();

    // Wrong or malformed codes: 403, audited, same message.
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .build(),
    );
    for bad in [
        format!("{}x", &code[..code.len() - 1]),
        "anj_nope".to_string(),
    ] {
        let mut r = agent
            .post(&format!("{}/join", h.url))
            .send_json(json!({ "code": bad }))
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert!(r
            .body_mut()
            .read_to_string()
            .unwrap()
            .contains("invalid or expired invite"));
    }

    ok(&b_home, &b_user, &["join", &h.url, &code]);
    assert!(b_home.join("remote.json").exists());
    // Guessing is rate-limited per client address: 429 well before many tries.
    let mut statuses = Vec::new();
    for _ in 0..12 {
        let r = agent
            .post(&format!("{}/join", h.url))
            .send_json(json!({ "code": "anj_guess" }))
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    assert!(statuses.contains(&429), "{statuses:?}");
    assert_eq!(*statuses.last().unwrap(), 429);
    assert!(h
        .store
        .audit_log(200)
        .unwrap()
        .iter()
        .any(|a| a.operation == "POST /join" && a.detail.as_deref() == Some("rate limited")));
    let left = ok(&b_home, &b_user, &["remote", "leave"]);
    assert_eq!(left["reason"], "left");
    assert!(!b_home.join("remote.json").exists(), "profile forgotten");
    let r = h.store.remote_agents().unwrap();
    assert_eq!(r[0].presence, Presence::Gone);
    let events = h.store.events(500).unwrap();
    let left_ev = events
        .iter()
        .rfind(|e| e.kind == "lane.agent_left")
        .unwrap();
    assert_eq!(left_ev.payload["reason"], "left");
    // Invites list shows it used.
    let list = ok(&h.home, &h.home, &["invite", "list"]);
    assert_eq!(list[0]["status"], "used");
}
