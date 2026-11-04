//! Decisions end to end: an agent asks over authenticated MCP, the manager lists and answers
//! over HTTP (scoped, audited, relayed_by recorded), and the local CLI flow including the
//! wake after an answer (through a fake `herdr`). Temporary ANOTHER_HOME, ephemeral ports.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::{Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

async fn connect(base: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn body(r: &CallToolResult) -> Value {
    let t = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str(&t).unwrap()
}

fn code(r: &CallToolResult) -> Option<String> {
    (r.is_error == Some(true))
        .then(|| body(r)["error"]["code"].as_str().map(str::to_string))
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_asks_manager_answers_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (name, program, project) in [
        ("manager", Program::GrokBot, None),
        ("claude", Program::Claude, Some("/work/shop")),
        ("codex", Program::Codex, Some("/work/vault")),
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
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    let base = format!("http://{addr}");
    let tok = |agent: &str, glob: &str| {
        store
            .create_token(agent, &[glob.to_string()], None, None)
            .unwrap()
            .secret
    };
    let claude = tok("claude@shop", "/work/shop");
    let codex = tok("codex@vault", "/work/vault");
    let manager = tok("manager", "/work/shop");

    // Agents ask over MCP (the agent is filled in from the token).
    let c = connect(&base, &claude).await;
    let ask = json!({
        "question": "Ship the refactor behind a flag, or wait for review?",
        "options": [{"value": "ship", "label": "Ship behind a flag"}, {"value": "wait", "label": "Wait for review"}],
        "context_links": ["https://example.test/pr/7"],
        "deadline": "2h",
        "on_timeout": "choose_default",
        "default": "wait"
    });
    let r = call(&c, "decision_ask", ask.clone()).await;
    assert_eq!(code(&r), None, "{}", body(&r));
    let shop_id = body(&r)["decision_id"].as_str().unwrap().to_string();
    // The asker can't answer its own decision, even claiming to relay for jacob.
    let own = call(
        &c,
        "decision_answer",
        json!({ "decision": shop_id, "value": "ship", "answered_by": "jacob" }),
    )
    .await;
    assert_eq!(code(&own).as_deref(), Some("forbidden"));
    let own_http = reqwest::Client::new()
        .post(format!("{base}/decisions/{shop_id}/answer"))
        .bearer_auth(&claude)
        .json(&json!({ "value": "ship", "answered_by": "jacob" }))
        .send()
        .await
        .unwrap();
    assert_eq!(own_http.status(), 403);
    // Acting as someone else is refused.
    let mut spoof = ask.clone();
    spoof["agent"] = json!("codex@vault");
    assert_eq!(
        code(&call(&c, "decision_ask", spoof).await).as_deref(),
        Some("forbidden")
    );
    let v = connect(&base, &codex).await;
    let r = call(
        &v,
        "decision_ask",
        json!({ "question": "Rotate vault keys?", "options": [{"value": "yes", "label": "Yes"}, {"value": "no", "label": "No"}] }),
    )
    .await;
    let vault_id = body(&r)["decision_id"].as_str().unwrap().to_string();

    // The manager sees only shop decisions, with markdown.
    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        let manager = manager.clone();
        async move {
            let r = http
                .get(format!("{base}{path}", base = addr_base(&addr)))
                .bearer_auth(manager)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (status, list) = get("/decisions".into()).await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = list["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["decision_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![shop_id.as_str()]);
    let (status, shown) = get(format!("/decisions/{shop_id}")).await;
    assert_eq!(status, 200);
    assert!(shown["markdown"]
        .as_str()
        .unwrap()
        .contains("1. Ship behind a flag (`ship`)"));
    let (status, _) = get(format!("/decisions/{vault_id}")).await;
    assert_eq!(status, 404, "a hidden decision is a missing one");
    assert_eq!(
        http.get(format!("{base}/decisions"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // Answer over HTTP: relayed_by is the token's agent; out-of-scope is 403; twice is 409.
    let post = |id: String, b: Value| {
        let http = http.clone();
        let manager = manager.clone();
        let base = base.clone();
        async move {
            let r = http
                .post(format!("{base}/decisions/{id}/answer"))
                .bearer_auth(manager)
                .json(&b)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (status, _) = post(vault_id.clone(), json!({ "value": "yes" })).await;
    assert_eq!(status, 404, "a hidden decision is a missing one");
    let (status, err) = post(shop_id.clone(), json!({ "value": "later" })).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (400, Some("invalid"))
    );
    let (status, d) = post(
        shop_id.clone(),
        json!({ "value": "1", "answered_by": "jacob" }),
    )
    .await;
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["answer"]["value"], "ship");
    assert_eq!(d["answer"]["answered_by"], "jacob");
    assert_eq!(d["answer"]["relayed_by"], "manager");
    let (status, _) = post(shop_id.clone(), json!({ "value": "wait" })).await;
    assert_eq!(status, 409);

    // MCP decision_answer: relayed_by can't be spoofed.
    let m = connect(&base, &manager).await;
    let r = call(
        &m,
        "decision_answer",
        json!({ "decision": shop_id, "value": "wait", "relayed_by": "claude@shop" }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));

    // The asking agent got the answer in its inbox.
    let r = call(&c, "fetch_inbox", json!({})).await;
    let inbox = body(&r).to_string();
    assert!(inbox.contains("[decision] answered: ship"), "{inbox}");

    // Audited.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let ops: Vec<(String, String)> = store
        .audit_log(200)
        .unwrap()
        .into_iter()
        .map(|a| (a.operation, a.outcome))
        .collect();
    for (op, outcome) in [
        ("GET /decisions", "ok"),
        ("GET /decisions/{id}", "error"),
        ("POST /decisions/{id}/answer", "ok"),
        ("POST /decisions/{id}/answer", "error"),
        ("tools/call decision_ask", "ok"),
        ("tools/call decision_ask", "denied"),
        ("tools/call decision_answer", "denied"),
    ] {
        assert!(
            ops.contains(&(op.to_string(), outcome.to_string())),
            "missing {op} {outcome}: {ops:?}"
        );
    }
    for client in [c, v, m] {
        client.cancel().await.unwrap();
    }
    ct.cancel();
}

fn addr_base(addr: &SocketAddr) -> String {
    format!("http://{addr}")
}

fn bin(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_DECIDER")
        .stdin(Stdio::null());
    c
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = bin(home, args).output().unwrap();
    assert!(
        out.status.success(),
        "another {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[cfg(unix)]
#[test]
fn decision_cli_and_wake() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let repo = home.join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_str().unwrap();
    ok(
        home,
        &[
            "agent",
            "register",
            "claude",
            "--program",
            "claude",
            "--project",
            repo_s,
        ],
    );

    let out = ok(
        home,
        &[
            "--json",
            "decision",
            "ask",
            "--agent",
            "claude@shop",
            "-q",
            "Which database?",
            "--option",
            "pg=Postgres",
            "--option",
            "sqlite=SQLite",
            "--free-text",
            "--to",
            "sam",
            "--deadline",
            "1d",
            "--on-timeout",
            "cancel",
        ],
    );
    let d: Value = serde_json::from_str(&out).unwrap();
    let id = d["decision_id"].as_str().unwrap().to_string();
    assert_eq!(d["human"], "sam");
    assert_eq!(d["project"], "shop");

    let list = ok(home, &["decision", "list", "--for", "sam"]);
    assert!(list.contains(&id) && list.contains("pg | sqlite"), "{list}");
    let md = ok(home, &["decision", "show", &id, "--markdown"]);
    assert!(md.contains("**Decision for sam**"));
    assert!(md.contains("2. SQLite (`sqlite`)"));
    let inbox = ok(home, &["inbox", "--agent", "sam"]);
    assert!(inbox.contains("[decision] claude@shop asks sam"), "{inbox}");

    // Wake target through a fake herdr that records its arguments.
    let log = home.join("herdr.log");
    let fake = home.join("herdr");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    ok(
        home,
        &[
            "wake",
            "set",
            "--agent",
            "claude@shop",
            "--via",
            "herdr",
            "--target",
            "claude",
        ],
    );

    let out = bin(
        home,
        &[
            "decision",
            "answer",
            &id,
            "pg",
            "--text",
            "Postgres, we need JSONB",
        ],
    )
    .env("HERDR_BIN_PATH", &fake)
    .output()
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("answered decision") && said.contains("(woken)"),
        "{said}"
    );
    let woke = std::fs::read_to_string(&log).unwrap();
    assert!(woke.starts_with("agent prompt claude"), "{woke}");
    assert!(
        !woke.contains("JSONB"),
        "answer text never goes into the wake message"
    );

    let inbox = ok(home, &["--json", "inbox", "--agent", "claude@shop"]);
    assert!(inbox.contains("[decision] answered: pg"), "{inbox}");
    assert!(inbox.contains("Answered by sam"));

    let out = bin(home, &["decision", "answer", &id, "sqlite"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "already answered");
    let all = ok(home, &["decision", "list", "--all"]);
    assert!(
        all.contains("answered") && all.contains("-> pg by sam"),
        "{all}"
    );
}
