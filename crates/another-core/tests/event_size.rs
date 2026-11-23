//! Event size (Q26, D99): one event is at most `MAX_EVENT_WIRE_BYTES` on the wire, the
//! fields a local caller could grow without bound each have a rule, and a push batch and a
//! pull page are bounded by bytes as well as by count. Two nodes, each with its own
//! database in a temp directory, paired without HTTP.

use std::path::PathBuf;

use another_core::test_support::{forge_event, wire_events_of};
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
        .unwrap()
        .0;
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

    let (at, high, _) = a.store.outbound_events_within(0, 500, body).unwrap();
    assert_eq!(ids(&at), ids(&all[..k]));
    assert_eq!(high, all[k - 1].seq);

    let (under, high, _) = a.store.outbound_events_within(0, 500, body - 1).unwrap();
    assert_eq!(ids(&under), ids(&all[..k - 1]));
    assert_eq!(high, all[k - 2].seq);
    // The event that did not fit opens the next batch.
    let (next, _, _) = a.store.outbound_events_within(high, 500, body - 1).unwrap();
    assert_eq!(next[0].event_id, all[k - 1].event_id);

    // No budget is too small for one event, and walking the log one event at a time gives
    // every event once, in order.
    let mut cursor = 0;
    let mut walked = Vec::new();
    loop {
        let (batch, high, _) = a.store.outbound_events_within(cursor, 500, 1).unwrap();
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
        .unwrap()
        .0;
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

    let at = hub.store.pull_page(peer, 0, &none, 500, counted).unwrap().0;
    assert_eq!(at.events.len(), k);
    assert!(at.more);
    assert_eq!(at.cursor, all.events[k - 1].seq);
    assert!(serde_json::to_vec(&at).unwrap().len() <= counted);

    let under = hub
        .store
        .pull_page(peer, 0, &none, 500, counted - 1)
        .unwrap()
        .0;
    assert_eq!(under.events.len(), k - 1);
    assert!(under.more);
    assert_eq!(under.cursor, all.events[k - 2].seq);
    let next = hub
        .store
        .pull_page(peer, under.cursor, &none, 500, counted - 1)
        .unwrap()
        .0;
    assert_eq!(next.events[0].event_id, all.events[k - 1].event_id);

    // No budget is too small for one event, and paging one event at a time gives every
    // event once, in order.
    let mut cursor = 0;
    let mut walked = Vec::new();
    loop {
        let page = hub.store.pull_page(peer, cursor, &none, 500, 1).unwrap().0;
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
    let mut ev = wire_events_of(&c, n.store.node_id())
        .into_iter()
        .rev()
        .find(|e| e.kind == kind)
        .unwrap();
    let mut p: Value = serde_json::from_str(&ev.payload).unwrap();
    p[field] = json!("d".repeat(bytes));
    ev.payload = p.to_string();
    let ev = forge_event(&c, ev);
    c.execute(
        "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
        params![ev.event_id, ev.payload, ev.sig],
    )
    .unwrap();
    ev
}

/// The cap is a rule for what this build writes, not for what is already in a log. An
/// event over it that an earlier build left in a spoke's log is sent, alone in its batch
/// when it is larger than the budget; the hub stores it and relays it to a second spoke,
/// and the events around it travel too.
#[test]
fn an_event_over_the_cap_already_in_a_spokes_log_is_sent_stored_and_relayed() {
    let hub = node("hub");
    let a = node("spokea");
    let c = node("spokec");
    pair(&hub, &a);
    pair(&hub, &c);
    register(&a.store, "alice");
    register(&a.store, "wordy");
    let big = grow_last(&a, "agent.registered", "description", 3 * 1024 * 1024);
    assert!(big.capped_bytes() > MAX_EVENT_WIRE_BYTES);
    register(&a.store, "after");

    // Under a budget a third its size it still travels, in a batch of its own.
    let mut cursor = 0;
    let mut batches: Vec<Vec<WireEvent>> = Vec::new();
    loop {
        let (batch, high, _) = a
            .store
            .outbound_events_within(cursor, 500, 1024 * 1024)
            .unwrap();
        if high == cursor {
            break;
        }
        cursor = high;
        if !batch.is_empty() {
            batches.push(batch);
        }
    }
    let alone: Vec<&Vec<WireEvent>> = batches
        .iter()
        .filter(|b| b.iter().any(|e| e.event_id == big.event_id))
        .collect();
    assert_eq!(alone.len(), 1);
    assert_eq!(alone[0].len(), 1);
    assert_eq!(batches.len(), 3);

    for batch in &batches {
        let r = hub.store.ingest(a.store.node_id(), batch, &[]).unwrap();
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        assert_eq!(r.accepted, batch.len());
    }
    for name in ["alice", "wordy", "after"] {
        assert!(
            hub.store.agent(&format!("{name}@@spokea")).is_ok(),
            "{name}"
        );
    }

    // The hub relays it to the second spoke under the page budget of this build.
    let mut cursor = 0;
    loop {
        let page = hub
            .store
            .pull_for_from(c.store.node_id(), cursor, &PullAnchors::default(), 500)
            .unwrap();
        let r = c
            .store
            .ingest(hub.store.node_id(), &page.events, &page.nodes)
            .unwrap();
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        if !page.more || page.cursor == cursor {
            break;
        }
        cursor = page.cursor;
    }
    for name in ["alice", "wordy", "after"] {
        assert!(c.store.agent(&format!("{name}@@spokea")).is_ok(), "{name}");
    }
}

/// An event that does not fit the transport even alone, a spoke's own event that would make
/// a push over the 8 MiB a hub reads, is stepped past (D-next): under each budget the scan
/// returns it as stepped past, never as an event to send, and returns the events after it.
#[test]
fn a_spokes_event_over_the_push_limit_is_stepped_past_and_returned() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&a.store, "alice");
    register(&a.store, "wordy");
    let big = grow_last(&a, "agent.registered", "description", 9 * 1024 * 1024);
    register(&a.store, "after");
    let after_big: Vec<String> = outbound(&a)
        .into_iter()
        .filter(|e| e.seq > big.seq)
        .map(|e| e.event_id)
        .collect();
    assert!(!after_big.is_empty());

    for budget in [1, PUSH_BATCH_BUDGET_BYTES, usize::MAX] {
        let mut cursor = 0;
        let mut sent = Vec::new();
        let mut stepped_past = Vec::new();
        loop {
            let (batch, high, skipped) =
                a.store.outbound_events_within(cursor, 500, budget).unwrap();
            if high == cursor {
                break;
            }
            sent.extend(batch.into_iter().map(|e| e.event_id));
            stepped_past.extend(skipped);
            cursor = high;
        }
        assert!(!sent.contains(&big.event_id), "budget {budget}");
        for id in &after_big {
            assert!(sent.contains(id), "budget {budget}: {id} was not returned");
        }
        assert_eq!(stepped_past.len(), 1, "budget {budget}");
        let s = &stepped_past[0];
        assert_eq!(s.event_id, big.event_id);
        assert_eq!(s.event_node, a.store.node_id());
        assert_eq!(s.event_kind, "agent.registered");
        assert_eq!(s.bytes, big.wire_bytes());
        assert_eq!(s.about.as_deref(), Some("agent wordy"));
        assert_eq!((s.route.as_str(), s.route_nodes.len()), ("directory", 0));
    }
    // The scan is a read: it wrote no marker.
    let markers: i64 = Connection::open(a.db())
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE kind = 'sync.skipped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(markers, 0);
}

/// The edge of what a push carries: an event whose push body is exactly the limit is sent,
/// one byte more is stepped past and not returned as an event.
#[test]
fn an_event_exactly_at_the_push_limit_is_sent() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&a.store, "wordy");
    let base = grow_last(&a, "agent.registered", "description", 0).wire_bytes();
    let fits = PUSH_BODY_LIMIT_BYTES - PUSH_ENVELOPE_BYTES - base;
    let at = grow_last(&a, "agent.registered", "description", fits);
    assert_eq!(at.wire_bytes() + PUSH_ENVELOPE_BYTES, PUSH_BODY_LIMIT_BYTES);
    let sent = outbound(&a);
    assert!(sent.iter().any(|e| e.event_id == at.event_id));
    let body = serde_json::to_vec(&PushRequest {
        events: vec![at.clone()],
    })
    .unwrap();
    assert_eq!(body.len(), PUSH_BODY_LIMIT_BYTES);
    let over = grow_last(&a, "agent.registered", "description", fits + 1);
    assert_eq!(
        over.wire_bytes() + PUSH_ENVELOPE_BYTES,
        PUSH_BODY_LIMIT_BYTES + 1
    );
    let (events, high, skipped) = a
        .store
        .outbound_events_within(0, 10_000, usize::MAX)
        .unwrap();
    assert!(events.iter().all(|e| e.event_id != over.event_id));
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].event_id, over.event_id);
    assert_eq!(skipped[0].bytes, over.wire_bytes());
    assert!(high >= over.seq);
}

/// The same on a hub. An event of 12 MiB in the hub's own log, larger than the page budget
/// and than anything a push carries, is sent to a spoke in a page of its own. One that
/// would make an answer over the 32 MiB a spoke reads is not: the page steps past it
/// (D-next), has `more` set so the spoke asks again, and carries the event after it.
#[test]
fn a_hubs_event_travels_alone_up_to_what_a_spoke_reads_and_is_stepped_past_beyond() {
    let hub = node("hub");
    let c = node("spokec");
    pair(&hub, &c);
    register(&hub.store, "bob");
    register(&hub.store, "wordy");
    let big = grow_last(&hub, "agent.registered", "description", 12 * 1024 * 1024);
    register(&hub.store, "after");
    let peer = c.store.node_id();
    let none = PullAnchors::default();
    let mut cursor = 0;
    let mut pages = Vec::new();
    loop {
        let page = hub.store.pull_for_from(peer, cursor, &none, 500).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= PULL_BODY_LIMIT_BYTES);
        let r = c
            .store
            .ingest(hub.store.node_id(), &page.events, &page.nodes)
            .unwrap();
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        let (more, next) = (page.more, page.cursor);
        pages.push(page.events);
        if !more || next == cursor {
            break;
        }
        cursor = next;
    }
    let with_big: Vec<&Vec<WireEvent>> = pages
        .iter()
        .filter(|p| p.iter().any(|e| e.event_id == big.event_id))
        .collect();
    assert_eq!(with_big.len(), 1);
    assert_eq!(with_big[0].len(), 1);
    for name in ["bob", "wordy", "after"] {
        assert!(c.store.agent(&format!("{name}@@hub")).is_ok(), "{name}");
    }

    register(&hub.store, "huge");
    let huge = grow_last(&hub, "agent.registered", "description", 33 * 1024 * 1024);
    register(&hub.store, "last");
    let last = grow_last(&hub, "agent.registered", "description", 0);
    let (page, skipped) = hub
        .store
        .pull_page(peer, cursor, &none, 500, PULL_PAGE_BUDGET_BYTES)
        .unwrap();
    assert!(page.events.iter().all(|e| e.event_id != huge.event_id));
    assert!(page.events.iter().any(|e| e.event_id == last.event_id));
    assert!(page.more);
    assert!(page.cursor >= last.seq);
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].event_id, huge.event_id);
    assert_eq!(skipped[0].bytes, huge.wire_bytes());
    assert_eq!(skipped[0].about.as_deref(), Some("agent huge"));
}

/// Whatever arrived by push can be relayed: the largest event a push carries, alone in a
/// pull answer with the node directory, is under what a spoke reads.
#[test]
fn the_largest_pushed_event_fits_a_pull_answer() {
    let hub = node("hub");
    let a = node("spokea");
    let c = node("spokec");
    pair(&hub, &a);
    pair(&hub, &c);
    register(&a.store, "wordy");
    let base = grow_last(&a, "agent.registered", "description", 0).wire_bytes();
    let largest = grow_last(
        &a,
        "agent.registered",
        "description",
        PUSH_BODY_LIMIT_BYTES - PUSH_ENVELOPE_BYTES - base,
    );
    let sent = outbound(&a);
    let r = hub.store.ingest(a.store.node_id(), &sent, &[]).unwrap();
    assert!(r.rejected.is_empty(), "{:?}", r.rejected);
    let none = PullAnchors::default();
    let mut cursor = 0;
    let mut seen = false;
    loop {
        let page = hub
            .store
            .pull_for_from(c.store.node_id(), cursor, &none, 500)
            .unwrap();
        let bytes = serde_json::to_vec(&page).unwrap().len();
        if page.events.iter().any(|e| e.event_id == largest.event_id) {
            seen = true;
            assert_eq!(page.events.len(), 1);
            assert!(bytes < PULL_BODY_LIMIT_BYTES / 2, "{bytes}");
        }
        if !page.more || page.cursor == cursor {
            break;
        }
        cursor = page.cursor;
    }
    assert!(seen);
}

/// A heartbeat and a session end over more holds than one event lists write several
/// events in their one transaction, so neither meets the cap: with 75,000 live holds both
/// succeed, every hold is renewed and then released, and a hub that syncs ends in the same
/// state. Up to 10,000 holds it is one event, as before.
#[test]
fn a_heartbeat_and_a_session_end_over_75_000_holds_succeed_in_several_events() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    register(&a.store, "alice");
    let project = a.project("shop");
    a.store.ensure_project("alice", &project).unwrap();
    sync(&hub, &a);
    // Taking 75,000 holds through `acquire_holds` compares each with every other and takes
    // many minutes in a debug build, so the rows are written straight into the spoke's
    // table, and into the hub's as it would hold them after importing them.
    const HOLDS: usize = 75_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let planted_at = now - 60_000;
    let (agent_id, project_id): (String, String) = Connection::open(a.db())
        .unwrap()
        .query_row(
            "SELECT (SELECT agent_id FROM agents WHERE name = 'alice'),
                    (SELECT project_id FROM projects LIMIT 1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    for n in [&a, &hub] {
        let mut c = Connection::open(n.db()).unwrap();
        let tx = c.transaction().unwrap();
        for i in 0..HOLDS {
            tx.execute(
                "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive,
                   ttl_seconds, created_at, renewed_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 14400, ?6, ?6, ?7)",
                params![
                    format!("01HOLD{i:020}"),
                    a.store.node_id(),
                    project_id,
                    agent_id,
                    format!("src/f{i}.rs"),
                    planted_at,
                    now + 3_600_000
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let kinds = |n: &Node, kind: &str| -> Vec<usize> {
        let c = Connection::open(n.db()).unwrap();
        let mut stmt = c
            .prepare("SELECT payload FROM events WHERE kind = ?1 ORDER BY seq")
            .unwrap();
        let rows = stmt.query_map([kind], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|p| {
            let p: Value = serde_json::from_str(&p.unwrap()).unwrap();
            p["hold_ids"].as_array().unwrap().len()
        })
        .collect()
    };
    let live = |n: &Node| -> i64 {
        Connection::open(n.db())
            .unwrap()
            .query_row(
                "SELECT count(*) FROM holds WHERE released_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    assert!(a
        .store
        .heartbeat("alice", false)
        .unwrap()
        .renewed_holds
        .is_empty());
    let ids: Vec<String> = (0..HOLDS).map(|i| format!("01HOLD{i:020}")).collect();
    a.store.renew_holds("alice", &ids).unwrap();
    assert_eq!(
        kinds(&a, "hold.renewed"),
        [10_000, 10_000, 10_000, 10_000, 10_000, 10_000, 10_000, 5_000]
    );
    assert_eq!(live(&a), HOLDS as i64);

    a.store.heartbeat("alice", true).unwrap();
    assert_eq!(kinds(&a, "hold.released").iter().sum::<usize>(), HOLDS);
    assert_eq!(kinds(&a, "hold.released").len(), 8);
    assert_eq!(live(&a), 0);
    for e in outbound(&a) {
        assert!(e.capped_bytes() < MAX_EVENT_WIRE_BYTES / 4, "{}", e.kind);
    }

    // The hub ends in the same state: every hold renewed, then released.
    let mut cursor = 0;
    loop {
        let (batch, high) = a.store.outbound_events(cursor, 500).unwrap();
        if high == cursor {
            break;
        }
        let r = hub.store.ingest(a.store.node_id(), &batch, &[]).unwrap();
        assert!(r.rejected.is_empty(), "{:?}", r.rejected.first());
        cursor = high;
    }
    for n in [&a, &hub] {
        let (held, renewed, released): (i64, i64, i64) = Connection::open(n.db())
            .unwrap()
            .query_row(
                "SELECT count(*), coalesce(sum(renewed_at > ?1), 0),
                        coalesce(sum(released_at IS NOT NULL), 0) FROM holds",
                [planted_at],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (held, renewed, released),
            (HOLDS as i64, HOLDS as i64, HOLDS as i64)
        );
    }
}

/// The ordinary case is unchanged: one renewal and one release event, with the payload a
/// build before D99 wrote.
#[test]
fn explicit_renew_and_session_end_over_a_few_holds_write_one_event_each() {
    let a = node("solo");
    register(&a.store, "alice");
    let project = a.project("shop");
    a.store.ensure_project("alice", &project).unwrap();
    let report = a
        .store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: project.clone(),
            paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    let ids: Vec<&str> = report.granted.iter().map(|h| h.hold_id.as_str()).collect();
    a.store.heartbeat("alice", false).unwrap();
    a.store
        .renew_holds(
            "alice",
            &ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
    a.store.heartbeat("alice", true).unwrap();
    let payloads = |kind: &str| -> Vec<String> {
        let c = Connection::open(a.db()).unwrap();
        let mut stmt = c
            .prepare("SELECT payload FROM events WHERE kind = ?1 ORDER BY seq")
            .unwrap();
        let rows = stmt.query_map([kind], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|p| p.unwrap()).collect()
    };
    assert_eq!(
        payloads("hold.renewed"),
        [format!(r#"{{"hold_ids":["{}","{}"]}}"#, ids[0], ids[1])]
    );
    assert_eq!(
        payloads("hold.released"),
        [format!(
            r#"{{"hold_ids":["{}","{}"],"reason":"session_end"}}"#,
            ids[0], ids[1]
        )]
    );
}

/// An automatic path that did meet the cap gives the error to its caller and writes
/// nothing. The edit guard is not held to the field rules, so a reason of 3 MiB reaches the
/// one place events are written, which refuses it.
#[test]
fn an_automatic_write_over_the_cap_surfaces_the_error() {
    let a = node("solo");
    register(&a.store, "alice");
    let project = a.project("shop");
    a.store.ensure_project("alice", &project).unwrap();
    let before = a.store.events(1000).unwrap().len();
    let err = invalid(a.store.guard_edit(
        "alice",
        &project,
        &["src/a.rs".to_string()],
        &"r".repeat(3 * 1024 * 1024),
    ));
    assert!(
        err.starts_with("this hold.acquired event would be ")
            && err.ends_with("the limit for one event is 2097152"),
        "{err}"
    );
    assert_eq!(a.store.events(1000).unwrap().len(), before);
    assert!(a.store.holds(HoldFilter::default()).unwrap().is_empty());
}
