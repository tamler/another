//! Event size (Q26, D99): one event is at most `MAX_EVENT_WIRE_BYTES` on the wire, the
//! fields a local caller could grow without bound each have a rule, and a push batch and a
//! pull page are bounded by bytes as well as by count. Two nodes, each with its own
//! database in a temp directory, paired without HTTP.

use std::path::PathBuf;

use another_core::*;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

struct Node {
    dir: tempfile::TempDir,
    store: Store,
}

impl Node {
    fn db(&self) -> PathBuf {
        self.dir.path().join(DB_FILE)
    }

    fn project(&self, name: &str) -> String {
        let dir = self.dir.path().join("projects").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    Node { dir, store }
}

fn register_with(
    store: &Store,
    name: &str,
    model: Option<String>,
    description: Option<String>,
) -> Result<Agent> {
    store.register_agent(RegisterAgent {
        name: name.into(),
        program: Program::Claude,
        project: None,
        model,
        description,
    })
}

fn register(store: &Store, name: &str) {
    register_with(store, name, None, None).unwrap();
}

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

/// Everything `spoke` may send, in one batch bounded by count only.
fn outbound(spoke: &Node) -> Vec<WireEvent> {
    spoke
        .store
        .outbound_events_within(0, 10_000, usize::MAX)
        .unwrap()
        .0
}

/// Push and pull once through the store, as `another sync` does over HTTP.
fn sync(hub: &Node, spoke: &Node) {
    let events = outbound(spoke);
    let r = hub
        .store
        .ingest(spoke.store.node_id(), &events, &[])
        .unwrap();
    assert!(r.rejected.is_empty(), "{:?}", r.rejected);
    let page = hub
        .store
        .pull_page(
            spoke.store.node_id(),
            0,
            &PullAnchors::default(),
            2000,
            usize::MAX,
        )
        .unwrap();
    let r = spoke
        .store
        .ingest(hub.store.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert!(r.rejected.is_empty(), "{:?}", r.rejected);
}

fn message(from: &str, to: Vec<String>, subject: String, body: String) -> SendMessage {
    SendMessage {
        from: from.into(),
        to,
        subject,
        body,
        ..Default::default()
    }
}

fn invalid(result: Result<impl std::fmt::Debug>) -> String {
    match result {
        Err(Error::Invalid(text)) => text,
        other => panic!("expected an invalid error, got {other:?}"),
    }
}

const BODY: usize = 256 * 1024;

/// The size compared with the cap is the size of the JSON that sync sends, less the extra
/// digits of the sequence number.
#[test]
fn an_events_size_is_the_size_of_its_wire_json() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&a.store, "alice");
    let events = outbound(&a);
    assert!(!events.is_empty());
    for mut e in events {
        assert_eq!(e.wire_bytes(), serde_json::to_vec(&e).unwrap().len());
        let at_one_digit = e.capped_bytes();
        e.seq = 1_234_567;
        assert_eq!(e.wire_bytes(), at_one_digit + 6);
        assert_eq!(e.capped_bytes(), at_one_digit);
    }
}

/// The largest mail an honest node can write to one recipient: a body of 256 KiB of the
/// characters that cost most on the wire (a control character is seven bytes there), a
/// subject of 300 four-byte characters, and names at their longest. It is under the cap,
/// and it travels in a batch and a page of its own.
#[test]
fn the_largest_honest_mail_to_one_recipient_is_under_the_cap() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    let long = |c: &str| c.repeat(64);
    register(&hub.store, &long("b"));
    register(&a.store, &long("a"));
    sync(&hub, &a);
    a.store
        .send_message(message(
            &long("a"),
            vec![format!("{}@@hub", long("b"))],
            "\u{1F600}".repeat(MAX_SUBJECT_CHARS),
            "\u{1}".repeat(BODY),
        ))
        .unwrap();
    let mail: Vec<WireEvent> = outbound(&a)
        .into_iter()
        .filter(|e| e.kind == "message.sent")
        .collect();
    assert_eq!(mail.len(), 1);
    let bytes = mail[0].capped_bytes();
    eprintln!("largest honest mail to one recipient: {bytes} bytes on the wire");
    assert!(bytes > 7 * BODY, "{bytes}");
    assert!(bytes < 7 * BODY + 8 * 1024, "{bytes}");
    assert!(bytes < MAX_EVENT_WIRE_BYTES);
}

/// The backstop: mail has no rule on how many agents one message may name, so a message
/// with the costliest body to enough recipients would pass the cap. It is refused before
/// anything is written, and the refusal names the kind of event and both sizes.
#[test]
fn a_local_event_over_the_cap_is_refused_and_nothing_is_written() {
    let store = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    register(&store, "alice");
    let names: Vec<String> = (0..2500).map(|i| format!("{i:0>64}")).collect();
    for name in &names {
        register(&store, name);
    }
    let body = "\u{1}".repeat(BODY);
    // A hundred recipients fit.
    store
        .send_message(message(
            "alice",
            names[..100].to_vec(),
            "fits".into(),
            body.clone(),
        ))
        .unwrap();
    let before = store.events(100_000).unwrap().len();
    let err = invalid(store.send_message(message("alice", names.clone(), "too many".into(), body)));
    assert!(
        err.starts_with("this message.sent event would be ")
            && err
                .ends_with(" bytes when sent to another node; the limit for one event is 2097152"),
        "{err}"
    );
    let bytes: usize = err
        .split_whitespace()
        .nth(5)
        .and_then(|n| n.parse().ok())
        .unwrap();
    eprintln!("the costliest body to 2500 recipients: {bytes} bytes on the wire");
    assert!(bytes > MAX_EVENT_WIRE_BYTES, "{err}");
    assert_eq!(store.events(100_000).unwrap().len(), before);
    let inbox = store
        .inbox(
            &names[0],
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 10,
            },
        )
        .unwrap();
    let subjects: Vec<&str> = inbox
        .messages
        .iter()
        .map(|m| m.message.subject.as_str())
        .collect();
    assert_eq!(subjects, ["fits"]);
}

/// Each field a local caller could grow without bound is accepted at its limit and refused
/// one byte over it, with the field, its size and the limit, before anything is written.
#[test]
fn each_capped_field_is_accepted_at_its_limit_and_refused_one_byte_over() {
    let n = node("solo");
    let s = &n.store;
    register(s, "alice");
    register(s, "bob");
    let project = n.project("shop");
    s.ensure_project("alice", &project).unwrap();
    let hold = |path: String, reason: Option<String>| {
        s.acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: project.clone(),
            paths: vec![path],
            ttl_seconds: None,
            exclusive: true,
            reason,
            repo_wide: false,
        })
    };
    let open_request = |subject: &str| -> String {
        let mut m = message("alice", vec!["bob".into()], subject.into(), "do it".into());
        m.request = Some(NewRequest {
            due_in_seconds: None,
        });
        s.send_message(m).unwrap().request_id.unwrap()
    };
    let close = |request_id: String, action: RequestAction, note: String| {
        // The recipient completes a request; only its creator cancels it.
        let agent = match action {
            RequestAction::Cancel => "alice",
            _ => "bob",
        };
        s.update_request(RequestUpdate {
            agent: agent.into(),
            request_id,
            action,
            note: Some(note),
            due_in_seconds: None,
        })
    };

    // At the limit.
    register_with(
        s,
        "carol",
        Some("m".repeat(MAX_AGENT_MODEL_BYTES)),
        Some("d".repeat(MAX_AGENT_DESCRIPTION_BYTES)),
    )
    .unwrap();
    let report = hold(
        format!("src/{}", "p".repeat(MAX_HOLD_PATTERN_BYTES - 4)),
        Some("r".repeat(MAX_HOLD_REASON_BYTES)),
    )
    .unwrap();
    assert_eq!(report.granted.len(), 1);
    let note = "n".repeat(MAX_REQUEST_NOTE_BYTES);
    close(open_request("one"), RequestAction::Complete, note.clone()).unwrap();
    close(open_request("two"), RequestAction::Cancel, note).unwrap();

    // One byte over.
    let before = s.events(100_000).unwrap().len();
    let over = |max: usize| "x".repeat(max + 1);
    let cases = [
        (
            invalid(register_with(
                s,
                "dave",
                Some(over(MAX_AGENT_MODEL_BYTES)),
                None,
            )),
            "agent model is 201 bytes; the limit is 200",
        ),
        (
            invalid(register_with(
                s,
                "dave",
                None,
                Some(over(MAX_AGENT_DESCRIPTION_BYTES)),
            )),
            "agent description is 4097 bytes; the limit is 4096",
        ),
        (
            invalid(hold("src/a.rs".into(), Some(over(MAX_HOLD_REASON_BYTES)))),
            "hold reason is 1025 bytes; the limit is 1024",
        ),
        (
            invalid(hold(over(MAX_HOLD_PATTERN_BYTES), None)),
            "hold pattern is 4097 bytes; the limit is 4096",
        ),
        (
            invalid(close(
                open_request("three"),
                RequestAction::Complete,
                over(MAX_REQUEST_NOTE_BYTES),
            )),
            "request note is 16385 bytes; the limit is 16384",
        ),
        (
            invalid(close(
                open_request("four"),
                RequestAction::Cancel,
                over(MAX_REQUEST_NOTE_BYTES),
            )),
            "request note is 16385 bytes; the limit is 16384",
        ),
    ];
    for (got, want) in cases {
        assert_eq!(got, want);
    }
    // Only the two requests opened for the note cases were written (three events each:
    // the message, its delivery bookkeeping aside, and the request).
    let after: Vec<String> = s
        .events(100_000)
        .unwrap()
        .into_iter()
        .skip(before)
        .map(|e| e.kind)
        .collect();
    assert!(
        after
            .iter()
            .all(|k| k == "message.sent" || k == "request.created"),
        "{after:?}"
    );
    assert!(s.agent("dave").is_err());
}

/// The edit guard and a session start write nothing near the caps and are not held to the
/// field rules, so neither can start failing: a guard hold on a path as deep as macOS
/// allows is taken, with the hook's own reason.
#[test]
fn the_edit_guard_and_a_session_start_stay_far_under_the_caps() {
    // The reason the edit hook gives (`another::hooks::AUTO_HOLD_REASON`).
    let reason = "editing (taken by the edit hook)";
    assert!(reason.len() * 10 < MAX_HOLD_REASON_BYTES);
    let n = node("solo");
    let s = &n.store;
    let project = n.project("shop");
    s.ensure_project("claude", &project).unwrap();
    // What a session start registers: a client name, no model and no description.
    s.register_agent(RegisterAgent {
        name: "claude".into(),
        program: Program::Claude,
        project: Some(project.clone()),
        model: None,
        description: None,
    })
    .unwrap();
    let deep = format!(
        "{}/file.rs",
        (0..9).map(|_| "d".repeat(90)).collect::<Vec<_>>().join("/")
    );
    let guard = s
        .guard_edit("claude@shop", &project, &[deep], reason)
        .unwrap();
    assert_eq!(guard.held.len(), 1);
    assert!(guard.held[0].pattern.len() * 4 < MAX_HOLD_PATTERN_BYTES);
    for e in s.events(1000).unwrap() {
        let bytes = e.payload.to_string().len();
        assert!(bytes * 100 < MAX_EVENT_WIRE_BYTES, "{}: {bytes}", e.kind);
    }
}

/// A push batch exactly at the budget is sent whole; with one byte less, its last event
/// goes to the next batch. A batch always holds one event, whatever the budget.
#[test]
fn a_push_batch_is_closed_at_the_byte_budget() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&hub.store, "bob");
    register(&a.store, "alice");
    sync(&hub, &a);
    for i in 0..6 {
        a.store
            .send_message(message(
                "alice",
                vec!["bob@@hub".into()],
                format!("m{i}"),
                "x".repeat(1000 * (i + 1)),
            ))
            .unwrap();
    }
    let all = outbound(&a);
    let ids = |events: &[WireEvent]| -> Vec<String> {
        events.iter().map(|e| e.event_id.clone()).collect()
    };
    let k = all.len() - 2;
    let body = serde_json::to_vec(&PushRequest {
        events: all[..k].to_vec(),
    })
    .unwrap()
    .len();
    assert_eq!(
        body,
        PUSH_ENVELOPE_BYTES - 1 + all[..k].iter().map(|e| e.wire_bytes() + 1).sum::<usize>()
    );

    let (at, high) = a.store.outbound_events_within(0, 500, body).unwrap();
    assert_eq!(ids(&at), ids(&all[..k]));
    assert_eq!(high, all[k - 1].seq);

    let (under, high) = a.store.outbound_events_within(0, 500, body - 1).unwrap();
    assert_eq!(ids(&under), ids(&all[..k - 1]));
    assert_eq!(high, all[k - 2].seq);
    // The event that did not fit opens the next batch.
    let (next, _) = a.store.outbound_events_within(high, 500, body - 1).unwrap();
    assert_eq!(next[0].event_id, all[k - 1].event_id);

    // No budget is too small for one event, and walking the log one event at a time gives
    // every event once, in order.
    let mut cursor = 0;
    let mut walked = Vec::new();
    loop {
        let (batch, high) = a.store.outbound_events_within(cursor, 500, 1).unwrap();
        if high == cursor {
            break;
        }
        assert!(batch.len() <= 1);
        walked.extend(ids(&batch));
        cursor = high;
    }
    assert_eq!(walked, ids(&all));
}

/// A pull page exactly at the budget is sent whole, with the node directory counted; with
/// one byte less, its last event goes to the next page and `more` says so.
#[test]
fn a_pull_page_is_closed_at_the_byte_budget() {
    let hub = node("hub");
    let c = node("spokec");
    pair(&hub, &c);
    register(&hub.store, "bob");
    register(&c.store, "carol");
    sync(&hub, &c);
    for i in 0..6 {
        hub.store
            .send_message(message(
                "bob",
                vec!["carol@@spokec".into()],
                format!("m{i}"),
                "y".repeat(1000 * (i + 1)),
            ))
            .unwrap();
    }
    let peer = c.store.node_id();
    let none = PullAnchors::default();
    let all = hub
        .store
        .pull_page(peer, 0, &none, 500, usize::MAX)
        .unwrap();
    assert!(!all.more);
    let k = all.events.len() - 2;
    // The size the hub counts: the body with the widest cursor and `more` it could carry.
    let counted = serde_json::to_vec(&PullResponse {
        events: all.events[..k].to_vec(),
        cursor: i64::MAX,
        more: false,
        nodes: all.nodes.clone(),
        reset: false,
    })
    .unwrap()
    .len();

    let at = hub.store.pull_page(peer, 0, &none, 500, counted).unwrap();
    assert_eq!(at.events.len(), k);
    assert!(at.more);
    assert_eq!(at.cursor, all.events[k - 1].seq);
    assert!(serde_json::to_vec(&at).unwrap().len() <= counted);

    let under = hub
        .store
        .pull_page(peer, 0, &none, 500, counted - 1)
        .unwrap();
    assert_eq!(under.events.len(), k - 1);
    assert!(under.more);
    assert_eq!(under.cursor, all.events[k - 2].seq);
    let next = hub
        .store
        .pull_page(peer, under.cursor, &none, 500, counted - 1)
        .unwrap();
    assert_eq!(next.events[0].event_id, all.events[k - 1].event_id);

    // No budget is too small for one event, and paging one event at a time gives every
    // event once, in order.
    let mut cursor = 0;
    let mut walked = Vec::new();
    loop {
        let page = hub.store.pull_page(peer, cursor, &none, 500, 1).unwrap();
        assert!(page.events.len() <= 1);
        walked.extend(page.events.iter().map(|e| e.event_id.clone()));
        if !page.more || page.cursor == cursor {
            break;
        }
        cursor = page.cursor;
    }
    let want: Vec<String> = all.events.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(walked, want);
}

/// Make the newest local event of `kind` on `n` as large as a build before D99 could have
/// written it, signed with the node's own key. Returns it as it would travel.
fn grow_last(n: &Node, kind: &str, field: &str, bytes: usize) -> WireEvent {
    let c = Connection::open(n.db()).unwrap();
    let (seq, event_id, node_id, ts, actor, payload, prev_id): (
        i64,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
    ) = c
        .query_row(
            "SELECT seq, event_id, node_id, ts, actor, payload, prev_id FROM events
             WHERE kind = ?1 AND node_id = (SELECT node_id FROM nodes WHERE is_local = 1)
             ORDER BY seq DESC LIMIT 1",
            [kind],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    let mut p: Value = serde_json::from_str(&payload).unwrap();
    p[field] = json!("d".repeat(bytes));
    let payload = p.to_string();
    let secret: String = c
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let sig = crypto::sign(
        &key,
        &crypto::event_message(&event_id, &node_id, ts, &actor, kind, &payload),
    );
    c.execute(
        "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
        params![event_id, payload, sig],
    )
    .unwrap();
    WireEvent {
        seq,
        event_id,
        node_id,
        ts,
        actor,
        kind: kind.into(),
        payload,
        prev_id,
        sig,
    }
}

/// An event over the cap that an earlier build left in a log. Its own node does not send
/// it: the events before it go, then the batch fails with a message that names the event
/// and says the owner decides. A hub that is sent one anyway refuses it like any other
/// event that breaks a rule: reported, not stored.
#[test]
fn an_event_over_the_cap_already_in_a_log_is_named_and_a_peers_is_refused() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&a.store, "alice");
    register(&a.store, "wordy");
    let big = grow_last(&a, "agent.registered", "description", 3 * 1024 * 1024);
    register(&a.store, "after");

    // The events before it are sent first.
    let (first, high) = a.store.outbound_events_within(0, 500, usize::MAX).unwrap();
    assert!(!first.is_empty());
    assert!(first.iter().all(|e| e.event_id != big.event_id));
    assert_eq!(high, big.seq - 1);
    let err = a
        .store
        .outbound_events_within(high, 500, usize::MAX)
        .unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        format!(
            "event {} (agent.registered, {} bytes on the wire) in this node's log is over the \
             limit for one event (2097152 bytes), so it cannot be synced and the events after \
             it wait behind it; retrying will not help; the owner must decide whether it is \
             skipped or keeps blocking (docs/FEDERATION.md, \"Size limits\")",
            big.event_id,
            big.capped_bytes()
        )
    );

    // Sent anyway, as a spoke on an earlier build would: the hub refuses that event only.
    let before = hub.store.events(100_000).unwrap().len();
    let r = hub
        .store
        .ingest(a.store.node_id(), &[first, vec![big.clone()]].concat(), &[])
        .unwrap();
    assert_eq!(r.rejected.len(), 1);
    assert_eq!(r.rejected[0].event_id, big.event_id);
    assert_eq!(
        r.rejected[0].reason,
        format!(
            "event is {} bytes on the wire; the limit for one event is 2097152",
            big.capped_bytes()
        )
    );
    assert_eq!(
        hub.store.events(100_000).unwrap().len(),
        before + r.accepted
    );
    assert!(hub.store.agent("wordy@@spokea").is_err());
    assert!(hub.store.agent("alice@@spokea").is_ok());
}

/// The same on a hub: an event over the cap in the hub's log is not sent to a spoke. The
/// page before it is closed, and the next pull fails with the message that names it.
#[test]
fn an_event_over_the_cap_in_the_hubs_log_is_named_to_the_spoke() {
    let hub = node("hub");
    let c = node("spokec");
    pair(&hub, &c);
    register(&hub.store, "bob");
    register(&hub.store, "wordy");
    let big = grow_last(&hub, "agent.registered", "description", 3 * 1024 * 1024);
    register(&hub.store, "after");
    let peer = c.store.node_id();
    let none = PullAnchors::default();
    let page = hub
        .store
        .pull_page(peer, 0, &none, 500, usize::MAX)
        .unwrap();
    assert!(!page.events.is_empty());
    assert!(page.events.iter().all(|e| e.event_id != big.event_id));
    assert!(page.more);
    assert_eq!(page.cursor, big.seq - 1);
    let err = hub
        .store
        .pull_page(peer, page.cursor, &none, 500, usize::MAX)
        .unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        text.starts_with(&format!(
            "event {} (agent.registered, {} bytes on the wire) in the hub's log is over the \
             limit for one event (2097152 bytes)",
            big.event_id,
            big.capped_bytes()
        )),
        "{text}"
    );
    assert!(text.contains("retrying will not help; the owner must decide"));
}
