//! Regression tests from the v0.1.0 bug hunt (`another-bug-hunt-BUGS.md`), one per finding,
//! named after its bug number. An authenticated server in process, on an ephemeral loopback
//! port, with a temporary home.

use std::net::SocketAddr;

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

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
}

fn agent(store: &Store, name: &str, program: Program, project: Option<&str>) {
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

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let inside = dir.path().join("sandbox/w/a");
    let outside = dir.path().join("private/data");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let inside = inside.to_str().unwrap();
    let outside = outside.to_str().unwrap();
    store.ensure_project("t", inside).unwrap();
    store.ensure_project("t", outside).unwrap();
    agent(&store, "worker", Program::Claude, Some(inside));
    agent(&store, "insider", Program::Claude, Some(outside));
    agent(&store, "colleague", Program::Claude, Some(outside));
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            legacy_repair: true,
            auth: true,
            bound_port: addr.port(),
            ..Default::default()
        },
    );
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
    Server {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        ct,
    }
}

impl Server {
    fn path(&self, relative: &str) -> String {
        self._dir.path().join(relative).to_str().unwrap().to_owned()
    }

    /// A token for `worker@a` restricted to this fixture's `sandbox/w`.
    fn token(&self) -> another_core::NewToken {
        let scope = format!("{}/**", self.path("sandbox/w").replace('\\', "/"));
        self.store
            .create_token("worker@a", &[scope], None, None)
            .unwrap()
    }
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

fn text(result: &CallToolResult) -> String {
    serde_json::to_string(&result.content).unwrap()
}

/// BH-1. The token's globs are checked against the raw string and the store then normalizes it,
/// so an apparently in-scope `sandbox/w/../../private/data` escapes the scope.
#[tokio::test]
async fn bh1_dot_dot_does_not_escape_a_tokens_project_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let escape = srv.path("sandbox/w/../../private/data");
    let outside = srv.path("private/data");
    let new_outside = srv.path("sandbox/w/../../brand/new");

    let refused = call(
        &client,
        "hold_paths",
        json!({ "project": outside, "paths": ["x"] }),
    )
    .await;
    assert_eq!(refused.is_error, Some(true), "the plain path is refused");

    let hold = call(
        &client,
        "hold_paths",
        json!({ "project": escape, "paths": ["x"] }),
    )
    .await;
    assert_eq!(
        hold.is_error,
        Some(true),
        "hold outside the globs: {hold:?}"
    );
    assert!(srv.store.holds(Default::default()).unwrap().is_empty());

    let resource = call(
        &client,
        "resource_register",
        json!({ "project": escape, "kind": "model_server", "name": "r", "ttl_seconds": 60 }),
    )
    .await;
    assert_eq!(
        resource.is_error,
        Some(true),
        "resource outside the globs: {resource:?}"
    );

    for path in [&escape, &new_outside] {
        let ensured = call(&client, "ensure_project", json!({ "path": path })).await;
        assert_eq!(
            ensured.is_error,
            Some(true),
            "{path} reads or creates a project outside the globs: {ensured:?}"
        );
    }
    assert!(
        srv.store.projects().unwrap().len() == 2,
        "project created outside the globs"
    );

    // A path that normalizes to somewhere inside the globs still works.
    let inside = call(
        &client,
        "hold_paths",
        json!({ "project": srv.path("sandbox/w/x/../a"), "paths": ["y"] }),
    )
    .await;
    assert_ne!(inside.is_error, Some(true), "{inside:?}");
    srv.ct.cancel();
}

/// BH-3. `claim_request` only checks who is acting: a token can claim or complete any request
/// on the node, however far outside its globs, and is told its title.
#[tokio::test]
async fn bh3_a_token_cannot_touch_requests_outside_its_scope() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let ask = |from: &str, to: &str, subject: &str| {
        srv.store
            .send_message(another_core::SendMessage {
                from: from.into(),
                to: vec![to.into()],
                subject: subject.into(),
                body: "private".into(),
                request: Some(Default::default()),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .unwrap()
    };
    let claim = ask("insider@data", "colleague@data", "private claim");
    let complete = ask("insider@data", "colleague@data", "private complete");

    let claimed = call(&client, "claim_request", json!({ "request_id": claim })).await;
    assert_eq!(claimed.is_error, Some(true), "claimed: {claimed:?}");
    let forged = call(
        &client,
        "claim_request",
        json!({ "request_id": complete, "action": "complete", "note": "forged" }),
    )
    .await;
    assert_eq!(forged.is_error, Some(true), "completed: {forged:?}");

    assert_eq!(srv.store.request(&claim).unwrap().owner, None);
    assert_eq!(
        srv.store.request(&complete).unwrap().status.as_str(),
        "open"
    );

    // Out of scope looks exactly like missing, and reveals nothing about the request.
    let missing = "01JZZZZZZZZZZZZZZZZZZZZZZZ";
    let absent = call(&client, "claim_request", json!({ "request_id": missing })).await;
    assert_eq!(
        text(&claimed).replace(&claim, "ID"),
        text(&absent).replace(missing, "ID"),
        "an out-of-scope request does not look missing"
    );
    assert!(text(&claimed).contains("not_found"), "{claimed:?}");
    assert!(!text(&claimed).contains("private claim"), "{claimed:?}");

    // A request addressed to the token's agent can still be claimed and completed.
    let mine = ask("insider@data", "worker@a", "for worker");
    let ok = call(
        &client,
        "claim_request",
        json!({ "request_id": mine, "action": "complete" }),
    )
    .await;
    assert_ne!(ok.is_error, Some(true), "{ok:?}");
    assert_eq!(srv.store.request(&mine).unwrap().status.as_str(), "done");
    srv.ct.cancel();
}

/// BH-12. `threads` is limited to threads the token's agent is in, but `send_message` lets it
/// join any thread by replying (or naming the thread), and that then opens the whole history.
#[tokio::test]
async fn bh12_a_reply_does_not_open_a_thread_the_token_cannot_read() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let private = srv
        .store
        .send_message(another_core::SendMessage {
            from: "insider@data".into(),
            to: vec!["colleague@data".into()],
            subject: "private subject".into(),
            body: "the private body".into(),
            ..Default::default()
        })
        .unwrap();
    let read = json!({ "thread_id": private.thread_id });

    let before = call(&client, "threads", read.clone()).await;
    assert_eq!(before.is_error, Some(true), "not a participant yet");

    let attempts = [
        json!({
            "to": ["insider@data"],
            "subject": "hello",
            "body": "let me in",
            "reply_to": private.message_id,
        }),
        // No subject: the tool would otherwise copy the private one.
        json!({ "to": ["insider@data"], "body": "let me in", "reply_to": private.message_id }),
        // No recipients: a plain reply to the private sender.
        json!({ "body": "let me in", "reply_to": private.message_id }),
    ];
    for args in attempts {
        let sent = call(&client, "send_message", args.clone()).await;
        assert_eq!(sent.is_error, Some(true), "{args}: {sent:?}");
        let t = text(&sent);
        assert!(t.contains("not_found"), "{args}: {t}");
        assert!(!t.contains("private subject"), "{args}: {t}");
    }
    // `send_message` has no `thread_id` argument; naming one must not post into the thread.
    let _ = call(
        &client,
        "send_message",
        json!({
            "to": ["insider@data"],
            "subject": "hello",
            "body": "let me in",
            "thread_id": private.thread_id,
        }),
    )
    .await;
    let after = call(&client, "threads", read).await;
    let t = text(&after);
    assert!(
        !t.contains("the private body"),
        "a reply opened the private thread: {t}"
    );
    assert_eq!(
        srv.store.thread(&private.thread_id).unwrap().messages.len(),
        1
    );

    // A thread the token's agent is in can still be replied to.
    let mine = srv
        .store
        .send_message(another_core::SendMessage {
            from: "insider@data".into(),
            to: vec!["worker@a".into()],
            subject: "for worker".into(),
            body: "hi".into(),
            ..Default::default()
        })
        .unwrap();
    let reply = call(
        &client,
        "send_message",
        json!({ "body": "hi back", "reply_to": mine.message_id }),
    )
    .await;
    assert_ne!(reply.is_error, Some(true), "{reply:?}");
    srv.ct.cancel();
}

/// BH-13. `authorize` checks the `project` argument, but renew and release ignore it and act on
/// every hold the agent has: an in-scope project unlocks holds outside the globs.
#[tokio::test]
async fn bh13_renew_and_release_stay_inside_the_tokens_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let hold = |project: &str, path: &str| acquire(&srv.store, "worker@a", project, path);
    let outside = hold(&srv.path("private/data"), "local-work.rs");
    let outside_expiry = live_holds(&srv.store)[0].1.clone();
    let untouched = || vec![(outside.clone(), outside_expiry.clone())];
    let inside = hold(&srv.path("sandbox/w/a"), "remote-work.rs");

    // Exact-subject renewal stays in scope and cannot renew the hidden file.
    let renew = call(
        &client,
        "hold_paths",
        json!({ "action": "renew", "project": srv.path("sandbox/w/a"), "hold_ids": [inside] }),
    )
    .await;
    assert_ne!(renew.is_error, Some(true), "{renew:?}");
    assert_eq!(
        ids(&out(&renew)["renewed"]),
        vec![inside.clone()],
        "{renew:?}"
    );
    let live = live_holds(&srv.store);
    assert!(live.contains(&untouched()[0]), "renew moved it: {live:?}");

    // Naming the hidden hold looks exactly like naming an unknown one.
    let unknown = "01JZZZZZZZZZZZZZZZZZZZZZZZ";
    for action in ["renew", "release"] {
        let by_id = |id: &str| json!({ "action": action, "project": srv.path("sandbox/w/a"), "hold_ids": [id] });
        let named = call(&client, "hold_paths", by_id(&outside)).await;
        let absent = call(&client, "hold_paths", by_id(unknown)).await;
        assert_eq!(named.is_error, Some(true), "{action} by id: {named:?}");
        assert!(text(&named).contains("not_found"), "{named:?}");
        assert_eq!(
            text(&named).replace(&outside, "ID"),
            text(&absent).replace(unknown, "ID"),
            "{action}: a hidden hold does not look missing"
        );
    }

    // The path form of release (D92) stays inside the globs too: naming the hidden hold's
    // file is refused, and nothing is released.
    let by_path = call(
        &client,
        "hold_paths",
        json!({ "action": "release", "project": srv.path("sandbox/w/a"),
                "paths": [srv.path("private/data/local-work.rs")] }),
    )
    .await;
    assert_eq!(by_path.is_error, Some(true), "{by_path:?}");
    assert!(
        text(&by_path).contains("outside your projects"),
        "{by_path:?}"
    );
    assert_eq!(live_holds(&srv.store).len(), 2, "release by path");

    // Release all: only the in-scope hold.
    let release = call(
        &client,
        "hold_paths",
        json!({ "action": "release", "project": srv.path("sandbox/w/a") }),
    )
    .await;
    assert_ne!(release.is_error, Some(true), "{release:?}");
    assert_eq!(out(&release)["released"], json!([inside]), "{release:?}");
    assert_eq!(live_holds(&srv.store), untouched(), "release");

    // Presence renews no file; an end still releases only the in-scope hold.
    let again = hold(&srv.path("sandbox/w/a"), "remote-work.rs");
    let beat = call(&client, "heartbeat", json!({})).await;
    assert_ne!(beat.is_error, Some(true), "{beat:?}");
    assert_eq!(
        ids(&out(&beat)["renewed_holds"]),
        Vec::<String>::new(),
        "heartbeat: {beat:?}"
    );
    let end = call(&client, "heartbeat", json!({ "end": true })).await;
    assert_ne!(end.is_error, Some(true), "{end:?}");
    assert_eq!(out(&end)["released_holds"], json!([again]), "{end:?}");
    assert_eq!(live_holds(&srv.store), untouched(), "heartbeat end");
    srv.ct.cancel();
}

/// BH-13, for resource leases. A remote heartbeat renews, and its end releases, only the
/// leases on resources in the token's projects; the agent's other leases are left alone.
#[tokio::test]
async fn bh13_heartbeat_leases_stay_inside_the_tokens_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let register = |project: &str, name: &str| {
        srv.store
            .register_resource(another_core::RegisterResource {
                agent: "worker@a".into(),
                kind: another_core::ResourceKind::LongJob,
                name: name.into(),
                endpoint: None,
                port: None,
                pid: None,
                project: Some(project.into()),
                purpose: None,
                mode: Default::default(),
                ttl_seconds: Some(600),
                metadata: None,
            })
            .unwrap()
            .resource
            .resource_id
    };
    let hidden = register(&srv.path("private/data"), "local-job");
    let visible = register(&srv.path("sandbox/w/a"), "remote-job");
    let leases = |id: &str| {
        srv.store
            .resource(&another_core::ResourceRef::Id(id.into()))
            .unwrap()
            .leases
            .into_iter()
            .map(|l| (l.lease_id, l.expires_at))
            .collect::<Vec<_>>()
    };
    let hidden_leases = leases(&hidden);
    assert_eq!(hidden_leases.len(), 1);

    let beat = call(&client, "heartbeat", json!({})).await;
    assert_ne!(beat.is_error, Some(true), "{beat:?}");
    assert_eq!(out(&beat)["resource_leases"], json!(0), "beat: {beat:?}");
    assert_eq!(
        leases(&hidden),
        hidden_leases,
        "beat renewed a hidden lease"
    );

    let end = call(&client, "heartbeat", json!({ "end": true })).await;
    assert_ne!(end.is_error, Some(true), "{end:?}");
    assert_eq!(out(&end)["resource_leases"], json!(1), "end: {end:?}");
    assert!(leases(&visible).is_empty(), "the visible lease is released");
    assert_eq!(
        leases(&hidden),
        hidden_leases,
        "end released a hidden lease"
    );
    srv.ct.cancel();
}

/// A local MCP caller: the server in process, with no token.
async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = another::mcp::AnotherMcp::legacy_repair(store)
            .serve(server_io)
            .await
            .unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

/// A8 (D92). `hold_paths` with `action: "release"` and `paths` frees the files an agent is
/// done with. A remote caller stays inside its token: a path outside its projects is
/// refused before the disk is read, and nothing it is told names a checkout directory.
#[tokio::test]
async fn mcp_release_with_paths() {
    let srv = start().await;
    let (inside, outside) = (srv.path("sandbox/w/a"), srv.path("private/data"));
    let hold = |project: &str, path: &str| acquire(&srv.store, "worker@a", project, path);
    let reasons = || -> Vec<String> {
        srv.store
            .events(1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "hold.released")
            .map(|e| e.payload["reason"].as_str().unwrap().to_string())
            .collect()
    };

    // A local caller: relative paths start at `project`, an absolute path may lie anywhere.
    let local = in_process(srv.store.clone()).await;
    let a = hold(&inside, "src/a.rs");
    let b = hold(&outside, "notes.rs");
    let kept = hold(&inside, "src/kept.rs");
    let elsewhere = std::path::Path::new(&outside).join("notes.rs");
    let freed = call(
        &local,
        "hold_paths",
        json!({ "agent": "worker@a", "action": "release", "project": inside,
                "paths": ["src/a.rs", elsewhere] }),
    )
    .await;
    assert_ne!(freed.is_error, Some(true), "{freed:?}");
    assert_eq!(out(&freed)["released"], json!([a, b]), "{freed:?}");
    assert_eq!(out(&freed)["notes"], json!([]));
    assert_eq!(reasons(), ["done"]);
    assert_eq!(
        live_holds(&srv.store)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![kept.clone()]
    );
    // Paths need the project they start from; nothing is released without it.
    let no_base = call(
        &local,
        "hold_paths",
        json!({ "agent": "worker@a", "action": "release", "paths": ["src/kept.rs"] }),
    )
    .await;
    assert_eq!(no_base.is_error, Some(true), "{no_base:?}");
    assert!(
        text(&no_base).contains("project is required"),
        "{no_base:?}"
    );
    // Ids still work beside paths, and nothing held is said, not an error.
    let again = hold(&inside, "src/a.rs");
    let both = call(
        &local,
        "hold_paths",
        json!({ "agent": "worker@a", "action": "release", "project": inside,
                "hold_ids": [kept], "paths": ["src/a.rs", "src/none.rs"] }),
    )
    .await;
    assert_eq!(out(&both)["released"], json!([kept, again]), "{both:?}");
    let notes = out(&both)["notes"].clone();
    assert_eq!(notes.as_array().unwrap().len(), 1, "{notes}");
    assert!(
        notes[0]
            .as_str()
            .unwrap()
            .starts_with("nothing of yours is held at "),
        "{notes}"
    );
    assert!(live_holds(&srv.store).is_empty());

    // A remote caller. `private` is a git checkout it has no business learning about.
    std::fs::create_dir_all(srv._dir.path().join("private/.git")).unwrap();
    let client = connect(&srv.base, &srv.token().secret).await;
    let hidden = hold(&outside, "local-work.rs");
    let visible = hold(&inside, "remote-work.rs");
    let release = |paths: Value| {
        call(
            &client,
            "hold_paths",
            json!({ "action": "release", "project": inside, "paths": paths }),
        )
    };
    // Outside its projects: refused, and an existing file reads exactly like a missing one.
    let mut refusals = Vec::new();
    for (name, path) in [
        ("local-work.rs", srv.path("private/data/local-work.rs")),
        ("no-such.rs", srv.path("private/data/no-such.rs")),
        (
            "local-work.rs",
            "../../../private/data/local-work.rs".to_string(),
        ),
    ] {
        let refused = release(json!([path])).await;
        assert_eq!(refused.is_error, Some(true), "{path}: {refused:?}");
        let said = text(&refused);
        assert!(said.contains("outside your projects"), "{said}");
        assert!(!said.contains("checkout"), "{said}");
        refusals.push(said.replace(name, "NAME"));
    }
    assert_eq!(refusals[0], refusals[1], "no probing of the disk");
    assert_eq!(live_holds(&srv.store).len(), 2, "nothing was released");
    // Inside its projects with nothing held: the path as given, and no directory.
    let nothing = release(json!(["src/none.rs"])).await;
    assert_ne!(nothing.is_error, Some(true), "{nothing:?}");
    assert_eq!(out(&nothing)["released"], json!([]));
    assert_eq!(
        out(&nothing)["notes"],
        json!(["nothing of yours is held at src/none.rs"])
    );
    // Its own hold in its project goes; the one outside its token stays.
    let done = release(json!(["remote-work.rs"])).await;
    assert_eq!(out(&done)["released"], json!([visible]), "{done:?}");
    assert_eq!(
        live_holds(&srv.store)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![hidden.clone()]
    );
    // A link inside its project that leads to a file outside it: the path as written is
    // inside, so it is looked at, but the hold on that file is in a project the token does
    // not cover and stays.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            srv._dir.path().join("private/data/local-work.rs"),
            std::path::Path::new(&inside).join("link.rs"),
        )
        .unwrap();
        let through = release(json!(["link.rs"])).await;
        assert_ne!(through.is_error, Some(true), "{through:?}");
        assert_eq!(out(&through)["released"], json!([]));
        assert_eq!(
            out(&through)["notes"],
            json!(["nothing of yours is held at link.rs"])
        );
        assert_eq!(live_holds(&srv.store).len(), 1);
    }

    // A6, for a remote caller: a pattern that points into another checkout is refused
    // without the absolute path or the command a local caller is given.
    std::fs::create_dir_all(std::path::Path::new(&inside).join("nested/.git")).unwrap();
    for action in ["acquire", "check"] {
        let refused = call(
            &client,
            "hold_paths",
            json!({ "action": action, "project": inside, "paths": ["nested/src/**"] }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{action}: {refused:?}");
        let said = text(&refused);
        assert!(
            said.contains(
                "nested/src/** is inside another checkout; a hold taken here would not apply \
                 to its files. Take it in that checkout's project."
            ),
            "{said}"
        );
        assert!(!said.contains("another hold acquire"), "{said}");
        assert!(!said.contains("sandbox"), "{said}");
    }
    // The same for a pattern that a link leads out of every checkout: where it leads is
    // not said.
    #[cfg(unix)]
    {
        std::fs::create_dir_all(srv._dir.path().join("loose/data")).unwrap();
        std::os::unix::fs::symlink(
            srv._dir.path().join("loose"),
            std::path::Path::new(&inside).join("out"),
        )
        .unwrap();
        let refused = call(
            &client,
            "hold_paths",
            json!({ "action": "acquire", "project": inside, "paths": ["out/data/**"] }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        let said = text(&refused);
        assert!(
            said.contains(
                "out/data/** points outside any checkout; a hold taken here would apply to \
                 nothing"
            ),
            "{said}"
        );
        assert!(!said.contains("loose"), "{said}");
    }
    assert_eq!(live_holds(&srv.store).len(), 1);
    srv.ct.cancel();
}

fn out(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("structured output")
}

/// The `hold_id`s of a list of holds in a tool result.
fn ids(holds: &Value) -> Vec<String> {
    holds
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["hold_id"].as_str().unwrap().to_string())
        .collect()
}

fn acquire(store: &Store, agent: &str, project: &str, path: &str) -> String {
    store
        .acquire_holds(another_core::AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: vec![path.into()],
            exclusive: true,
            ttl_seconds: Some(600),
            reason: None,
            repo_wide: false,
        })
        .unwrap()
        .granted[0]
        .hold_id
        .clone()
}

/// Live holds as (id, expiry), read from the store.
fn live_holds(store: &Store) -> Vec<(String, String)> {
    store
        .holds(Default::default())
        .unwrap()
        .into_iter()
        .map(|h| (h.hold_id, h.expires_at))
        .collect()
}
