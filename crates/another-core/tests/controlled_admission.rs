use another_core::{controlled_sha256, Clock, ControlledEdit, Program, Store};

#[test]
fn controlled_literal_conservatively_conflicts_with_case_alias_class_hold() {
    use another_core::{AcquireHolds, RegisterAgent};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = root.to_str().unwrap();
    let store = Store::open(&temp.path().join("home")).unwrap();
    let other = store
        .register_agent(RegisterAgent {
            name: "other".into(),
            program: Program::Claude,
            project: Some(root.into()),
            model: None,
            description: None,
        })
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: other.address,
            project: root.into(),
            paths: vec!["[a-z].rs".into()],
            ttl_seconds: Some(1800),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    store
        .session_actor("claude", Program::Claude, root, None, "session", true)
        .unwrap();
    let edit = ControlledEdit {
        project: root.into(),
        path: "A.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    assert!(
        store
            .prepare_controlled_edit(
                "claude",
                Program::Claude,
                root,
                None,
                "session",
                "call",
                &edit
            )
            .is_err(),
        "controlled literal must not bypass a case-alias class hold"
    );
}

#[test]
fn exact_claim_is_single_use_and_unknown_survives_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = root.to_str().unwrap();
    let (clock, now) = Clock::manual(1_000_000);
    let store = Store::open_with_clock(&temp.path().join("home"), clock).unwrap();
    store
        .session_actor("claude", Program::Claude, root, None, "session-a", true)
        .unwrap();
    store
        .session_actor("claude", Program::Claude, root, None, "session-b", true)
        .unwrap();
    let edit = ControlledEdit {
        project: root.into(),
        path: "a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    let prepare = |session, id| {
        store.prepare_controlled_edit("claude", Program::Claude, root, None, session, id, &edit)
    };
    prepare("session-a", "call-a").unwrap();
    assert!(prepare("session-b", "call-a").is_err());
    let mut altered = edit.clone();
    altered.replacement = "different".into();
    assert!(store
        .claim_controlled_edit("claude", "call-a", &altered, "1:2")
        .is_err());
    assert!(store
        .claim_controlled_edit("codex", "call-a", &edit, "1:2")
        .is_err());
    let claim = store
        .claim_controlled_edit("claude", "call-a", &edit, "1:2")
        .unwrap();
    assert!(store
        .claim_controlled_edit("claude", "call-a", &edit, "1:2")
        .is_err());
    assert!(prepare("session-a", "call-b").is_ok());
    let held = store.holds(Default::default()).unwrap();
    let first = held
        .iter()
        .find(|h| h.reason.as_deref() == Some("controlled edit"))
        .unwrap();
    assert!(store
        .renew_holds(&first.agent, std::slice::from_ref(&first.hold_id))
        .is_err());
    now.store(4_000_000, std::sync::atomic::Ordering::SeqCst);
    store.sweep().unwrap();
    assert_eq!(
        store.unknown_controlled_edits().unwrap(),
        vec![claim.admission_id()]
    );
    assert!(!store.finish_controlled_edit(&claim, true).unwrap());
    assert!(prepare("session-a", "call-a").is_err());
}

#[test]
fn only_server_receipt_settles_exact_claim_once() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = root.to_str().unwrap();
    let store = Store::open(&temp.path().join("home")).unwrap();
    store
        .session_actor("claude", Program::Claude, root, None, "session", true)
        .unwrap();
    let edit = ControlledEdit {
        project: root.into(),
        path: "a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    store
        .prepare_controlled_edit(
            "claude",
            Program::Claude,
            root,
            None,
            "session",
            "call",
            &edit,
        )
        .unwrap();
    assert!(!store
        .confirm_pending_edit("claude", Program::Claude, root, None, "session", "call")
        .unwrap());
    let claim = store
        .claim_controlled_edit("claude", "call", &edit, "1:2")
        .unwrap();
    assert!(store.finish_controlled_edit(&claim, true).unwrap());
    let db = rusqlite::Connection::open(temp.path().join("home/another.db")).unwrap();
    let renewed: String = db
        .query_row(
            "SELECT payload FROM events WHERE kind='hold.renewed' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let renewed: serde_json::Value = serde_json::from_str(&renewed).unwrap();
    assert_eq!(
        renewed["hold_ids"].as_array().map(Vec::len),
        Some(1),
        "renewal must retain the federation event shape"
    );
    assert!(!store.finish_controlled_edit(&claim, true).unwrap());
    assert!(store.unknown_controlled_edits().unwrap().is_empty());
}

#[test]
fn an_unknown_outcome_cannot_have_its_hold_renewed_or_released() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = root.to_str().unwrap();
    let store = Store::open(&temp.path().join("home")).unwrap();
    let (actor, _) = store
        .session_actor("claude", Program::Claude, root, None, "session", true)
        .unwrap();
    let edit = ControlledEdit {
        project: root.into(),
        path: "a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    store
        .prepare_controlled_edit(
            "claude",
            Program::Claude,
            root,
            None,
            "session",
            "unknown",
            &edit,
        )
        .unwrap();
    let claim = store
        .claim_controlled_edit("claude", "unknown", &edit, "1:2")
        .unwrap();
    assert!(!store.finish_controlled_edit(&claim, false).unwrap());
    let hold = store.holds(Default::default()).unwrap().remove(0);
    assert!(
        store
            .renew_holds(&actor, std::slice::from_ref(&hold.hold_id))
            .is_err(),
        "unknown coverage must keep its original deadline"
    );
    assert!(store
        .release_holds(&actor, std::slice::from_ref(&hold.hold_id))
        .is_err());
}

#[test]
fn a_new_nested_registration_prevents_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let store = Store::open(&temp.path().join("home")).unwrap();
    let root_text = root.to_str().unwrap();
    store
        .session_actor("claude", Program::Claude, root_text, None, "session", true)
        .unwrap();
    let edit = ControlledEdit {
        project: root_text.into(),
        path: "nested/a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    store
        .prepare_controlled_edit(
            "claude",
            Program::Claude,
            root_text,
            None,
            "session",
            "call",
            &edit,
        )
        .unwrap();
    let claim = store
        .claim_controlled_edit("claude", "call", &edit, "1:2")
        .unwrap();
    store
        .ensure_project("operator", root.join("nested").to_str().unwrap())
        .unwrap();
    assert!(!store.finish_controlled_edit(&claim, true).unwrap());
    assert_eq!(
        store.unknown_controlled_edits().unwrap(),
        vec![claim.admission_id()]
    );
    assert!(store
        .prepare_controlled_edit(
            "claude",
            Program::Claude,
            root_text,
            None,
            "session",
            "next",
            &edit
        )
        .is_err());
}

#[test]
fn v10_can_record_a_claim_and_keeps_the_v9_secret() {
    let temp = tempfile::tempdir().unwrap();
    let _store = Store::open(temp.path()).unwrap();
    let db = rusqlite::Connection::open(temp.path().join("another.db")).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 10);
    let columns: i64 = db.query_row(
        "SELECT count(*) FROM pragma_table_info('admissions') WHERE name IN ('call_key','operation','claimed_at','object_identity','client','session_digest','preimage','replacement')",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(columns, 8);
    let keys: i64 = db
        .query_row(
            "SELECT count(*) FROM admission_secret WHERE length(secret)=32",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(keys, 1);
}

#[cfg(windows)]
#[test]
fn native_windows_private_home_and_disk_claims_work() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let root = root.to_str().unwrap();
    let store = Store::open(&home).unwrap();
    store
        .session_actor(
            "claude",
            Program::Claude,
            root,
            None,
            "windows-session",
            true,
        )
        .unwrap();
    let edit = ControlledEdit {
        project: root.into(),
        path: "a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    store
        .prepare_controlled_edit(
            "claude",
            Program::Claude,
            root,
            None,
            "windows-session",
            "call",
            &edit,
        )
        .unwrap();
    let claim = store
        .claim_controlled_edit("claude", "call", &edit, "1:2")
        .unwrap();
    assert!(!store.finish_controlled_edit(&claim, false).unwrap());
    drop(store);
    let reopened = Store::open(&home).unwrap();
    assert_eq!(reopened.unknown_controlled_edits().unwrap().len(), 1);
    assert!(std::fs::read_dir(home.join("identity-ops"))
        .unwrap()
        .next()
        .is_none());
}
