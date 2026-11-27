//! Q29, the rules one by one: the nonce and the signed answer on the hub, the check on the
//! spoke, the signed invite, what `last_sync_at` means, the warnings, and a node of this
//! build against one of an older build in every mix. The attacks these rules close are in
//! `federation_relay.rs`, which also compiles on the build before the fix.

#[path = "common/relay.rs"]
mod relay;

use std::path::Path;
use std::time::Duration;

use another::fed::{self, Exchange, Limits};
use another_core::*;
use relay::*;
use serde_json::{json, Value};

// The texts of the spec, each in full.
const J1: &str = "this invite is not signed by its hub: an older build made it, or its \
                  signature was removed; not joined. Make a new invite on a hub that runs this \
                  build or a newer one";
const J2: &str = "this invite's signature does not match its contents: the code was changed \
                  after the hub made it, or a newer build made it in a form this build cannot \
                  check; not joined. Ask the hub's owner for a new invite over a channel you \
                  trust, and update this node if the hub runs a newer build";
const J3: &str = "this invite is signed but in a format this build does not know (v 3); not \
                  joined. Update this node to the hub's build";
const N1: &str = "this hub signs its answers and serves only nodes that ask for that, and this \
                  request did not. Update this node to the hub's build, then run `another \
                  sync`. Nothing was stored, nothing is lost, and the node does not need to \
                  pair again";
const NO_NONCE_AUDIT: &str = "no x-another-nonce header: the node runs an older build, or \
                              something on the path removed it";
const BAD_NONCE: &str = "x-another-nonce must be 32 lowercase hex characters";

fn u1(address: &str, what: &str) -> String {
    format!(
        "the hub at {address} answered the {what} without a signature; refused, and nothing \
         from that answer was applied. Either the hub runs an older build (update the hub and \
         its spokes together), or something between this node and the hub answered for it or \
         removed the signature"
    )
}

fn u2(address: &str, what: &str, fingerprint: &str) -> String {
    format!(
        "the hub at {address} answered the {what} with a signature that is not the hub's (key \
         {fingerprint}) for this request and this body; refused, and nothing from that answer \
         was applied. Something between this node and the hub changed the answer or replayed \
         another one; a proxy that rewrites answer bodies has the same effect"
    )
}

fn u3(address: &str, what: &str) -> String {
    format!(
        "the hub at {address} refused the {what} as carrying no nonce, but this node sent one; \
         nothing was applied. Something between this node and the hub removed the header, or \
         the refusal is not the hub's; if a proxy in front of the hub demands TLS, the hub's \
         address may need to be https://"
    )
}

fn k(node_id: &str, listed: &str, pinned: &str) -> String {
    format!(
        "the hub's signed node list gives node {node_id} the key {listed}, but this node \
         pinned the key {pinned} for it earlier; the answer is refused and this node cannot \
         sync. A node's key never changes, so one of the two is wrong. Compare both with the \
         fingerprint that `another node show` prints on that node. If the pinned key is the \
         wrong one, this home must be replaced; if the hub's is, the hub's home must be: see \
         \"A pinned key that is wrong\" in docs/FEDERATION.md"
    )
}

/// A hub of the build before signed answers: no nonce asked for, no answer signed.
fn start_older_hub(name: &str) -> Hub {
    let node = node(name);
    let served = serve(older_router(&node));
    Hub { node, served }
}

fn older_router(node: &Node) -> axum::Router {
    fed::router_as(node.store.clone(), Limits::CURRENT, Exchange::Unsigned)
}

/// A sync as a spoke of the build before signed answers runs it.
fn older_sync(spoke: &Node) -> anyhow::Result<fed::SyncReport> {
    fed::sync_as(&spoke.store, &Limits::CURRENT, Exchange::Unsigned)
}

fn client() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    )
}

/// A join as a build before signed invites made it: `sig` is not looked at.
fn older_join(spoke: &Node, code: &str) {
    let (invite, req) = spoke
        .store
        .prepare_join_before_signed_invites(code)
        .unwrap();
    let mut resp = client()
        .post(&format!("{}/fed/pair", invite.hub_url))
        .header("content-type", "application/json")
        .send(serde_json::to_vec(&req).unwrap().as_slice())
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let paired: PairResponse =
        serde_json::from_slice(&resp.body_mut().read_to_vec().unwrap()).unwrap();
    spoke.store.complete_join(&invite, &paired).unwrap();
}

fn error_text(result: anyhow::Result<impl std::fmt::Debug>) -> String {
    match result {
        Ok(value) => panic!("expected a failure, got {value:?}"),
        Err(e) => format!("{e:#}"),
    }
}

/// The failure of a sync, with the kind it has when it is one of the core's errors.
fn sync_error(spoke: &Node) -> (String, Option<&'static str>) {
    match fed::sync(&spoke.store) {
        Ok(report) => panic!("the sync went through: {report:?}"),
        Err(e) => (
            format!("{e:#}"),
            e.downcast_ref::<another_core::Error>().map(|e| e.code()),
        ),
    }
}

/// The private key of a test node, as stored in its temporary home.
fn secret_of(node: &Node) -> String {
    node.db()
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap()
}

/// Sign `invite` as it now reads with the private key `secret`, as its hub would have.
fn resign(invite: &mut Value, secret: &str) {
    let perms: Vec<&str> = invite["perms"]
        .as_array()
        .unwrap()
        .iter()
        .map(text)
        .collect();
    let message = crypto::invite_message(
        invite["v"].as_u64().unwrap() as u8,
        text(&invite["hub"]["node_id"]),
        text(&invite["hub"]["public_key"]),
        text(&invite["hub"]["display_name"]),
        text(&invite["hub_url"]),
        text(&invite["invite_id"]),
        &crypto::sha256_hex(text(&invite["secret"]).as_bytes()),
        &perms.join(","),
        invite["expires_at"].as_i64().unwrap(),
    );
    invite["sig"] = json!(crypto::sign(
        &crypto::decode_secret(secret).unwrap(),
        &message
    ));
}

/// Every row of `nodes`, `peers` and `invites`, without the private key column.
fn pairing_rows(node: &Node) -> Vec<String> {
    use rusqlite::types::ValueRef;
    let conn = node.db();
    let mut out = Vec::new();
    for table in ["nodes", "peers", "invites"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
        let mut found = stmt.query([]).unwrap();
        while let Some(row) = found.next().unwrap() {
            let mut line = table.to_string();
            for (i, name) in names.iter().enumerate() {
                if name == "secret_key" {
                    continue;
                }
                let value = match row.get_ref(i).unwrap() {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Integer(n) => n.to_string(),
                    ValueRef::Real(n) => n.to_string(),
                    ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                    ValueRef::Blob(b) => format!("{b:?}"),
                };
                line.push_str(&format!(" {name}={value}"));
            }
            out.push(line);
        }
    }
    out
}

/// What the hub has stored as the last sync of `spoke`.
fn contact(hub: &Node, spoke_id: &str) -> Option<i64> {
    hub.db()
        .query_row(
            "SELECT last_sync_at FROM peers WHERE node_id = ?1",
            [spoke_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// One answer of the hub to a request made by hand.
struct Raw {
    status: u16,
    signature: Option<String>,
    upgrade: Option<String>,
    body: Vec<u8>,
    /// The text of the request's own signature header.
    request_signature: String,
}

impl Raw {
    fn error(&self) -> (String, String) {
        let v: Value = serde_json::from_slice(&self.body).unwrap();
        (
            text(&v["error"]["code"]).to_string(),
            text(&v["error"]["message"]).to_string(),
        )
    }
}

/// How a hand-made request differs from the one a spoke of this build sends.
#[derive(Clone, Copy)]
enum Signing {
    /// Signed by the node, as a spoke signs.
    Good,
    /// No signature headers at all.
    None,
}

/// A push (with `body`) or a pull (without) from `spoke`, made by hand.
fn request(
    hub_url: &str,
    spoke: &Node,
    path: &str,
    body: Option<&[u8]>,
    nonce: Option<&str>,
    signing: Signing,
) -> Raw {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let method = if body.is_some() { "POST" } else { "GET" };
    let request_signature = spoke
        .store
        .sign_request(method, path, time, body.unwrap_or_default());
    let mut headers: Vec<(&str, String)> = Vec::new();
    if let Signing::Good = signing {
        headers.push(("x-another-node", spoke.id()));
        headers.push(("x-another-time", time.to_string()));
        headers.push(("x-another-signature", request_signature.clone()));
    }
    if let Some(nonce) = nonce {
        headers.push(("x-another-nonce", nonce.to_string()));
    }
    let url = format!("{hub_url}{path}");
    let agent = client();
    let mut resp = match body {
        Some(body) => {
            let mut r = agent.post(&url).header("content-type", "application/json");
            for (n, v) in &headers {
                r = r.header(*n, v.as_str());
            }
            r.send(body).unwrap()
        }
        None => {
            let mut r = agent.get(&url);
            for (n, v) in &headers {
                r = r.header(*n, v.as_str());
            }
            r.call().unwrap()
        }
    };
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    Raw {
        status: resp.status().as_u16(),
        signature: header("x-another-signature"),
        upgrade: header("upgrade"),
        body: {
            let _ = &header;
            resp.body_mut().read_to_vec().unwrap()
        },
        request_signature,
    }
}

/// The body of a push of everything `spoke` has to send.
fn push_body(spoke: &Node) -> Vec<u8> {
    let (events, _) = spoke.store.outbound_events(0, 500).unwrap();
    assert!(!events.is_empty());
    serde_json::to_vec(&PushRequest { events }).unwrap()
}

const NONCE: &str = "00112233445566778899aabbccddeeff";

/// Run the built binary on a temporary home. Every run has its own `ANOTHER_HOME`.
fn run(home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    run_with(home, args, &[])
}

fn run_with(home: &Path, args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let user = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_another"));
    command
        .args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", user.path())
        .env("USERPROFILE", user.path())
        .env_remove("ANOTHER_NODE_NAME");
    for name in [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
    ] {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let out = command.output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The `peers` check of `another doctor --json` on `home`.
fn doctor_peers(home: &Path) -> Value {
    let (_, out, err) = run(home, &["--json", "doctor"]);
    let report: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out} {err}"));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "peers")
        .unwrap()
        .clone()
}

/// A home whose pinned key for `other` is not the one the hub lists: what a spoke that a
/// relay fed its own key looks like once it reaches the real hub. Returns the wrong key.
fn pin_a_wrong_key(spoke: &Node, other_id: &str) -> String {
    let wrong = crypto::public_of(&crypto::generate_key().unwrap());
    let changed = spoke
        .db()
        .execute(
            "UPDATE nodes SET public_key = ?1 WHERE node_id = ?2",
            [&wrong, other_id],
        )
        .unwrap();
    assert_eq!(changed, 1);
    wrong
}

// ---------------------------------------------------------------------------
// S1, S2, S3: the nonce and the signed answer on the hub.

#[test]
fn every_request_carries_a_fresh_nonce() {
    // A hub that closes a pull page after one event, and a spoke that closes a push batch
    // after one: several of each in one sync.
    let one_event = Limits {
        push_budget: 1,
        pull_budget: 1,
        ..Limits::CURRENT
    };
    let hub_node = node("hub");
    let hub = Hub {
        served: serve(fed::router_with(hub_node.store.clone(), one_event)),
        node: hub_node,
    };
    let vic = node("vic");
    let late = node("late");
    join(&hub, &vic);
    join(&hub, &late);
    register(&late.store, "l1");
    register(&late.store, "l2");
    fed::sync(&late.store).unwrap();
    register(&vic.store, "v1");
    register(&vic.store, "v2");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());

    fed::sync_with(&vic.store, &one_event).unwrap();
    let seen = relay.seen();
    let pushes = seen.iter().filter(|r| r.is_push()).count();
    let pulls = seen.iter().filter(|r| r.is_pull()).count();
    assert!(pushes >= 2 && pulls >= 2, "{pushes} pushes, {pulls} pulls");
    let mut nonces: Vec<&str> = seen
        .iter()
        .map(|r| {
            r.header("x-another-nonce")
                .expect("a nonce on every request")
        })
        .collect();
    for nonce in &nonces {
        assert_eq!(nonce.len(), 32, "{nonce}");
        assert!(
            nonce
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{nonce}"
        );
    }
    let sent = nonces.len();
    nonces.sort_unstable();
    nonces.dedup();
    assert_eq!(nonces.len(), sent, "every nonce is new");
}

#[test]
fn the_hub_signs_exactly_the_bytes_it_sends() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let hub_key = hub.node.store.node_entry().unwrap().public_key;
    let body = push_body(&vic);
    for (path, body) in [
        ("/fed/push", Some(&body[..])),
        ("/fed/pull?after=0&limit=500", None),
    ] {
        let answer = request(hub.url(), &vic, path, body, Some(NONCE), Signing::Good);
        assert_eq!(answer.status, 200, "{path}");
        let signature = answer.signature.expect("a signature on every 200");
        let signed = |nonce: &str, request_signature: &str, body: &[u8]| {
            crypto::verify_strict(
                &hub_key,
                &crypto::answer_message(&hub.id(), &vic.id(), nonce, request_signature, body),
                &signature,
            )
        };
        assert!(
            signed(NONCE, &answer.request_signature, &answer.body),
            "{path}: the body received is the body signed, for the signature header sent"
        );
        assert!(!signed(
            "ffeeddccbbaa99887766554433221100",
            &answer.request_signature,
            &answer.body
        ));
        assert!(!signed(NONCE, "another-signature", &answer.body));
        let mut changed = answer.body.clone();
        changed.push(b' ');
        assert!(!signed(NONCE, &answer.request_signature, &changed));
        // The body is the answer itself, unchanged in form.
        let parsed: Value = serde_json::from_slice(&answer.body).unwrap();
        assert!(parsed.get("accepted").is_some() || parsed.get("events").is_some());
    }
}

/// CRC-32 as gzip uses it.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `bytes` as a gzip stream of stored blocks: a valid content encoding that any gzip
/// reader decodes back to `bytes`.
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut blocks = bytes.chunks(0xffff).peekable();
    if blocks.peek().is_none() {
        out.extend([1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        out.push(u8::from(blocks.peek().is_none()));
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend(block);
    }
    out.extend(crc32(bytes).to_le_bytes());
    out.extend((bytes.len() as u32).to_le_bytes());
    out
}

#[test]
fn a_gzip_encoded_answer_verifies() {
    let (hub, vic, late) = {
        let hub = start_hub();
        let (vic, late) = (node("vic"), node("late"));
        join(&hub, &vic);
        join(&hub, &late);
        (hub, vic, late)
    };
    register(&vic.store, "v1");
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|req, hub| {
        let mut answer = hub.send(&req);
        answer.body = gzip(&answer.body);
        answer
            .headers
            .push(("content-encoding".into(), "gzip".into()));
        answer
    });

    let report = fed::sync(&vic.store).unwrap();
    assert!(report.pushed > 0 && report.applied > 0, "{report:?}");
    assert!(events_from(&vic, &late.id()) > 0);
    assert!(events_from(&hub.node, &vic.id()) > 0);
}

#[test]
fn a_request_without_a_nonce_is_refused_before_anything_is_stored() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let rows = pairing_rows(&hub.node);
    let body = push_body(&vic);
    for (path, body, operation) in [
        ("/fed/push", Some(&body[..]), "POST /fed/push"),
        ("/fed/pull?after=0&limit=500", None, "GET /fed/pull"),
    ] {
        let answer = request(hub.url(), &vic, path, body, None, Signing::Good);
        assert_eq!(answer.status, 426, "{path}");
        assert_eq!(answer.upgrade.as_deref(), Some("another-signed-sync/1"));
        assert_eq!(answer.signature, None);
        assert_eq!(answer.error(), ("invalid".to_string(), N1.to_string()));
        let audit = audit_rows(&hub.node.store, operation);
        assert_eq!(audit.len(), 1, "{audit:?}");
        assert_eq!(audit[0].outcome, "denied");
        assert_eq!(audit[0].detail.as_deref(), Some(NO_NONCE_AUDIT));
        assert_eq!(
            audit[0].agent.as_deref(),
            Some(format!("node:{}", vic.id()).as_str())
        );
    }
    assert_eq!(events_from(&hub.node, &vic.id()), 0, "no event stored");
    assert_eq!(
        pairing_rows(&hub.node),
        rows,
        "the spoke's row is unchanged"
    );
    assert_eq!(contact(&hub.node, &vic.id()), None);
}

#[test]
fn a_malformed_nonce_is_answered_400_before_ingest() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let body = push_body(&vic);
    let malformed = [
        "xyz",
        "00112233445566778899aabbccddeef",
        "00112233445566778899aabbccddeeff0",
        "00112233445566778899AABBCCDDEEFF",
        "",
    ];
    for nonce in malformed {
        for (path, body) in [
            ("/fed/push", Some(&body[..])),
            ("/fed/pull?after=0&limit=500", None),
        ] {
            let answer = request(hub.url(), &vic, path, body, Some(nonce), Signing::Good);
            assert_eq!(answer.status, 400, "{nonce:?} {path}");
            assert_eq!(
                answer.error(),
                ("invalid".to_string(), BAD_NONCE.to_string())
            );
            assert_eq!(answer.signature, None);
        }
    }
    assert_eq!(events_from(&hub.node, &vic.id()), 0, "no event stored");
    for operation in ["POST /fed/push", "GET /fed/pull"] {
        let audit = audit_rows(&hub.node.store, operation);
        assert_eq!(audit.len(), malformed.len(), "{audit:?}");
        assert!(audit
            .iter()
            .all(|a| a.outcome == "denied" && a.detail.as_deref() == Some(BAD_NONCE)));
    }
    assert_eq!(contact(&hub.node, &vic.id()), None);
}

#[test]
fn the_nonce_checks_come_after_authentication() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    // A node with a key of its own that never paired with this hub.
    let stranger = node("stranger");
    register(&stranger.store, "s1");
    for nonce in [None, Some("xyz")] {
        for (who, signing) in [(&vic, Signing::None), (&stranger, Signing::Good)] {
            let body = push_body(who);
            for (path, body) in [
                ("/fed/push", Some(&body[..])),
                ("/fed/pull?after=0&limit=500", None),
            ] {
                let answer = request(hub.url(), who, path, body, nonce, signing);
                assert_eq!(answer.status, 401, "{nonce:?} {path}");
                assert_eq!(answer.upgrade, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// S4, S6, K1: the check on the spoke.

#[test]
fn a_refused_answer_writes_an_audit_row_and_applies_nothing() {
    let hub = start_hub();
    let vic = node("vic");
    let other = node("other");
    join(&hub, &vic);
    join(&hub, &other);
    register(&vic.store, "v1");
    register(&other.store, "o1");
    fed::sync(&other.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let hub_fingerprint = hub.node.store.node().unwrap().fingerprint;
    let last_audit = |operation: &str| {
        let rows = audit_rows(&vic.store, operation);
        let row = rows.last().unwrap_or_else(|| panic!("no {operation} row"));
        assert_eq!(row.outcome, "denied");
        assert_eq!(
            row.agent.as_deref(),
            Some(format!("node:{}", hub.id()).as_str())
        );
        (rows.len(), row.detail.clone().unwrap())
    };

    // U1: the relay answers the push itself.
    relay.set(|req, hub| {
        if req.is_push() {
            Answer::json(
                200,
                &json!({ "accepted": 9, "duplicates": 0, "rejected": [] }),
            )
        } else {
            hub.send(&req)
        }
    });
    let before = sync_state(&vic, &hub.id());
    let (err, kind) = sync_error(&vic);
    assert_eq!(err, u1(relay.url(), "push"));
    assert_eq!(kind, Some("forbidden"));
    assert_eq!(sync_state(&vic, &hub.id()), before);
    assert_eq!(
        last_audit("sync POST /fed/push"),
        (1, "unsigned answer".to_string())
    );

    // U3: the relay removes the nonce.
    relay.set(|mut req, hub| {
        req.remove_header("x-another-nonce");
        hub.send(&req)
    });
    let (err, kind) = sync_error(&vic);
    assert_eq!(err, u3(relay.url(), "push"));
    assert_eq!(kind, Some("forbidden"));
    assert_eq!(sync_state(&vic, &hub.id()), before);
    assert_eq!(
        last_audit("sync POST /fed/push"),
        (
            2,
            "refused as carrying no nonce, although one was sent".to_string()
        )
    );

    // U2: the relay changes a pull answer and passes the hub's signature on.
    relay.set(|req, hub| {
        if req.is_pull() {
            hub.send(&req).edit(|v| v["more"] = json!(true))
        } else {
            hub.send(&req)
        }
    });
    let (err, kind) = sync_error(&vic);
    assert_eq!(err, u2(relay.url(), "pull", &hub_fingerprint));
    assert_eq!(kind, Some("forbidden"));
    let after = sync_state(&vic, &hub.id());
    assert_eq!(after.pull_cursor, before.pull_cursor);
    assert_eq!(after.anchors.received, before.anchors.received);
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.last_sync_at, before.last_sync_at);
    assert_eq!(events_from(&vic, &other.id()), 0);
    assert_eq!(
        last_audit("sync GET /fed/pull"),
        (
            1,
            "answer signature is not the hub's for this request".to_string()
        )
    );
}

#[test]
fn a_listed_key_that_differs_from_the_pinned_one_is_refused_with_both_fingerprints() {
    let hub = start_hub();
    let vic = node("vic");
    let other = node("other");
    join(&hub, &vic);
    join(&hub, &other);
    register(&other.store, "o1");
    fed::sync(&other.store).unwrap();
    fed::sync(&vic.store).unwrap();
    let real = other.store.node_entry().unwrap().public_key;
    let wrong = pin_a_wrong_key(&vic, &other.id());
    // Something new for the next page to carry.
    register(&other.store, "o2");
    fed::sync(&other.store).unwrap();
    let before = sync_state(&vic, &hub.id());
    let held = events_from(&vic, &other.id());

    let (err, kind) = sync_error(&vic);
    let (listed, pinned) = (crypto::fingerprint(&real), crypto::fingerprint(&wrong));
    assert_eq!(err, k(&other.id(), &listed, &pinned));
    assert_eq!(kind, Some("conflict"));
    let after = sync_state(&vic, &hub.id());
    assert_eq!(
        after.pull_cursor, before.pull_cursor,
        "the page is not applied"
    );
    assert_eq!(after.anchors, before.anchors);
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.last_sync_at, before.last_sync_at);
    assert_eq!(events_from(&vic, &other.id()), held);
    // The refusal rolled back with its transaction; the audit row is there all the same.
    let rows = audit_rows(&vic.store, "sync GET /fed/pull");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(
        rows[0].agent.as_deref(),
        Some(format!("node:{}", hub.id()).as_str())
    );
    assert_eq!(
        rows[0].detail.as_deref(),
        Some(
            format!(
                "node list names {} with key {listed}; pinned {pinned}",
                other.id()
            )
            .as_str()
        )
    );
}

#[test]
fn an_unsigned_error_body_is_shown_as_one_bounded_line() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|_, _| Answer {
        status: 500,
        headers: Vec::new(),
        body: "line one\nline two\r\n\u{1b}[2Jcleared "
            .repeat(1024 * 1024 / 32)
            .into_bytes(),
    });
    let before = sync_state(&vic, &hub.id());

    let (err, kind) = sync_error(&vic);
    assert_eq!(
        kind, None,
        "an unsigned error is not one of the core's kinds"
    );
    let quoted = err
        .strip_prefix("push failed (500 Internal Server Error): ")
        .unwrap_or_else(|| panic!("{err}"));
    assert_eq!(quoted.chars().count(), 600);
    assert!(quoted.starts_with("line one line two"), "{quoted}");
    assert!(quoted.ends_with('…'), "cut, and marked as cut");
    assert!(
        !quoted.contains(['\n', '\r', '\u{1b}']),
        "one line, nothing a terminal acts on"
    );
    assert_eq!(sync_state(&vic, &hub.id()), before);

    // The bound is 600 characters: a message of 600 arrives whole, one of 601 is cut.
    let quoted_message = |message: String| {
        relay.set(move |_, _| Answer::json(500, &json!({ "error": { "message": message } })));
        let (err, _) = sync_error(&vic);
        err.strip_prefix("push failed (500 Internal Server Error): ")
            .unwrap_or_else(|| panic!("{err}"))
            .to_string()
    };
    assert_eq!(quoted_message("x".repeat(600)), "x".repeat(600));
    assert_eq!(
        quoted_message("x".repeat(601)),
        format!("{}…", "x".repeat(599))
    );
    // A forged message of several lines with an escape sequence, as `error.message`.
    assert_eq!(
        quoted_message("first\nsecond\r\n\u{1b}]0;title\u{7} third".to_string()),
        "first second   ]0;title  third"
    );
    let long = quoted_message("a\nb\u{1b}".repeat(4096));
    assert_eq!(long.chars().count(), 600);
    assert!(!long.contains(['\n', '\u{1b}']));
}

#[test]
fn a_forged_oversize_pull_answer_only_fails_the_sync() {
    let hub = start_hub();
    let vic = node("vic");
    let late = node("late");
    join(&hub, &vic);
    join(&hub, &late);
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    fed::sync(&vic.store).unwrap();
    // Settle the push cursor over the records the sync above left, which stay on this node.
    fed::sync(&vic.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        Answer {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: vec![b' '; PULL_BODY_LIMIT_BYTES + 1],
        }
    });
    let before = sync_state(&vic, &hub.id());

    let (err, kind) = sync_error(&vic);
    assert!(
        err.contains("is by itself larger than this node reads in one answer"),
        "{err}"
    );
    assert_eq!(kind, None);
    let tries = relay.seen().iter().filter(|r| r.is_pull()).count();
    assert!((2..=10).contains(&tries), "{tries} requests");
    assert_eq!(sync_state(&vic, &hub.id()), before, "no state change");
}

// ---------------------------------------------------------------------------
// S7: what `last_sync_at` means.

#[test]
fn last_sync_at_is_written_only_by_a_completed_sync() {
    let hub = start_hub();
    let vic = node("vic");
    let late = node("late");
    join(&hub, &vic);
    join(&hub, &late);
    register(&vic.store, "v1");
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let synced_at = || sync_state(&vic, &hub.id()).last_sync_at;
    assert_eq!(synced_at(), None);

    // The push is accepted by the hub; the pull fails.
    relay.set(|req, hub| {
        if req.is_pull() {
            Answer::json(500, &json!({ "error": { "message": "down" } }))
        } else {
            hub.send(&req)
        }
    });
    assert!(fed::sync(&vic.store).is_err());
    assert!(
        events_from(&hub.node, &vic.id()) > 0,
        "the push went through"
    );
    assert!(sync_state(&vic, &hub.id()).push_cursor > 0);
    assert_eq!(
        synced_at(),
        None,
        "a sync that did not finish is not a sync"
    );

    // A complete sync sets it.
    relay.honest();
    fed::sync(&vic.store).unwrap();
    let completed = synced_at().expect("a completed sync is recorded");

    // New records that stay on this node, and a hub that fails: the push cursor moves over
    // them without a request, and the sync time stays.
    std::thread::sleep(Duration::from_millis(5));
    let cursor = sync_state(&vic, &hub.id()).push_cursor;
    relay.set(|_, _| Answer::json(500, &json!({ "error": { "message": "down" } })));
    relay.forget();
    assert!(fed::sync(&vic.store).is_err());
    assert!(
        sync_state(&vic, &hub.id()).push_cursor > cursor,
        "there were local records to pass"
    );
    assert!(relay.seen().iter().all(|r| r.is_pull()), "no push was sent");
    assert_eq!(synced_at(), Some(completed));
}

#[test]
fn the_hub_records_a_spokes_completed_push_and_pull() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let seen = || contact(&hub.node, &vic.id());
    let body = push_body(&vic);
    let pull = "/fed/pull?after=0&limit=500";
    assert_eq!(seen(), None, "never, before any sync");

    // 426, 400 and 401 record nothing.
    for path_body in [("/fed/push", Some(&body[..])), (pull, None)] {
        let (path, body) = path_body;
        assert_eq!(
            request(hub.url(), &vic, path, body, None, Signing::Good).status,
            426
        );
        assert_eq!(
            request(hub.url(), &vic, path, body, Some("xyz"), Signing::Good).status,
            400
        );
        assert_eq!(
            request(hub.url(), &vic, path, body, Some(NONCE), Signing::None).status,
            401
        );
    }
    assert_eq!(seen(), None);

    // A store call that fails records nothing: the hub's log refuses the insert.
    hub.node
        .db()
        .execute_batch(
            "CREATE TRIGGER refuse_events BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
    let failed = request(
        hub.url(),
        &vic,
        "/fed/push",
        Some(&body),
        Some(NONCE),
        Signing::Good,
    );
    assert_eq!(failed.status, 500, "{:?}", failed.error());
    assert_eq!(failed.signature, None, "an error answer is not signed");
    assert_eq!(seen(), None);
    hub.node
        .db()
        .execute_batch("DROP TRIGGER refuse_events")
        .unwrap();

    // A signed 200 to a push sets it.
    let pushed = request(
        hub.url(),
        &vic,
        "/fed/push",
        Some(&body),
        Some(NONCE),
        Signing::Good,
    );
    assert_eq!(pushed.status, 200);
    assert!(seen().is_some(), "a completed push is recorded");

    // And a signed 200 to a pull.
    hub.node
        .db()
        .execute("UPDATE peers SET last_sync_at = NULL", [])
        .unwrap();
    let pulled = request(hub.url(), &vic, pull, None, Some(NONCE), Signing::Good);
    assert_eq!(pulled.status, 200);
    assert!(seen().is_some(), "a completed pull is recorded");
}

#[test]
fn the_hub_answers_200_when_recording_the_contact_fails() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    hub.node
        .db()
        .execute_batch(
            "CREATE TRIGGER refuse_contact BEFORE UPDATE OF last_sync_at ON peers
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
    assert!(
        hub.node.store.record_peer_contact(&vic.id()).is_err(),
        "the write fails"
    );

    // The answer is still 200, signed and verifiable: the spoke's sync completes.
    let report = fed::sync(&vic.store).unwrap();
    assert!(report.pushed > 0, "{report:?}");
    assert!(
        events_from(&hub.node, &vic.id()) > 0,
        "the events are stored"
    );
    assert_eq!(contact(&hub.node, &vic.id()), None);
    assert!(sync_state(&vic, &hub.id()).last_sync_at.is_some());
}

#[test]
fn doctor_reports_a_hub_whose_pulls_keep_failing_as_stale() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    fed::sync(&vic.store).unwrap();
    assert_eq!(doctor_peers(vic.dir.path())["status"], "ok");
    // The last completed sync was 25 hours ago.
    vic.db()
        .execute(
            "UPDATE peers SET last_sync_at = last_sync_at - 25 * 3600 * 1000 - 60000",
            [],
        )
        .unwrap();
    // A fresh sync that pushes and whose pull fails.
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|req, hub| {
        if req.is_pull() {
            Answer::json(500, &json!({ "error": { "message": "down" } }))
        } else {
            hub.send(&req)
        }
    });
    let pushed = events_from(&hub.node, &vic.id());
    assert!(fed::sync(&vic.store).is_err());
    assert!(events_from(&hub.node, &vic.id()) > pushed, "it pushed");

    let check = doctor_peers(vic.dir.path());
    assert_eq!(check["status"], "warn", "{check}");
    assert_eq!(check["detail"], "hub hub: last sync 25 h ago", "{check}");
    assert_eq!(
        check["fix"],
        "another sync, and check the hub URL with another peers"
    );
}

#[test]
fn a_hubs_doctor_is_ok_after_a_sync_and_names_the_right_fix_for_a_stale_spoke() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    let check = doctor_peers(hub.node.dir.path());
    assert_eq!(check["detail"], "spoke vic: never synced", "{check}");
    let spoke_fix =
        "run another sync on that node; if it is gone for good, another pair revoke <node>";
    assert_eq!(check["fix"], spoke_fix);

    fed::sync(&vic.store).unwrap();
    let check = doctor_peers(hub.node.dir.path());
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(check["detail"], "1 paired, all synced recently");
    let (_, peers, _) = run(hub.node.dir.path(), &["peers"]);
    assert!(!peers.contains("last sync never"), "{peers}");

    // The spoke's last push or pull was 8 days ago.
    hub.node
        .db()
        .execute(
            "UPDATE peers SET last_sync_at = last_sync_at - 8 * 24 * 3600 * 1000 - 60000",
            [],
        )
        .unwrap();
    let check = doctor_peers(hub.node.dir.path());
    assert_eq!(check["status"], "warn", "{check}");
    assert_eq!(check["detail"], "spoke vic: last sync 192 h ago");
    assert_eq!(check["fix"], spoke_fix);
    assert!(!text(&check["fix"]).contains("another sync, and check the hub URL"));

    // A node that is a hub and a spoke, with both stale, gets both fixes.
    let top = start_hub_named("top");
    join(&top, &hub.node);
    let check = doctor_peers(hub.node.dir.path());
    assert_eq!(
        check["fix"],
        format!("another sync, and check the hub URL with another peers; {spoke_fix}").as_str(),
        "{check}"
    );
}

// ---------------------------------------------------------------------------
// I2, I3, I4: the invite.

/// A hub whose invites name the relay's address, so that the relay counts what a joiner
/// sends. The hub signs that address; nothing edits it.
fn hub_behind_a_relay() -> (Hub, Relay) {
    let hub = start_hub();
    let relay = Relay::start(hub.url());
    (hub, relay)
}

fn invite_at(hub: &Hub, url: &str) -> (String, Value) {
    let code = hub
        .node
        .store
        .create_invite(url, &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    (code.clone(), decode_invite(&code))
}

/// The join must fail with exactly `text` and `kind`, having sent and written nothing.
fn join_refused(spoke: &Node, relay: &Relay, invite: &Value, text: &str, kind: &str, why: &str) {
    match fed::join(&spoke.store, &encode_invite(invite)) {
        Ok(peer) => panic!("{why}: joined {peer:?}"),
        Err(e) => {
            assert_eq!(e.to_string(), text, "{why}");
            assert_eq!(
                e.downcast_ref::<another_core::Error>().map(|e| e.code()),
                Some(kind),
                "{why}"
            );
        }
    }
    assert!(relay.seen().is_empty(), "{why}: nothing was sent");
    assert!(spoke.store.peers().unwrap().is_empty(), "{why}");
}

#[test]
fn an_unsigned_invite_is_refused_before_sending() {
    let (hub, relay) = hub_behind_a_relay();
    let vic = node("vic");
    let (code, invite) = invite_at(&hub, relay.url());

    let mut unsigned = invite.clone();
    unsigned.as_object_mut().unwrap().remove("sig");
    join_refused(&vic, &relay, &unsigned, J1, "invalid", "sig removed");
    let mut empty = invite.clone();
    empty["sig"] = json!("");
    join_refused(&vic, &relay, &empty, J1, "invalid", "sig emptied");

    fed::join(&vic.store, &code).unwrap();
    assert_eq!(relay.seen().len(), 1, "the genuine code is still unused");
}

#[test]
fn an_invite_is_refused_when_any_signed_field_is_changed() {
    let (hub, relay) = hub_behind_a_relay();
    let vic = node("vic");
    let (code, invite) = invite_at(&hub, relay.url());
    let other_key = crypto::public_of(&crypto::generate_key().unwrap());
    let now = hub.node.store.now();
    type Change = (&'static str, Box<dyn Fn(&mut Value)>);
    let changes: Vec<Change> = vec![
        ("v", Box::new(|v| v["v"] = json!(3))),
        ("v to 1", Box::new(|v| v["v"] = json!(1))),
        (
            "hub name",
            Box::new(|v| v["hub"]["display_name"] = json!("hub2")),
        ),
        (
            "address",
            Box::new(|v| v["hub_url"] = json!("http://127.0.0.1:9")),
        ),
        (
            "address with a path",
            Box::new(|v| v["hub_url"] = json!(format!("{}/x", text(&v["hub_url"])))),
        ),
        (
            "invite id",
            Box::new(|v| v["invite_id"] = json!("01ARZ3NDEKTSV4RRFFQ69G5FAV")),
        ),
        (
            "secret",
            Box::new(|v| v["secret"] = json!("another-secret")),
        ),
        (
            "a permission removed",
            Box::new(|v| v["perms"] = json!(["read_feed"])),
        ),
        (
            "a permission added",
            Box::new(|v| v["perms"] = json!(["read_feed", "send_mail", "borrow_resource"])),
        ),
        (
            "the permissions in another order",
            Box::new(|v| v["perms"] = json!(["send_mail", "read_feed"])),
        ),
        (
            "a later expiry",
            Box::new(|v| v["expires_at"] = json!(v["expires_at"].as_i64().unwrap() + 1)),
        ),
        (
            "an expiry in the past",
            Box::new(move |v| v["expires_at"] = json!(now - 1000)),
        ),
        (
            "hub id",
            Box::new(|v| v["hub"]["node_id"] = json!("01ARZ3NDEKTSV4RRFFQ69G5FAV")),
        ),
        (
            "hub key",
            Box::new(move |v| v["hub"]["public_key"] = json!(other_key.clone())),
        ),
        (
            "hub key that does not decode",
            Box::new(|v| v["hub"]["public_key"] = json!("not a key")),
        ),
        (
            "the signature itself",
            Box::new(|v| v["sig"] = json!(crypto::b64(&[7u8; 64]))),
        ),
    ];
    for (why, change) in &changes {
        let mut edited = invite.clone();
        change(&mut edited);
        assert_ne!(edited, invite, "{why}");
        join_refused(&vic, &relay, &edited, J2, "forbidden", why);
    }
    // A line break in a signed text field is an invalid code, whatever the signature.
    let mut broken = invite.clone();
    broken["hub"]["display_name"] = json!("hub\nx");
    join_refused(
        &vic,
        &relay,
        &broken,
        "invalid invite code",
        "invalid",
        "line break",
    );
    let used: i64 = hub
        .node
        .db()
        .query_row(
            "SELECT count(*) FROM invites WHERE used_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(used, 0, "the invite is not used up");
    fed::join(&vic.store, &code).unwrap();
}

#[test]
fn an_edit_that_no_longer_decodes_is_an_invalid_code() {
    let (hub, relay) = hub_behind_a_relay();
    let vic = node("vic");
    let (_, invite) = invite_at(&hub, relay.url());
    type Change = (&'static str, Box<dyn Fn(&mut Value)>);
    let changes: Vec<Change> = vec![
        (
            "an unknown permission name",
            Box::new(|v| v["perms"] = json!(["read_feed", "reap_remote"])),
        ),
        ("v as text", Box::new(|v| v["v"] = json!("2"))),
        (
            "a missing field",
            Box::new(|v| {
                v.as_object_mut().unwrap().remove("expires_at");
            }),
        ),
        (
            "no v",
            Box::new(|v| {
                v.as_object_mut().unwrap().remove("v");
            }),
        ),
    ];
    for (why, change) in &changes {
        let mut edited = invite.clone();
        change(&mut edited);
        join_refused(&vic, &relay, &edited, "invalid invite code", "invalid", why);
    }
    // Broken base64.
    match fed::join(&vic.store, "another-invite:***") {
        Err(e) => assert_eq!(e.to_string(), "invalid invite code"),
        Ok(peer) => panic!("{peer:?}"),
    }
    assert!(relay.seen().is_empty());

    // A field this build does not have is not signed and never read: the invite verifies.
    let mut extended = invite.clone();
    extended["note"] = json!("added on the way");
    fed::join(&vic.store, &encode_invite(&extended)).unwrap();
    assert_eq!(relay.seen().len(), 1);
}

#[test]
fn a_signed_invite_of_an_unknown_version_is_refused() {
    let (hub, relay) = hub_behind_a_relay();
    let vic = node("vic");
    let (_, mut invite) = invite_at(&hub, relay.url());
    invite["v"] = json!(3);
    resign(&mut invite, &secret_of(&hub.node));
    join_refused(
        &vic,
        &relay,
        &invite,
        J3,
        "invalid",
        "v 3, signed by the hub",
    );
}

/// The limit this fix states: an invite whose hub key was replaced, and which was signed
/// again with that key, is a valid invite to another hub. Only the fingerprint shows it.
#[test]
fn a_self_consistent_invite_from_another_key_joins_that_key_only() {
    let hub = start_hub();
    let vic = node("vic");
    // The attacker's own hub, under the same name.
    let attacker = start_hub_named("hub");
    let (_, genuine) = invite_at(&hub, hub.url());
    let (_, theirs) = invite_at(&attacker, &attacker.served.url);
    let mut forged = genuine.clone();
    forged["hub_url"] = theirs["hub_url"].clone();
    forged["hub"]["node_id"] = theirs["hub"]["node_id"].clone();
    forged["hub"]["public_key"] = theirs["hub"]["public_key"].clone();
    forged["invite_id"] = theirs["invite_id"].clone();
    forged["secret"] = theirs["secret"].clone();
    resign(&mut forged, &secret_of(&attacker.node));

    let peer = fed::join(&vic.store, &encode_invite(&forged)).unwrap();
    let real = hub.node.store.node().unwrap().fingerprint;
    let wrong = attacker.node.store.node().unwrap().fingerprint;
    assert_ne!(real, wrong);
    assert_eq!(
        peer.fingerprint, wrong,
        "the fingerprint the join prints is the attacker's"
    );
    assert_eq!(peer.node_id, attacker.id());
    assert!(
        audit_rows(&hub.node.store, "POST /fed/pair").is_empty(),
        "the real hub saw nothing"
    );
    // A join exchanges nothing but the pairing request: no event has moved.
    assert_eq!(events_from(&attacker.node, &vic.id()), 0);
}

// ---------------------------------------------------------------------------
// D1 and W1: the warnings.

#[test]
fn doctor_warns_for_a_hub_joined_before_invites_were_signed() {
    let hub = start_hub();
    let old = node("old");
    let code = hub
        .node
        .store
        .create_invite_before_signed_invites(hub.url(), &Perm::ALL, 3600)
        .unwrap();
    older_join(&old, &code);
    fed::sync(&old.store).unwrap();

    let check = doctor_peers(old.dir.path());
    assert_eq!(check["status"], "warn", "{check}");
    assert_eq!(
        check["detail"],
        format!(
            "1 paired, all synced recently; hub hub was joined before invites were signed, so \
             its address {} was never checked; compare it with the address the hub's owner \
             gives, or join again with a fresh invite made with the same --perms",
            hub.url()
        )
        .as_str()
    );
    assert_eq!(check["fix"], Value::Null);

    // A new join with a fresh invite clears it.
    join(&hub, &old);
    let check = doctor_peers(old.dir.path());
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(check["detail"], "1 paired, all synced recently");
}

#[test]
fn sync_and_peers_show_a_hub_joined_before_invites_were_signed() {
    let hub = start_hub();
    let old = node("old");
    let code = hub
        .node
        .store
        .create_invite_before_signed_invites(hub.url(), &[Perm::ReadFeed], 3600)
        .unwrap();
    older_join(&old, &code);
    register(&old.store, "o1");
    let home = old.dir.path();
    let note = format!(
        "note: hub hub was joined before invites were signed, so its address {} was never \
         checked; see another doctor\n",
        hub.url()
    );
    let mark = "  (joined before invites were signed; address not checked)";

    let (code, out, err) = run(home, &["sync"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.starts_with("synced with "), "{out}");
    assert_eq!(err, note, "one line on standard error");
    let (code, out, err) = run(home, &["--json", "sync"]);
    assert_eq!(code, Some(0));
    let report: Value = serde_json::from_str(&out).unwrap();
    assert!(report.get("warnings").is_none() && report.get("note").is_none());
    assert_eq!(report["hub"], hub.url());
    assert_eq!(err, note);

    let (_, peers, _) = run(home, &["peers"]);
    let row = peers.lines().find(|l| l.starts_with("hub ")).unwrap();
    assert!(row.ends_with(mark), "{row}");
    assert!(row.contains("perms [read_feed]"), "{row}");
    let (_, peers_json, _) = run(home, &["--json", "peers"]);
    assert!(!peers_json.contains("signed"), "{peers_json}");
    let cursors = old.store.push_cursor(&hub.id()).unwrap();
    assert!(cursors.0 > 0 && cursors.1 > 0, "{cursors:?}");

    // A new join, with other permissions: the note and the mark go, the permissions change
    // on both sides, and the cursors stay.
    let fresh = hub
        .node
        .store
        .create_invite(hub.url(), &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    let (code, _, err) = run(home, &["pair", "join", &fresh]);
    assert_eq!(code, Some(0), "{err}");
    let (code, _, err) = run(home, &["sync"]);
    assert_eq!(code, Some(0));
    assert_eq!(err, "", "no note after a join on a signed invite");
    let (_, peers, _) = run(home, &["peers"]);
    let row = peers.lines().find(|l| l.starts_with("hub ")).unwrap();
    assert!(!row.contains("joined before"), "{row}");
    assert!(row.contains("perms [read_feed, send_mail]"), "{row}");
    let (_, hub_peers, _) = run(hub.node.dir.path(), &["peers"]);
    let row = hub_peers.lines().find(|l| l.starts_with("spoke ")).unwrap();
    assert!(row.contains("perms [read_feed, send_mail]"), "{row}");
    let after = old.store.push_cursor(&hub.id()).unwrap();
    assert!(after.0 >= cursors.0 && after.1 >= cursors.1, "{after:?}");
}

/// An HTTP proxy on loopback that tunnels every `CONNECT` to the hub at `hub_url`, whatever
/// host the request names. Returns the proxy's address and the hosts it was asked for.
fn tunnel_to(hub_url: &str) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};

    let hub = hub_url.strip_prefix("http://").unwrap().to_string();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let seen = asked.clone();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { continue };
            let (hub, seen) = (hub.clone(), seen.clone());
            std::thread::spawn(move || {
                // The request head, one byte at a time, so nothing after it is taken.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match client.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let mut words = head.split_whitespace();
                if words.next() != Some("CONNECT") {
                    let _ = client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n");
                    return;
                }
                seen.lock()
                    .unwrap()
                    .push(words.next().unwrap_or_default().to_string());
                let Ok(mut upstream) = TcpStream::connect(&hub) else {
                    return;
                };
                if client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .is_err()
                {
                    return;
                }
                let (Ok(mut from_client), Ok(mut to_hub)) =
                    (client.try_clone(), upstream.try_clone())
                else {
                    return;
                };
                let up = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from_client, &mut to_hub);
                    let _ = to_hub.shutdown(Shutdown::Write);
                });
                let _ = std::io::copy(&mut upstream, &mut client);
                let _ = client.shutdown(Shutdown::Write);
                let _ = up.join();
            });
        }
    });
    (url, asked)
}

#[test]
fn plain_http_off_loopback_warns_at_invite_join_and_doctor() {
    let warns = ["http://10.0.0.5:7466", "http://localhost.example"];
    let quiet = [
        "http://127.0.0.1:1",
        "http://localhost:1",
        "http://[::1]:1",
        "https://hub.example",
    ];
    for url in warns {
        assert!(fed::plain_http_off_loopback(url), "{url}");
    }
    for url in quiet {
        assert!(!fed::plain_http_off_loopback(url), "{url}");
    }

    // `pair invite`.
    let hub = start_hub();
    let home = hub.node.dir.path();
    for url in warns {
        let warning = format!(
            "warning: {url} is plain http://. Anyone on the network path between a spoke and \
             this hub can read mail and the feed in transit. Give spokes an https:// address \
             unless the link is a private network"
        );
        let (code, out, err) = run(home, &["pair", "invite", "--url", url]);
        assert_eq!(code, Some(0), "{err}");
        assert!(out.contains("another-invite:"), "{out}");
        assert_eq!(err, format!("{warning}\n"));
        let (code, out, err) = run(home, &["--json", "pair", "invite", "--url", url]);
        assert_eq!(code, Some(0));
        assert_eq!(err, "");
        let invite: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(invite["warnings"], json!([warning]));
    }
    for url in quiet {
        let (code, _, err) = run(home, &["pair", "invite", "--url", url]);
        assert_eq!((code, err.as_str()), (Some(0), ""), "{url}");
        let (_, out, _) = run(home, &["--json", "pair", "invite", "--url", url]);
        let invite: Value = serde_json::from_str(&out).unwrap();
        assert!(invite.get("warnings").is_none(), "{url}");
    }

    // `pair join` and `another doctor`. The hub's owner gave an address that is not
    // loopback (one from the range reserved for documentation, which nothing routes); the
    // joiner reaches it through an HTTP proxy, which here is a tunnel on loopback that ends
    // at the hub, so nothing leaves this machine.
    let (proxy_url, tunnelled) = tunnel_to(hub.url());
    let join_warning = "warning: this hub is reached over plain http://. Anyone on the network \
                        path can read this node's mail and feed in transit; they cannot change \
                        them, because the hub's answers are signed. Ask for an https:// address \
                        unless the link is a private network\n";
    let address = "http://192.0.2.1:7466";
    assert!(fed::plain_http_off_loopback(address));
    let proxy = [("http_proxy", proxy_url.as_str())];
    for json in [false, true] {
        let spoke = node(if json { "spoke-json" } else { "spoke" });
        let code = hub
            .node
            .store
            .create_invite(address, &Perm::ALL, 3600)
            .unwrap();
        let mut args = vec!["pair", "join", code.as_str()];
        if json {
            args.insert(0, "--json");
        }
        let (status, out, err) = run_with(spoke.dir.path(), &args, &proxy);
        assert_eq!(status, Some(0), "{err}");
        if json {
            assert_eq!(err, "");
            let peer: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(peer["url"], address);
            assert_eq!(peer["warnings"], json!([join_warning.trim_end()]));
        } else {
            assert!(out.starts_with("paired with hub hub at http://192.0.2.1:7466"));
            assert_eq!(err, join_warning);
        }
        let check = doctor_peers(spoke.dir.path());
        assert_eq!(check["status"], "warn", "{check}");
        assert_eq!(
            check["detail"],
            "hub hub: never synced; hub hub is reached over plain http:// at \
             http://192.0.2.1:7466: anyone on the network path can read mail and the feed in \
             transit"
        );
    }
    assert_eq!(
        *tunnelled.lock().unwrap(),
        ["192.0.2.1:7466", "192.0.2.1:7466"],
        "both joins went through the tunnel"
    );
    // The loopback test is on the parsed host: a name that only starts like one warns.
    let far = node("far");
    join(&hub, &far);
    assert_eq!(fed::join_warning(&far.store.peers().unwrap()[0]), None);
    point_at(&far, "http://localhost.example:7466");
    assert_eq!(
        fed::join_warning(&far.store.peers().unwrap()[0]),
        Some(join_warning.trim_end())
    );
    let check = doctor_peers(far.dir.path());
    assert_eq!(
        check["detail"],
        "hub hub: never synced; hub hub is reached over plain http:// at \
         http://localhost.example:7466: anyone on the network path can read mail and the \
         feed in transit",
        "{check}"
    );
    // A hub on loopback: no warning at join or in doctor.
    let near = node("near");
    let code = hub
        .node
        .store
        .create_invite(hub.url(), &Perm::ALL, 3600)
        .unwrap();
    let (status, _, err) = run(near.dir.path(), &["pair", "join", &code]);
    assert_eq!((status, err.as_str()), (Some(0), ""));
    let (status, _, err) = run(near.dir.path(), &["sync"]);
    assert_eq!((status, err.as_str()), (Some(0), ""));
    assert_eq!(doctor_peers(near.dir.path())["status"], "ok");
}

// ---------------------------------------------------------------------------
// 7: every new refusal, with the real binary.

#[test]
fn each_new_refusal_has_its_kind_and_exit_code() {
    let (hub, relay) = hub_behind_a_relay();
    let failure = |home: &Path, args: &[&str]| {
        let mut json_args = vec!["--json"];
        json_args.extend(args);
        let (code, out, _) = run(home, &json_args);
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
        let (_, _, human) = run(home, args);
        (
            code,
            text(&v["error"]["code"]).to_string(),
            text(&v["error"]["message"]).to_string(),
            human,
        )
    };
    let expect = |got: (Option<i32>, String, String, String), exit: i32, kind: &str, text: &str| {
        assert_eq!(got.0, Some(exit), "{text}");
        assert_eq!(got.1, kind, "{text}");
        assert_eq!(got.2, text);
        assert_eq!(
            got.3,
            format!("another: {text}\n"),
            "the same text, in full"
        );
    };

    // The invite: J1, J2, J3, and the address a hub refuses to sign.
    let joiner = node("joiner");
    let (_, invite) = invite_at(&hub, relay.url());
    let mut unsigned = invite.clone();
    unsigned.as_object_mut().unwrap().remove("sig");
    let code = encode_invite(&unsigned);
    expect(
        failure(joiner.dir.path(), &["pair", "join", &code]),
        1,
        "invalid",
        J1,
    );
    let mut edited = invite.clone();
    edited["hub_url"] = json!("http://127.0.0.1:9");
    let code = encode_invite(&edited);
    expect(
        failure(joiner.dir.path(), &["pair", "join", &code]),
        1,
        "forbidden",
        J2,
    );
    let mut newer = invite.clone();
    newer["v"] = json!(3);
    resign(&mut newer, &secret_of(&hub.node));
    let code = encode_invite(&newer);
    expect(
        failure(joiner.dir.path(), &["pair", "join", &code]),
        1,
        "invalid",
        J3,
    );
    expect(
        failure(
            hub.node.dir.path(),
            &["pair", "invite", "--url", "ftp://hub.example"],
        ),
        1,
        "invalid",
        "hub url must be an http:// or https:// URL with a host",
    );
    assert!(relay.seen().is_empty(), "no refused join sent anything");

    // Sync: U1, U2, U3 through the relay, and K on a home with a wrong pinned key.
    let vic = node("vic");
    let other = node("other");
    join(&hub, &vic);
    join(&hub, &other);
    register(&vic.store, "v1");
    fed::sync(&other.store).unwrap();
    point_at(&vic, relay.url());
    let fingerprint = hub.node.store.node().unwrap().fingerprint;
    relay.set(|req, hub| {
        if req.is_push() {
            Answer::json(
                200,
                &json!({ "accepted": 1, "duplicates": 0, "rejected": [] }),
            )
        } else {
            hub.send(&req)
        }
    });
    expect(
        failure(vic.dir.path(), &["sync"]),
        1,
        "forbidden",
        &u1(relay.url(), "push"),
    );
    relay.set(|req, hub| hub.send(&req).edit(|v| v["x"] = json!(1)));
    expect(
        failure(vic.dir.path(), &["sync"]),
        1,
        "forbidden",
        &u2(relay.url(), "push", &fingerprint),
    );
    relay.set(|mut req, hub| {
        req.remove_header("x-another-nonce");
        hub.send(&req)
    });
    expect(
        failure(vic.dir.path(), &["sync"]),
        1,
        "forbidden",
        &u3(relay.url(), "push"),
    );
    relay.honest();
    fed::sync(&vic.store).unwrap();
    let real = other.store.node_entry().unwrap().public_key;
    let wrong = pin_a_wrong_key(&vic, &other.id());
    expect(
        failure(vic.dir.path(), &["sync"]),
        3,
        "conflict",
        &k(
            &other.id(),
            &crypto::fingerprint(&real),
            &crypto::fingerprint(&wrong),
        ),
    );

    // N1, as a spoke of the build before signed answers gets and prints it: a failure
    // that is none of the core's kinds, which the command line reports as `error`, exit 1.
    let old = node("old");
    join(&hub, &old);
    register(&old.store, "o1");
    let err = older_sync(&old).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!("push failed (426 Upgrade Required): {N1}")
    );
    assert!(err.downcast_ref::<another_core::Error>().is_none());
}

// ---------------------------------------------------------------------------
// 8.3: a node of this build against one of the build before.

#[test]
fn a_new_spoke_refuses_an_older_hubs_unsigned_answers() {
    // One home for the hub, served first by the older build and then by this one, behind
    // one address.
    let hub_node = node("hub");
    let older = serve(older_router(&hub_node));
    let newer = serve(fed::router(hub_node.store.clone()));
    let relay = Relay::start(&older.url);
    let vic = node("vic");
    let code = hub_node
        .store
        .create_invite(relay.url(), &Perm::ALL, 3600)
        .unwrap();
    fed::join(&vic.store, &code).unwrap();
    register(&vic.store, "v1");
    let hub_id = hub_node.id();
    let before = sync_state(&vic, &hub_id);
    let rows = (pairing_rows(&hub_node), pairing_rows(&vic));

    let (err, kind) = sync_error(&vic);
    assert_eq!(err, u1(relay.url(), "push"));
    assert_eq!(kind, Some("forbidden"));
    assert_eq!(sync_state(&vic, &hub_id), before, "cursors unchanged");
    let stored = events_from(&hub_node, &vic.id());
    assert!(
        stored > 0,
        "the older hub stored the batch before answering"
    );

    // The hub is updated. Nobody pairs again.
    relay.retarget(&newer.url);
    let report = fed::sync(&vic.store).unwrap();
    assert_eq!(report.pushed, 0, "{report:?}");
    assert_eq!(
        report.push_duplicates as i64, stored,
        "what the older hub had stored counts as already there"
    );
    let hub_rows = pairing_rows(&hub_node);
    let spoke_rows = pairing_rows(&vic);
    let without_sync_time = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.split(' ')
                    .filter(|f| !f.starts_with("last_sync_at=") && !f.contains("_cursor="))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    assert_eq!(without_sync_time(&hub_rows), without_sync_time(&rows.0));
    assert_eq!(without_sync_time(&spoke_rows), without_sync_time(&rows.1));
}

#[test]
fn a_new_hub_refuses_an_older_spoke_and_loses_nothing() {
    let hub = start_hub();
    let old = node("old");
    join(&hub, &old);
    register(&old.store, "o1");
    let before = sync_state(&old, &hub.id());

    let err = error_text(older_sync(&old));
    assert_eq!(err, format!("push failed (426 Upgrade Required): {N1}"));
    assert_eq!(events_from(&hub.node, &old.id()), 0, "nothing stored");
    assert_eq!(sync_state(&old, &hub.id()), before, "cursors unchanged");
    let denied = audit_rows(&hub.node.store, "POST /fed/push");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].detail.as_deref(), Some(NO_NONCE_AUDIT));

    // A spoke with nothing to push is refused at its pull.
    let idle = node("idle");
    join(&hub, &idle);
    fed::sync(&idle.store).unwrap();
    fed::sync(&idle.store).unwrap();
    let err = error_text(older_sync(&idle));
    assert_eq!(err, format!("pull failed (426 Upgrade Required): {N1}"));

    // The spoke is updated, on the same home, without joining again.
    let report = fed::sync(&old.store).unwrap();
    assert!(report.pushed > 0, "{report:?}");
    assert!(events_from(&hub.node, &old.id()) > 0);
}

#[test]
fn pairing_is_untouched_by_either_refusal() {
    // A hub of this build and a spoke of the older one.
    let hub = start_hub();
    let old = node("old");
    join(&hub, &old);
    register(&old.store, "o1");
    let rows = (pairing_rows(&hub.node), pairing_rows(&old));
    assert!(older_sync(&old).is_err());
    assert_eq!(pairing_rows(&hub.node), rows.0);
    assert_eq!(pairing_rows(&old), rows.1);

    // A hub of the older build and a spoke of this one.
    let older = start_older_hub("elder");
    let vic = node("vic");
    join(&older, &vic);
    register(&vic.store, "v1");
    let rows = (pairing_rows(&older.node), pairing_rows(&vic));
    assert!(fed::sync(&vic.store).is_err());
    assert_eq!(pairing_rows(&older.node), rows.0);
    assert_eq!(pairing_rows(&vic), rows.1);
}

#[test]
fn an_older_joiner_accepts_a_signed_invite_and_is_then_refused_at_sync() {
    let hub = start_hub();
    let old = node("old");
    let code = hub
        .node
        .store
        .create_invite(hub.url(), &Perm::ALL, 3600)
        .unwrap();
    assert!(decode_invite(&code)["sig"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    older_join(&old, &code);
    assert_eq!(old.store.hub().unwrap().unwrap().0, hub.id());
    register(&old.store, "o1");
    let err = error_text(older_sync(&old));
    assert_eq!(err, format!("push failed (426 Upgrade Required): {N1}"));
}

#[test]
fn a_new_joiner_refuses_an_older_hubs_invite() {
    let older = start_older_hub("elder");
    let vic = node("vic");
    let code = older
        .node
        .store
        .create_invite_before_signed_invites(older.url(), &Perm::ALL, 3600)
        .unwrap();
    assert!(decode_invite(&code).get("sig").is_some_and(|s| s == ""));
    let err = fed::join(&vic.store, &code).unwrap_err();
    assert_eq!(err.to_string(), J1);
    assert!(vic.store.peers().unwrap().is_empty());
    assert!(
        audit_rows(&older.node.store, "POST /fed/pair").is_empty(),
        "nothing was sent"
    );
}

/// A node in the middle of a chain: a spoke of `top`, and a hub for `leaf`.
struct Chain {
    top: Hub,
    mid: Hub,
    leaf: Node,
}

fn chain(top_is_older: bool, mid_is_older: bool) -> Chain {
    let top = if top_is_older {
        start_older_hub("top")
    } else {
        start_hub_named("top")
    };
    let mid = if mid_is_older {
        start_older_hub("mid")
    } else {
        start_hub_named("mid")
    };
    let leaf = node("leaf");
    join(&top, &mid.node);
    join(&mid, &leaf);
    register(&top.node.store, "t1");
    register(&mid.node.store, "m1");
    register(&leaf.store, "f1");
    Chain { top, mid, leaf }
}

#[test]
fn a_mixed_chain_stops_only_at_the_mixed_link() {
    // Top older, middle and leaf on this build: the middle refuses the top's answers and
    // keeps serving its leaf, signed.
    let c = chain(true, false);
    let (err, _) = sync_error(&c.mid.node);
    assert_eq!(err, u1(c.top.url(), "push"));
    let report = fed::sync(&c.leaf.store).unwrap();
    assert!(report.pushed > 0 && report.applied > 0, "{report:?}");
    assert!(events_from(&c.leaf, &c.mid.id()) > 0);
    assert_eq!(
        events_from(&c.leaf, &c.top.id()),
        0,
        "nothing new from beyond the middle"
    );

    // Top on this build, middle and leaf older: the top refuses the middle; the middle and
    // its leaf sync as before.
    let c = chain(false, true);
    let err = error_text(older_sync(&c.mid.node));
    assert_eq!(err, format!("push failed (426 Upgrade Required): {N1}"));
    assert_eq!(events_from(&c.top.node, &c.mid.id()), 0);
    let report = older_sync(&c.leaf).unwrap();
    assert!(report.pushed > 0 && report.applied > 0, "{report:?}");

    // Top and leaf on this build, the middle older: both links stop.
    let c = chain(false, true);
    let err = error_text(older_sync(&c.mid.node));
    assert_eq!(err, format!("push failed (426 Upgrade Required): {N1}"));
    let (err, _) = sync_error(&c.leaf);
    assert_eq!(err, u1(c.mid.url(), "push"));
}
