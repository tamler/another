//! Subjects Another builds itself from file names or titles must still be one line
//! (D75): `send` now refuses control characters, and a held file name may contain a line
//! break.

use another_core::*;

fn store(dir: &std::path::Path) -> (Store, String) {
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_with_clock(&dir.join("home"), clock).unwrap();
    let project = dir.join("shop");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_string_lossy().into_owned();
    for name in ["alice", "bob"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: Some(project.clone()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    (store, project)
}

#[test]
fn a_release_request_for_a_file_with_a_line_break_is_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let (store, project) = store(dir.path());
    let held = store
        .acquire_holds(AcquireHolds {
            agent: "alice@shop".into(),
            project,
            paths: vec!["src/odd\nname.rs".into()],
            ttl_seconds: Some(1800),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert_eq!(held.granted.len(), 1, "{held:?}");
    let ask = store
        .request_hold_release("bob@shop", &held.granted[0].hold_id)
        .expect("asking for a release must not fail on the file name");
    assert!(
        !ask.subject.chars().any(is_unsafe_text_char),
        "{:?}",
        ask.subject
    );
    assert!(
        ask.subject.starts_with("Release src/odd name.rs"),
        "{:?}",
        ask.subject
    );
}

/// A message stored before D75 may have a control character in its subject. Replying to
/// it, and completing a request opened by it, must still work, with one-line subjects.
#[test]
fn replies_and_done_notices_to_old_subjects_are_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _project) = store(dir.path());
    let sent = store
        .send_message(SendMessage {
            from: "alice@shop".into(),
            to: vec!["bob@shop".into()],
            subject: "placeholder".into(),
            body: "body".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("home").join(DB_FILE)).unwrap();
    db.execute(
        "UPDATE messages SET subject = 'old\r\nsubject' WHERE message_id = ?1",
        [&sent.message_id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET title = 'old\r\nsubject' WHERE message_id = ?1",
        [&sent.message_id],
    )
    .unwrap();

    let reply = store
        .reply(Reply {
            from: "bob@shop".into(),
            message_id: sent.message_id.clone(),
            body: "ok".into(),
            all: false,
            importance: None,
            ack_required: false,
        })
        .expect("a reply to an old subject must not fail");
    assert_eq!(reply.subject, "Re: old  subject");

    let request_id = sent.request_id.clone().unwrap();
    store
        .update_request(RequestUpdate {
            agent: "bob@shop".into(),
            request_id,
            action: RequestAction::Complete,
            note: Some("done".into()),
            due_in_seconds: None,
        })
        .expect("completing a request with an old title must not fail");
    let done: String = db
        .query_row(
            "SELECT subject FROM messages WHERE subject LIKE 'Done:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(done, "Done: old  subject");
}
