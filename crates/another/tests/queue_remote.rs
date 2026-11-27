//! The queue and remote callers (T5 of the queue spec's test plan): over a real
//! authenticated server no queue tool is available, and no route returns the text of an
//! item: not its title, body, done condition, notes, reasons, result or a notice error.

use std::net::SocketAddr;

use another::dashboard::DashboardMode;
use another::serve::{http_router, HttpOptions};
use another_core::{
    AdmitItem, DoneItem, EditItem, PointerKind, Program, ProposeItem, QueueCaller, RegisterAgent,
    ReleaseItem, Store,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

/// Every free text in the run starts with this.
const MARK: &str = "ZQMARK_";

async fn serve(store: Store, opts: HttpOptions, ct: &CancellationToken) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut opts = opts;
    opts.bound_port = addr.port();
    opts.dashboard_bound = Some(addr);
    let app = http_router(store, ct.clone(), &opts);
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
    format!("http://{addr}")
}

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

fn error_code(res: &CallToolResult) -> Option<String> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    v["error"]["code"].as_str().map(str::to_string)
}

/// Write every `queue.*` event kind, with [`MARK`] in every free text. Returns the id and
/// number of the item the run is about.
fn marked_run(store: &Store) -> (String, i64) {
    let owner = QueueCaller::Owner;
    let made = store.queue_delegate_add(&owner, "arch").unwrap();
    let arch = QueueCaller::Agent {
        agent: "arch".into(),
        admit_key: Some(made.key),
    };
    store.queue_set_limit(&owner, "ann", 3).unwrap();
    store.queue_set_grace(&owner, 7200).unwrap();
    let propose = |title: &str| {
        store
            .queue_propose(
                &QueueCaller::agent("ann"),
                ProposeItem {
                    title: format!("{MARK}-title {title}"),
                    body: format!("{MARK}-body"),
                    scope: Some("M1".into()),
                    done_when: Some(format!("{MARK}-done")),
                    ..Default::default()
                },
            )
            .unwrap()
    };
    let item = propose("one");
    let rejected = propose("two");
    store
        .queue_reject(&arch, &rejected.label, &format!("{MARK}-reject-reason"))
        .unwrap();
    store
        .queue_admit(&arch, &item.label, AdmitItem::default())
        .unwrap();
    store
        .queue_edit(
            &arch,
            &item.label,
            EditItem {
                done_when: Some(format!("{MARK}-done-edited")),
                ..Default::default()
            },
        )
        .unwrap();
    store.queue_next("ann").unwrap().claimed.unwrap();
    store
        .queue_note("bob", &item.label, &format!("{MARK}-note"))
        .unwrap();
    store
        .queue_release(
            &QueueCaller::agent("ann"),
            &item.label,
            ReleaseItem {
                to: Some("bob".into()),
                note: Some(format!("{MARK}-handoff-note")),
                ..Default::default()
            },
        )
        .unwrap();
    store.queue_next("bob").unwrap().claimed.unwrap();
    // The notice for this release fails, with a marker in the error.
    let db = rusqlite::Connection::open(store.db_path().unwrap()).unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER break_mail BEFORE INSERT ON messages
         BEGIN SELECT RAISE(ABORT, '{MARK}-notice-error'); END;"
    ))
    .unwrap();
    store
        .queue_release(
            &arch,
            &item.label,
            ReleaseItem {
                reason: Some(format!("{MARK}-release-reason")),
                ..Default::default()
            },
        )
        .unwrap();
    db.execute_batch("DROP TRIGGER break_mail").unwrap();
    // A release whose notice is delivered: its mail carries fixed words only.
    store.queue_next("ann").unwrap().claimed.unwrap();
    store
        .queue_release(
            &arch,
            &item.label,
            ReleaseItem {
                reason: Some(format!("{MARK}-release-reason-two")),
                ..Default::default()
            },
        )
        .unwrap();
    store.queue_next("cy").unwrap().claimed.unwrap();
    store
        .queue_done(
            "cy",
            &item.label,
            DoneItem {
                summary: format!("{MARK}-summary"),
                pointer_kind: PointerKind::File,
                pointer: format!("{MARK}-pointer"),
            },
        )
        .unwrap();
    store.queue_delegate_remove(&owner, "arch").unwrap();
    // A kind a later build may add: it must render without its payload.
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         VALUES ('01ZZZZZZZZZZZZZZZZZZZZZZZZ', ?1, ?2, 'ann', 'queue.future', ?3)",
        rusqlite::params![
            store.node_id(),
            store.now(),
            json!({ "item_id": item.item_id, "seq": item.seq, "title": format!("{MARK}-future") })
                .to_string()
        ],
    )
    .unwrap();
    let written: std::collections::BTreeSet<String> = store
        .events(100_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("queue."))
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        written.len(),
        15,
        "every queue kind and the unknown one: {written:?}"
    );
    (item.item_id, item.seq)
}

/// Every activity item in `v` (any object with a `kind` and a `refs`) that is a queue event.
fn queue_items(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            let is_queue = m
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| k.starts_with("queue."));
            if is_queue && m.contains_key("refs") {
                out.push(v.clone());
            }
            for child in m.values() {
                queue_items(child, out);
            }
        }
        Value::Array(a) => {
            for child in a {
                queue_items(child, out);
            }
        }
        _ => {}
    }
}

// T5
#[tokio::test]
async fn remote_callers_cannot_act_on_or_read_queue_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work = dir.path().join("work/shop").to_string_lossy().into_owned();
    let work_glob = dir
        .path()
        .join("work/*")
        .to_string_lossy()
        .replace('\\', "/");
    store.ensure_project("t", &work).unwrap();
    for (name, program) in [
        ("grok-bot", Program::GrokBot),
        ("ann", Program::Claude),
        ("bob", Program::Codex),
        ("cy", Program::Claude),
        ("arch", Program::Claude),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let ct = CancellationToken::new();
    // The server a remote caller reaches: tokens required, the dashboard read-only.
    let remote = serve(
        store.clone(),
        HttpOptions {
            auth: true,
            dashboard: DashboardMode::Redacted,
            ..Default::default()
        },
        &ct,
    )
    .await;
    // The loopback server of the same home, whose dashboard shows message bodies.
    let local = serve(
        store.clone(),
        HttpOptions {
            dashboard: DashboardMode::Local,
            ..Default::default()
        },
        &ct,
    )
    .await;
    let token = store
        .create_token("grok-bot", std::slice::from_ref(&work_glob), None, None)
        .unwrap();
    let client = connect(&remote, &token.secret).await;

    // A ready item exists, so a remote `next` would have something to take.
    let waiting = store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: format!("{MARK}-title waiting"),
                scope: Some("M1".into()),
                done_when: Some(format!("{MARK}-done")),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .queue_admit(&QueueCaller::Owner, &waiting.label, AdmitItem::default())
        .unwrap();

    // Every queue tool is refused, whoever the caller says it is.
    for who in [
        json!({}),
        json!({ "agent": "grok-bot" }),
        json!({ "agent": "ann" }),
    ] {
        for (tool, args) in [
            ("queue_propose", json!({ "title": "from outside" })),
            ("queue_next", json!({})),
            (
                "queue_update",
                json!({ "item": waiting.label, "action": "note", "note": "n" }),
            ),
            (
                "queue_update",
                json!({ "item": waiting.label, "action": "release" }),
            ),
            (
                "queue_update",
                json!({ "item": waiting.label, "action": "handoff", "to": "bob" }),
            ),
            (
                "queue_update",
                json!({ "item": waiting.label, "action": "done", "summary": "s", "pointer_kind": "pr", "pointer": "1" }),
            ),
            ("queue_list", json!({})),
            ("queue_list", json!({ "item": waiting.label })),
        ] {
            let mut args = args;
            args.as_object_mut()
                .unwrap()
                .extend(who.as_object().unwrap().clone());
            let res = call(&client, tool, args.clone()).await;
            assert_eq!(
                error_code(&res).as_deref(),
                Some("forbidden"),
                "{tool} {args}: {res:?}"
            );
        }
    }
    // A queue item is not a request a remote caller can claim.
    let res = call(
        &client,
        "claim_request",
        json!({ "request_id": waiting.item_id, "action": "claim" }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("not_found"));
    let still = store.queue_item(&waiting.label).unwrap();
    assert_eq!(still.state, another_core::QueueState::Ready);
    assert_eq!(still.history.unwrap().len(), 2, "nothing was added to it");
    store
        .queue_reject(
            &QueueCaller::Owner,
            &waiting.label,
            &format!("{MARK}-reject-reason"),
        )
        .unwrap();

    let (item_id, seq) = marked_run(&store);

    // Everything a caller can read. First the MCP tools the remote caller is allowed.
    let mut seen: Vec<(String, String)> = Vec::new();
    for (tool, args) in [
        ("activity_feed", json!({ "since": "1d", "limit": 500 })),
        (
            "activity_feed",
            json!({ "since": "1d", "limit": 500, "kinds": ["queue"] }),
        ),
        ("activity_feed", json!({})),
        ("activity_summarize", json!({ "since": "1d" })),
        ("whats_new", json!({ "since": "1d", "limit": 20 })),
        ("work_board", json!({ "project": work })),
        ("fetch_inbox", json!({ "unread_only": false })),
        ("threads", json!({})),
        ("resource_find", json!({})),
        ("orphans_list", json!({})),
    ] {
        let res = call(&client, tool, args.clone()).await;
        assert_ne!(res.is_error, Some(true), "{tool} {args}: {res:?}");
        seen.push((
            format!("mcp {tool} {args}"),
            serde_json::to_string(&res).unwrap(),
        ));
    }
    // Then every mounted route (serve.rs: /mcp, /activity, /health; dashboard.rs: /ui and
    // /api/dash). The federation routes are signed and are covered by the core test
    // `queue_events_never_leave_the_node`; the OAuth routes carry no node data.
    let http = reqwest::Client::new();
    let routes = [
        "/health",
        "/activity",
        "/activity?since=1d&limit=500",
        "/activity?since=1d&limit=500&kinds=queue",
        "/ui",
        "/ui/",
        "/ui/app.css",
        "/ui/app.js",
        "/api/dash",
        "/api/dash/activity",
        "/api/dash/activity?kind=queue&limit=200",
        "/api/dash/activity?since=all&limit=200",
        "/api/dash/activity?q=completed",
        "/api/dash/revision",
    ];
    for (base, bearer) in [(&remote, Some(&token.secret)), (&local, None)] {
        for route in routes {
            let mut req = http.get(format!("{base}{route}"));
            if let Some(secret) = bearer {
                req = req.bearer_auth(secret);
            }
            let res = req.send().await.unwrap();
            let status = res.status();
            let body = res.text().await.unwrap();
            assert!(status.is_success(), "{base}{route}: {status} {body}");
            seen.push((format!("GET {base}{route}"), body));
        }
    }

    let mut queue_lines = 0;
    for (what, body) in &seen {
        assert!(!body.contains(MARK), "{what} returned item text: {body}");
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        // MCP results carry their JSON as text too; look inside both forms.
        let mut items = Vec::new();
        queue_items(&v, &mut items);
        if let Some(text) = v["content"][0]["text"].as_str() {
            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                queue_items(&inner, &mut items);
            }
        }
        for i in items {
            queue_lines += 1;
            let refs = &i["refs"];
            let whole = json!({ "item_id": item_id, "seq": seq });
            let rejected_early = refs.get("seq").is_some_and(|s| s != &json!(seq));
            assert!(
                refs == &json!({}) || refs == &whole || rejected_early,
                "{what}: refs {refs}"
            );
            if let Some(m) = refs.as_object() {
                assert!(
                    m.is_empty() || m.keys().collect::<Vec<_>>() == ["item_id", "seq"],
                    "{what}: refs {refs}"
                );
            }
            if i["kind"] == "queue.future" {
                assert_eq!(refs, &json!({}), "{what}");
                assert_eq!(i["summary"], "ann: queue.future", "{what}");
            }
            assert!(
                !what.contains("whats_new"),
                "whats_new shows no queue event: {i}"
            );
        }
    }
    assert!(
        queue_lines > 30,
        "the feeds did show the queue events, without their text ({queue_lines})"
    );
    // A remote caller can learn that items exist, their numbers and scope ids: no more.
    let feed = call(
        &client,
        "activity_feed",
        json!({ "since": "1d", "limit": 500, "kinds": ["queue"] }),
    )
    .await;
    let feed = feed.structured_content.unwrap();
    let lines: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["summary"].as_str().unwrap())
        .collect();
    let label = format!("Q{seq} (M1)");
    assert!(
        lines.contains(&format!("cy completed {label}").as_str()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&format!("arch released {label}: delegate").as_str()),
        "{lines:?}"
    );

    client.cancel().await.unwrap();
    ct.cancel();
}

/// The owner list (`another needs`) is a local command only: no MCP tool and no route
/// returns it, on the authenticated server or on the loopback one.
#[tokio::test]
async fn needs_is_not_reachable_remotely() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work_glob = dir
        .path()
        .join("work/*")
        .to_string_lossy()
        .replace('\\', "/");
    store
        .register_agent(RegisterAgent {
            name: "grok-bot".into(),
            program: Program::GrokBot,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    // Something the list would show, so an answer would not be empty.
    store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: format!("{MARK}-title waiting on the owner"),
                ..Default::default()
            },
        )
        .unwrap();
    let ct = CancellationToken::new();
    let remote = serve(
        store.clone(),
        HttpOptions {
            auth: true,
            dashboard: DashboardMode::Redacted,
            ..Default::default()
        },
        &ct,
    )
    .await;
    let local = serve(
        store.clone(),
        HttpOptions {
            dashboard: DashboardMode::Local,
            ..Default::default()
        },
        &ct,
    )
    .await;
    let token = store
        .create_token("grok-bot", std::slice::from_ref(&work_glob), None, None)
        .unwrap();

    for (base, secret) in [(&remote, token.secret.as_str()), (&local, "")] {
        let client = connect(base, secret).await;
        let tools: Vec<String> = client
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(tools.len() > 10, "{tools:?}");
        assert!(
            !tools.iter().any(|name| name.contains("needs")),
            "{tools:?}"
        );
        client.cancel().await.unwrap();
    }

    let http = reqwest::Client::new();
    for (base, bearer) in [(&remote, Some(&token.secret)), (&local, None)] {
        for route in ["/needs", "/api/needs", "/api/dash/needs"] {
            let mut req = http.get(format!("{base}{route}"));
            if let Some(secret) = bearer {
                req = req.bearer_auth(secret);
            }
            let res = req.send().await.unwrap();
            let status = res.status();
            let body = res.text().await.unwrap();
            assert_eq!(status.as_u16(), 404, "{base}{route}: {body}");
            assert!(!body.contains(MARK), "{base}{route}: {body}");
        }
    }
    ct.cancel();
}
