//! Message limits and threading that had no test: the subject and body size limits at their
//! boundaries, the three ways `send_message` refuses a thread (unknown reply target, unknown
//! thread, a thread that is not the reply target's), and a reply to one's own message going
//! to the original `to` recipients. A refused send stores nothing.

use another_core::*;
use rusqlite::Connection;

const MAX_BODY_BYTES: usize = 256 * 1024;

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    db: std::path::PathBuf,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    for name in ["alice", "bob", "carol"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    Fx {
        _dir: dir,
        store,
        db: home.join(DB_FILE),
    }
}

impl Fx {
    fn send(&self, subject: &str, body: &str) -> Result<Message> {
        self.store.send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into(), "carol".into()],
            subject: subject.into(),
            body: body.into(),
            ..Default::default()
        })
    }

    fn post(&self, reply_to: Option<&str>, thread_id: Option<&str>) -> Result<Message> {
        self.store.send_message(SendMessage {
            from: "bob".into(),
            to: vec!["alice".into()],
            subject: "follow-up".into(),
            body: "b".into(),
            reply_to: reply_to.map(str::to_string),
            thread_id: thread_id.map(str::to_string),
            ..Default::default()
        })
    }

    fn messages(&self) -> i64 {
        Connection::open(&self.db)
            .unwrap()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn subject_and_body_limits_hold_at_their_boundaries() {
    let f = fx();
    f.send(&"s".repeat(MAX_SUBJECT_CHARS), "b").unwrap();
    // Characters, not bytes: a subject of multi-byte characters at the limit is accepted.
    f.send(&"é".repeat(MAX_SUBJECT_CHARS), "b").unwrap();
    f.send("s", &"b".repeat(MAX_BODY_BYTES)).unwrap();
    let stored = f.messages();

    let err = f.send(&"s".repeat(MAX_SUBJECT_CHARS + 1), "b").unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("subject is longer than"), "{err}");
    // Bytes, not characters: a body of multi-byte characters over the byte limit is refused.
    let err = f
        .send("s", &"é".repeat(MAX_BODY_BYTES / 2 + 1))
        .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("body is larger than"), "{err}");
    assert_eq!(f.messages(), stored, "a refused message was stored");
}

#[test]
fn a_send_into_a_thread_must_name_one_that_exists_and_agrees() {
    let f = fx();
    let first = f.send("one", "b").unwrap();
    let other = f.send("two", "b").unwrap();
    let stored = f.messages();

    let err = f.post(Some("01NOSUCHMESSAGE"), None).unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    assert!(err.to_string().contains("unknown message"), "{err}");

    let err = f.post(None, Some("01NOSUCHTHREAD")).unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    assert!(err.to_string().contains("unknown thread"), "{err}");

    let err = f
        .post(Some(&first.message_id), Some(&other.thread_id))
        .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(
        err.to_string().contains("does not match the thread"),
        "{err}"
    );
    assert_eq!(f.messages(), stored, "a refused message was stored");

    let joined = f.post(None, Some(&other.thread_id)).unwrap();
    assert_eq!(joined.thread_id, other.thread_id);
    assert_eq!(joined.reply_to, None);
    let replied = f
        .post(Some(&first.message_id), Some(&first.thread_id))
        .unwrap();
    assert_eq!(replied.thread_id, first.thread_id);
    assert_eq!(replied.reply_to.as_deref(), Some(first.message_id.as_str()));
}

/// Replying to a message one sent oneself goes to its original `to` recipients, not back to
/// the sender; anyone else's plain reply goes to the sender only. (Whether a plain self-reply
/// should also keep the original cc is not decided here; FINDINGS-219-w9Z.)
#[test]
fn a_reply_to_ones_own_message_goes_to_its_recipients() {
    let f = fx();
    let sent = f.send("plan", "b").unwrap();
    let mine = f
        .store
        .reply(Reply {
            from: "alice".into(),
            message_id: sent.message_id.clone(),
            body: "one more thing".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(mine.to, sent.to);
    assert!(!mine.to.contains(&sent.from), "{mine:?}");
    assert_eq!(mine.subject, "Re: plan");
    assert_eq!(mine.thread_id, sent.thread_id);

    let theirs = f
        .store
        .reply(Reply {
            from: "bob".into(),
            message_id: mine.message_id.clone(),
            body: "ok".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(theirs.to, vec![sent.from.clone()]);
    assert_eq!(theirs.subject, "Re: plan", "the prefix is not repeated");
}

/// A reply to a subject at the limit still fits: the `Re: ` prefix is made room for by
/// shortening the original, so the reply is not refused for its own subject.
#[test]
fn a_reply_to_a_subject_at_the_limit_fits_under_it() {
    let f = fx();
    let sent = f.send(&"s".repeat(MAX_SUBJECT_CHARS), "b").unwrap();
    let reply = f
        .store
        .reply(Reply {
            from: "bob".into(),
            message_id: sent.message_id.clone(),
            body: "ok".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(reply.subject.starts_with("Re: s"), "{}", reply.subject);
    assert_eq!(reply.subject.chars().count(), MAX_SUBJECT_CHARS);
}

/// The `Re:` check ignores case, so a lowercase prefix is not doubled.
#[test]
fn a_lowercase_re_prefix_is_not_repeated() {
    let f = fx();
    let sent = f.send("re: plan", "b").unwrap();
    let reply = f
        .store
        .reply(Reply {
            from: "bob".into(),
            message_id: sent.message_id.clone(),
            body: "ok".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reply.subject, "re: plan");
}
