//! Fuzz harnesses for Another.
//!
//! Each `pub fn` takes arbitrary bytes and must never panic, hang or leave the store
//! unreadable, whatever the input. `fuzz/` runs them under libFuzzer (nightly, in CI); the
//! smoke test in `tests/smoke.rs` runs them on stable with a deterministic generator in
//! every `cargo test`. A harness panics only when a property below is violated.
//!
//! - [`ingest_events`]: signed events from a paired spoke, built from the bytes, are
//!   ingested by a hub. A paired peer's push never fails as a whole (bad events are
//!   rejected one by one, D23), and imported subjects are one line without control
//!   characters (D75).
//! - [`ingest_raw`]: the bytes as a JSON push body.
//! - [`peer_request`]: the signed node-to-node request envelope: only an exact signature
//!   by the paired key verifies.
//! - [`tool_args`]: MCP tool calls with arbitrary arguments, through a real in-process
//!   server, plus the remote gate and the audit redaction.
//! - [`cli_args`]: the command-line parser (never executes anything).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use another_core::crypto;
use another_core::{
    ActivityQuery, Clock, InboxQuery, NodeEntry, Perm, Program, RegisterAgent, Store, WireEvent,
};
use serde_json::{json, Map, Value};

const T0: i64 = 1_800_000_000_000;
/// Fixed test key for the fuzzing spoke (32 zero-ish bytes, base64url). Not a secret.
const SPOKE_SECRET: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
/// A well-formed node id (a ULID: no `I`, `L`, `O` or `U`), as pairing requires.
const SPOKE_NODE: &str = "01FZZSPQKEN0DE000000000000";

/// Reads values from the fuzz input; past the end it yields zeros, so every input is a
/// complete, deterministic recipe.
pub struct Gen<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Gen<'a> {
    pub fn new(data: &'a [u8]) -> Gen<'a> {
        Gen { data, pos: 0 }
    }

    fn byte(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.byte() as usize % n
        }
    }

    /// True for one input byte in `n`.
    fn one_in(&mut self, n: u8) -> bool {
        self.byte().is_multiple_of(n)
    }

    fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())].clone()
    }

    fn int(&mut self) -> i64 {
        match self.byte() % 6 {
            0 => 0,
            1 => -1,
            2 => i64::MAX,
            3 => i64::MIN,
            4 => T0 + (self.byte() as i64) * 1000,
            _ => i64::from_le_bytes([
                self.byte(),
                self.byte(),
                self.byte(),
                self.byte(),
                0,
                0,
                0,
                0,
            ]),
        }
    }

    /// Short strings over a palette that includes control characters, escapes, separators
    /// and non-ASCII text.
    fn string(&mut self, pool: &[String]) -> String {
        if self.one_in(3) && !pool.is_empty() {
            return self.pick(pool);
        }
        const PALETTE: &[&str] = &[
            "a",
            "Z",
            "0",
            " ",
            "@",
            "/",
            ".",
            "..",
            "*",
            "**",
            ":",
            "group:",
            "\\n",
            "\n",
            "\r",
            "\u{1b}[2J",
            "\u{7f}",
            "\u{85}",
            "\u{0}",
            "\t",
            "é",
            "日本",
            "\u{202e}",
            "\"",
            "'",
            "%",
            "Re: ",
            "Done: ",
            "-",
            "_",
        ];
        let len = self.below(12);
        (0..len).map(|_| self.pick(PALETTE)).collect()
    }

    fn value(&mut self, pool: &[String], depth: u8) -> Value {
        match self.byte() % if depth == 0 { 4 } else { 6 } {
            0 => Value::Null,
            1 => Value::Bool(self.one_in(2)),
            2 => json!(self.int()),
            3 => Value::String(self.string(pool)),
            4 => Value::Array(
                (0..self.below(4))
                    .map(|_| self.value(pool, depth - 1))
                    .collect(),
            ),
            _ => {
                let mut m = Map::new();
                for _ in 0..self.below(4) {
                    let k = self.string(&[]);
                    m.insert(k, self.value(pool, depth - 1));
                }
                Value::Object(m)
            }
        }
    }
}

/// Field names federation and the projections read.
const FIELDS: &[&str] = &[
    "message_id",
    "thread_id",
    "reply_to",
    "from",
    "to",
    "cc",
    "subject",
    "body",
    "importance",
    "ack_required",
    "request_id",
    "sender_id",
    "recipients",
    "recipient_nodes",
    "agent_id",
    "node_id",
    "name",
    "program",
    "project_id",
    "slug",
    "abs_path",
    "resource_id",
    "lease_id",
    "lease_ids",
    "hold_id",
    "hold_ids",
    "patterns",
    "expires_at",
    "ttl_seconds",
    "display_name",
    "public_key",
    "message_node",
    "resource_node",
    "resource_nodes",
    "kind",
    "mode",
    "owner_id",
    "status",
    "title",
    "due_at",
    "group_id",
    "note_id",
    "key",
    "bytes",
    "exclusive",
    "reason",
];

const KINDS: &[&str] = &[
    "agent.registered",
    "project.ensured",
    "node.created",
    "node.renamed",
    "session.started",
    "message.sent",
    "delivery.read",
    "delivery.acked",
    "request.created",
    "request.claimed",
    "request.completed",
    "hold.acquired",
    "hold.renewed",
    "hold.released",
    "hold.expired",
    "resource.registered",
    "resource.adopted",
    "resource.borrowed",
    "resource.renewed",
    "resource.released",
    "resource.lease_expired",
    "resource.transferred",
    "resource.stopped",
    "token.created",
    "note.set",
    "group.created",
    "admission.created",
    "x.unknown",
];

struct Hub {
    store: Store,
    /// Address of the hub's local agent.
    local_agent: String,
    pool: Vec<String>,
}

/// A hub with one local agent, paired with the fuzzing spoke (all permissions).
fn hub() -> Hub {
    let (clock, _) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).expect("in-memory store");
    store.set_node_name("hub").expect("name the hub");
    let local = store
        .register_agent(RegisterAgent {
            name: "codex".into(),
            program: Program::Codex,
            project: None,
            model: None,
            description: None,
        })
        .expect("local agent");
    let code = store
        .create_invite("http://127.0.0.1:1", &Perm::ALL, 3600)
        .expect("invite");
    let raw = code.trim().strip_prefix("another-invite:").expect("code");
    let invite: Value = serde_json::from_slice(&crypto::unb64(raw).expect("b64")).expect("json");
    let key = crypto::decode_secret(SPOKE_SECRET).expect("spoke key");
    let text = |v: &Value| v.as_str().expect("text").to_string();
    let public_key = crypto::public_of(&key);
    let sig = crypto::sign(
        &key,
        &crypto::pair_message(
            &text(&invite["hub"]["node_id"]),
            &text(&invite["hub"]["public_key"]),
            &text(&invite["invite_id"]),
            &crypto::sha256_hex(text(&invite["secret"]).as_bytes()),
            SPOKE_NODE,
            &public_key,
            "fuzzspoke",
            T0,
        ),
    );
    let req: another_core::PairRequest = serde_json::from_value(json!({
        "invite_id": invite["invite_id"],
        "node": {
            "node_id": SPOKE_NODE,
            "display_name": "fuzzspoke",
            "public_key": public_key,
        },
        "time_ms": T0,
        "sig": sig,
    }))
    .expect("pair request");
    store.accept_pairing(&req).expect("pairing");
    let hub_node = store.node_id().to_string();
    let pool = vec![
        hub_node,
        SPOKE_NODE.to_string(),
        local.agent_id.clone(),
        "01FUZZAGENT000000000000000".to_string(),
        "codex".to_string(),
        "codex@@hub".to_string(),
        "fz@@fuzzspoke".to_string(),
    ];
    Hub {
        store,
        local_agent: local.address,
        pool,
    }
}

/// An event signed with the fuzzing spoke's key.
fn signed(
    event_id: String,
    node_id: String,
    ts: i64,
    actor: String,
    kind: String,
    payload: String,
) -> WireEvent {
    let key = crypto::decode_secret(SPOKE_SECRET).expect("spoke key");
    let sig = crypto::sign(
        &key,
        &crypto::event_message(&event_id, &node_id, ts, &actor, &kind, &payload),
    );
    WireEvent {
        seq: 1,
        event_id,
        node_id,
        ts,
        actor,
        kind,
        payload,
        prev_id: None,
        sig,
    }
}

/// Every read surface a peer's events feed must stay readable, and imported subjects must
/// be one line without control characters.
fn check_readable(hub: &Hub) {
    let s = &hub.store;
    s.events(10_000).expect("events stay readable");
    s.activity(ActivityQuery {
        since: None,
        limit: 500,
        kinds: vec![],
    })
    .expect("activity stays readable");
    s.agents().expect("agents stay readable");
    s.requests(Default::default())
        .expect("requests stay readable");
    let inbox = s
        .inbox(
            &hub.local_agent,
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 1000,
            },
        )
        .expect("inbox stays readable");
    for m in &inbox.messages {
        assert!(
            !m.message.subject.chars().any(char::is_control),
            "an imported subject kept a control character: {:?}",
            m.message.subject
        );
    }
}

/// Signed spoke events built from `data`, ingested by a paired hub.
pub fn ingest_events(data: &[u8]) {
    let mut g = Gen::new(data);
    let hub = hub();
    let mut pool = hub.pool.clone();
    let mut events = Vec::new();
    for i in 0..g.below(8) + 1 {
        let kind = if g.one_in(8) {
            g.string(&[])
        } else {
            g.pick(KINDS).to_string()
        };
        let mut payload = Map::new();
        for _ in 0..g.below(10) {
            let field = g.pick(FIELDS).to_string();
            let v = g.value(&pool, 2);
            payload.insert(field, v);
        }
        let event_id = if g.one_in(5) {
            g.string(&pool)
        } else {
            format!("01FUZZEVENT{i:015}")
        };
        pool.push(event_id.clone());
        for v in payload.values() {
            if let Some(s) = v.as_str() {
                pool.push(s.to_string());
            }
        }
        let payload = if g.one_in(10) {
            g.string(&[])
        } else {
            Value::Object(payload).to_string()
        };
        let node_id = match g.byte() % 10 {
            0 => g.string(&hub.pool),
            _ => SPOKE_NODE.to_string(),
        };
        let actor = g.string(&pool);
        let ts = g.int();
        let mut ev = signed(event_id, node_id, ts, actor, kind, payload);
        if g.one_in(12) {
            ev.sig = g.string(&[]);
        }
        events.push(ev);
    }
    let nodes: Vec<NodeEntry> = if g.one_in(4) {
        vec![NodeEntry {
            node_id: g.string(&hub.pool),
            display_name: g.string(&[]),
            public_key: g.string(&[]),
        }]
    } else {
        vec![]
    };
    let report = hub.store.ingest(SPOKE_NODE, &events, &nodes);
    // A paired peer's push is never refused as a whole: bad events are rejected one by one
    // so the peer's cursor still advances (D23). A malformed node entry may be refused.
    if nodes.is_empty() {
        let report =
            report.unwrap_or_else(|e| panic!("a paired spoke's push failed as a whole: {e}"));
        assert_eq!(
            report.accepted + report.duplicates + report.rejected.len(),
            events.len(),
            "every event is accounted for"
        );
    }
    check_readable(&hub);
}

/// `data` as a raw JSON push body from the paired spoke (unsigned or mis-signed events).
pub fn ingest_raw(data: &[u8]) {
    let Ok(push) = serde_json::from_slice::<another_core::PushRequest>(data) else {
        return;
    };
    let hub = hub();
    let _ = hub.store.ingest(SPOKE_NODE, &push.events, &[]);
    check_readable(&hub);
}

/// The node-to-node request envelope: arbitrary fields never verify, the exact signature
/// by the paired key does.
pub fn peer_request(data: &[u8]) {
    let mut g = Gen::new(data);
    let hub = hub();
    let key = crypto::decode_secret(SPOKE_SECRET).expect("spoke key");
    let method = g.pick(&["GET", "POST", "", "get"]).to_string();
    let path = g.string(&["/fed/push".to_string(), "/fed/pull?after=0".to_string()]);
    let body: Vec<u8> = (0..g.below(32)).map(|_| g.byte()).collect();
    let time = if g.one_in(2) { T0 } else { g.int() };
    let node = if g.one_in(3) {
        g.string(&hub.pool)
    } else {
        SPOKE_NODE.to_string()
    };
    let honest = crypto::sign(&key, &crypto::request_message(&method, &path, time, &body));
    let sig = if g.one_in(2) {
        honest.clone()
    } else {
        g.string(std::slice::from_ref(&honest))
    };
    let result = hub
        .store
        .verify_peer_request(&node, &method, &path, time, &body, &sig);
    let exact = node == SPOKE_NODE && sig == honest && time.abs_diff(T0) <= 5 * 60 * 1000;
    if exact {
        result.expect("the paired spoke's exact signature verifies");
    } else if node != SPOKE_NODE || sig != honest {
        assert!(
            result.is_err(),
            "a request not signed by the paired key verified"
        );
    }
}

const TOOLS: &[&str] = &[
    "ensure_project",
    "register_agent",
    "send_message",
    "fetch_inbox",
    "threads",
    "acknowledge",
    "claim_request",
    "hold_paths",
    "heartbeat",
    "resource_register",
    "resource_find",
    "resource_borrow",
    "resource_release",
    "resource_heartbeat",
    "orphans_list",
    "resource_reap",
    "whats_new",
    "activity_feed",
    "activity_summarize",
    "work_board",
    "project_notes",
    "controlled_edit",
    "no_such_tool",
];

const ARG_KEYS: &[&str] = &[
    "agent",
    "from",
    "to",
    "cc",
    "subject",
    "body",
    "importance",
    "ack_required",
    "reply_to",
    "reply_all",
    "create_request",
    "due_in_seconds",
    "request_id",
    "action",
    "note",
    "message_ids",
    "ack",
    "thread_id",
    "limit",
    "unread_only",
    "urgent_only",
    "project",
    "paths",
    "ttl_seconds",
    "exclusive",
    "reason",
    "repo_wide",
    "hold_ids",
    "name",
    "program",
    "model",
    "description",
    "path",
    "kind",
    "target",
    "port",
    "endpoint",
    "pid",
    "since",
    "kinds",
    "key",
    "text",
    "note_id",
    "scan",
    "confirm",
    "start",
    "end",
];

struct Rig {
    rt: tokio::runtime::Runtime,
    client: rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    store: Store,
    scope: another_core::TokenScope,
}

fn rig() -> &'static Mutex<Rig> {
    static RIG: OnceLock<Mutex<Rig>> = OnceLock::new();
    RIG.get_or_init(|| {
        use rmcp::ServiceExt;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_in_memory(clock).expect("store");
        store
            .register_agent(RegisterAgent {
                name: "bot".into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .expect("agent");
        let token = store
            .create_token("bot", &["**".to_string()], None, None)
            .expect("token");
        let scope = store.verify_token(&token.secret).expect("scope");
        let server_store = store.clone();
        let client = rt.block_on(async move {
            let (server_io, client_io) = tokio::io::duplex(1 << 20);
            tokio::spawn(async move {
                if let Ok(server) = another::mcp::AnotherMcp::new(server_store)
                    .serve(server_io)
                    .await
                {
                    let _ = server.waiting().await;
                }
            });
            rmcp::model::ClientConfig::default()
                .serve(client_io)
                .await
                .expect("mcp client")
        });
        Mutex::new(Rig {
            rt,
            client,
            store,
            scope,
        })
    })
}

/// One MCP tool call with arbitrary arguments, through a real in-process server, plus the
/// remote gate and the audit redaction for the same arguments.
pub fn tool_args(data: &[u8]) {
    let mut g = Gen::new(data);
    let tool = if g.one_in(10) {
        g.string(&[])
    } else {
        g.pick(TOOLS).to_string()
    };
    let pool = vec!["bot".to_string(), "group:x".to_string(), "/tmp".to_string()];
    let mut args = Map::new();
    for _ in 0..g.below(8) {
        let k = if g.one_in(6) {
            g.string(&[])
        } else {
            g.pick(ARG_KEYS).to_string()
        };
        let v = g.value(&pool, 2);
        args.insert(k, v);
    }
    let rig = rig().lock().unwrap_or_else(|e| e.into_inner());
    // The remote gate and the audit line: any outcome but a panic is fine, the audit line is
    // JSON, and a top-level message `body` or request `note` is replaced by its size. (Note
    // `text` redaction, D70, arrives with #143 and is not on main.) The redacted value is
    // compared, not searched for: a short body can occur verbatim in the JSON syntax itself.
    let mut gated = args.clone();
    let _ = another::remote::authorize(&rig.store, &rig.scope, &tool, &mut gated);
    let audit = another::remote::redact(Some(&args));
    let logged: Value =
        serde_json::from_str(&audit).unwrap_or_else(|e| panic!("audit is not JSON ({e}): {audit}"));
    for key in ["body", "note"] {
        if let Some(Value::String(secret)) = args.get(key) {
            assert_eq!(
                logged[key],
                Value::String(format!("<{} bytes>", secret.len())),
                "audit kept {key}: {audit}"
            );
        }
    }
    // The real server answers (with a result or an error) and stays alive.
    let call = rmcp::model::CallToolRequestParams::new(tool.clone()).with_arguments(args);
    let outcome = rig.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), rig.client.call_tool(call)).await
    });
    match outcome {
        Err(_) => panic!("tool {tool:?} did not answer within 20 s"),
        Ok(Ok(_)) => {}
        // Invalid parameters and unknown tools are protocol errors, not crashes.
        Ok(Err(rmcp::ServiceError::McpError(_))) => {}
        Ok(Err(e)) => panic!("tool {tool:?} broke the server: {e}"),
    }
}

/// The command-line parser on arbitrary argument vectors. Parsing only; nothing runs.
pub fn cli_args(data: &[u8]) {
    use another::cli::Cli;
    use clap::Parser;
    let mut g = Gen::new(data);
    let words: &[&str] = &[
        "send",
        "inbox",
        "request",
        "requests",
        "hold",
        "holds",
        "acquire",
        "release",
        "note",
        "set",
        "wake",
        "daemon",
        "serve",
        "token",
        "pair",
        "--json",
        "--agent",
        "--project",
        "--to",
        "--from",
        "--subject",
        "--due",
        "--ttl",
        "-a",
        "-p",
        "--",
        "-",
        "--help",
        "--version",
        "claim",
        "next",
        "board",
        "activity",
        "--since",
        "1h",
    ];
    let mut argv = vec!["another".to_string()];
    for _ in 0..g.below(10) {
        if g.one_in(3) {
            argv.push(g.string(&[]));
        } else {
            argv.push(g.pick(words).to_string());
        }
    }
    let _ = Cli::try_parse_from(argv);
}
