use std::sync::atomic::Ordering;

use another_core::{AcquireHolds, Clock, Program, RegisterAgent, Store};
use rusqlite::{params, Connection};

#[test]
fn pending_hold_is_not_renewed_released_or_adopted_by_ordinary_actions() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let (clock, now) = Clock::manual(1_000_000);
    let store = Store::open_with_clock(&home, clock).unwrap();
    let actor = store
        .register_agent(RegisterAgent {
            name: "pending-owner".into(),
            program: Program::Claude,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap()
        .address;
    let acquire = || {
        store
            .acquire_holds(AcquireHolds {
                agent: actor.clone(),
                project: project.clone(),
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
            .granted[0]
            .hold_id
            .clone()
    };
    let pending = acquire();
    let db = Connection::open(home.join("another.db")).unwrap();
    let (node, agent, expiry): (String, String, i64) = db
        .query_row(
            "SELECT node_id, agent_id, expires_at FROM holds WHERE hold_id=?1",
            [&pending],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    db.execute("INSERT INTO admissions(admission_id,node_id,agent_id,correlation,targets,state,created_at) VALUES ('test-admission',?1,?2,?3,?4,'pending',?5)",
        params![node, agent, vec![1u8;32], vec![2u8;32], 1_000_000_i64]).unwrap();
    db.execute(
        "INSERT INTO admission_holds(admission_id,hold_id) VALUES ('test-admission',?1)",
        [&pending],
    )
    .unwrap();
    let snapshot = || -> (i64, Option<i64>) {
        db.query_row(
            "SELECT expires_at, released_at FROM holds WHERE hold_id=?1",
            [&pending],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    now.store(expiry - 1000, Ordering::SeqCst);
    assert!(store
        .renew_holds(&actor, std::slice::from_ref(&pending))
        .is_err());
    assert!(store
        .release_holds(&actor, std::slice::from_ref(&pending))
        .is_err());
    store.keepalive(&actor).unwrap();
    store.heartbeat(&actor, false).unwrap();
    assert_eq!(snapshot(), (expiry, None));
    let explicit = acquire();
    assert_ne!(
        explicit, pending,
        "a manual hold must not adopt a pending one"
    );
    store.heartbeat(&actor, true).unwrap();
    assert_eq!(snapshot(), (expiry, None));
    now.store(expiry, Ordering::SeqCst);
    store.sweep().unwrap();
    assert_eq!(snapshot(), (expiry, Some(expiry)));
    let state: String = db
        .query_row(
            "SELECT state FROM admissions WHERE admission_id='test-admission'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "expired",
        "an expired pending admission must not remain confirmable"
    );
}

#[test]
fn missing_pending_table_never_falls_back_to_creating_a_hold() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    let actor = store
        .register_agent(RegisterAgent {
            name: "missing-schema".into(),
            program: Program::Claude,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap()
        .address;
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute_batch("DROP TABLE admission_holds").unwrap();
    let result = store.acquire_holds(AcquireHolds {
        agent: actor,
        project,
        paths: vec!["src/a.rs".into()],
        ttl_seconds: Some(1800),
        exclusive: true,
        reason: None,
        repo_wide: false,
    });
    assert!(
        result.is_err(),
        "missing pending-hold metadata must not be treated as no existing hold"
    );
    let count: i64 = db
        .query_row("SELECT count(*) FROM holds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn foreign_origin_hold_with_local_actor_is_not_own_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    let actor = store
        .register_agent(RegisterAgent {
            name: "local-owner".into(),
            program: Program::Claude,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap()
        .address;
    let hold = store
        .acquire_holds(AcquireHolds {
            agent: actor.clone(),
            project: project.clone(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(1800),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap()
        .granted[0]
        .hold_id
        .clone();
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute(
        "INSERT INTO nodes(node_id,display_name,created_at) VALUES ('remote-node','remote',0)",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE holds SET node_id='remote-node' WHERE hold_id=?1",
        [&hold],
    )
    .unwrap();
    let result = store
        .guard_edit(&actor, &project, &["src/a.rs".into()], "edit")
        .unwrap();
    assert!(
        !result.conflicts.is_empty(),
        "foreign-origin hold must be treated as a conflict"
    );
    assert!(result.held.is_empty());
}

#[test]
fn pending_admission_creates_distinct_holds_and_denies_replay() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    let admit = |tool_id: &str| {
        store.admit_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "session-a",
            tool_id,
            "edit",
            |_| another_core::EditScope {
                groups: vec![(project.clone(), vec!["src/a.rs".into()])],
                unguarded: vec![],
            },
        )
    };
    let private_id = "private-tool-call-8675309";
    assert!(admit(private_id).unwrap().admitted());
    assert!(
        admit(private_id).is_err(),
        "duplicate correlation must deny"
    );
    assert!(admit("second").unwrap().admitted());
    let db = Connection::open(home.join("another.db")).unwrap();
    let mut q = db.prepare("SELECT a.state,h.hold_id FROM admissions a JOIN admission_holds ah ON ah.admission_id=a.admission_id JOIN holds h ON h.hold_id=ah.hold_id ORDER BY h.hold_id").unwrap();
    let rows: Vec<(String, String)> = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.0 == "pending"));
    assert_ne!(rows[0].1, rows[1].1);
    drop(q);
    let confirm = |tool_id: &str| {
        store.confirm_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "session-a",
            tool_id,
        )
    };
    assert!(!confirm("wrong").unwrap());
    assert!(store
        .confirm_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "other-session",
            private_id
        )
        .is_err());
    assert!(confirm(private_id).unwrap());
    assert!(
        !confirm(private_id).unwrap(),
        "duplicate receipt must not confirm twice"
    );
    let states: Vec<String> = db
        .prepare("SELECT state FROM admissions ORDER BY state")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(states, ["confirmed", "pending"]);
    let leaked: i64 = db
        .query_row(
            "SELECT count(*) FROM events WHERE instr(payload,?1)>0",
            [private_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        leaked, 0,
        "raw tool-call ID must not appear in the event log"
    );
}

#[test]
fn missing_admission_secret_denies_without_regenerating_or_creating_holds() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute("DELETE FROM admission_secret", []).unwrap();
    let result = store.admit_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "call-a",
        "edit",
        |_| another_core::EditScope {
            groups: vec![(project.clone(), vec!["src/a.rs".into()])],
            unguarded: vec![],
        },
    );
    assert!(result.is_err());
    let counts: (i64, i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM admission_secret), (SELECT count(*) FROM admissions), (SELECT count(*) FROM holds)",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(counts, (0, 0, 0));
}

#[test]
fn extra_admission_secret_row_denies_without_creating_holds() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    db.execute(
        "INSERT INTO admission_secret(singleton,secret) VALUES (2,?1)",
        [vec![7u8; 32]],
    )
    .unwrap();
    let result = store.admit_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "call-a",
        "edit",
        |_| another_core::EditScope {
            groups: vec![(project.clone(), vec!["src/a.rs".into()])],
            unguarded: vec![],
        },
    );
    assert!(result.is_err(), "extra key rows must be rejected");
    let holds: i64 = db
        .query_row("SELECT count(*) FROM holds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(holds, 0);
}

#[test]
fn receipt_does_not_confirm_when_an_admission_mapping_is_dangling() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    assert!(store
        .admit_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "session-a",
            "call-a",
            "edit",
            |_| another_core::EditScope {
                groups: vec![(project.clone(), vec!["src/a.rs".into()])],
                unguarded: vec![]
            },
        )
        .unwrap()
        .admitted());
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db.execute("INSERT INTO admission_holds(admission_id,hold_id) SELECT admission_id,'missing-hold' FROM admissions", []).unwrap();
    let result = store.confirm_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "call-a",
    );
    assert!(
        !matches!(result, Ok(true)),
        "a dangling mapping must not be confirmed"
    );
    let state: String = db
        .query_row("SELECT state FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "pending");
    db.execute("UPDATE holds SET expires_at=0", []).unwrap();
    store.sweep().unwrap();
    let after_sweep: String = db
        .query_row("SELECT state FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        after_sweep, "pending",
        "an incomplete mapping has an unknown outcome"
    );
}

#[test]
fn repeated_target_groups_are_rejected_without_partial_admission() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    let result = store.admit_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "duplicate-target",
        "edit",
        |_| another_core::EditScope {
            groups: vec![
                (project.clone(), vec!["src/a.rs".into()]),
                (project.clone(), vec!["src/a.rs".into()]),
            ],
            unguarded: vec![],
        },
    );
    assert!(
        result.is_err(),
        "duplicate normalized targets must not create duplicate holds"
    );
    let db = Connection::open(home.join("another.db")).unwrap();
    let counts: (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM admissions),(SELECT count(*) FROM holds)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (0, 0));
}

#[test]
fn receipt_does_not_confirm_if_a_target_mapping_was_removed() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let store = Store::open(&home).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    assert!(store
        .admit_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "session-a",
            "two-targets",
            "edit",
            |_| another_core::EditScope {
                groups: vec![(project.clone(), vec!["src/a.rs".into(), "src/b.rs".into()])],
                unguarded: vec![]
            },
        )
        .unwrap()
        .admitted());
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute(
        "DELETE FROM admission_holds WHERE hold_id=(SELECT min(hold_id) FROM admission_holds)",
        [],
    )
    .unwrap();
    let result = store.confirm_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "two-targets",
    );
    assert!(
        !matches!(result, Ok(true)),
        "a partial target set must not be confirmed"
    );
    let state: String = db
        .query_row("SELECT state FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "pending");
}

#[test]
fn receipt_does_not_confirm_a_substituted_hold_with_the_same_target() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap().to_owned();
    let home = temp.path().join("home");
    let (clock, _) = Clock::manual(1_000_000);
    let store = Store::open_with_clock(&home, clock).unwrap();
    store
        .session_actor("claude", Program::Claude, &project, None, "session-a", true)
        .unwrap();
    assert!(store
        .admit_pending_edit(
            "claude",
            Program::Claude,
            &project,
            None,
            "session-a",
            "call-a",
            "edit",
            |_| another_core::EditScope {
                groups: vec![(project.clone(), vec!["src/a.rs".into()])],
                unguarded: vec![]
            }
        )
        .unwrap()
        .admitted());
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute("INSERT INTO holds (hold_id,node_id,project_id,agent_id,pattern,exclusive,reason,ttl_seconds,created_at,renewed_at,expires_at) SELECT 'substitute',node_id,project_id,agent_id,pattern,exclusive,reason,ttl_seconds,created_at,renewed_at,expires_at FROM holds", []).unwrap();
    db.execute("UPDATE admission_holds SET hold_id='substitute'", [])
        .unwrap();
    let result = store.confirm_pending_edit(
        "claude",
        Program::Claude,
        &project,
        None,
        "session-a",
        "call-a",
    );
    assert!(
        !matches!(result, Ok(true)),
        "a substituted hold must never be confirmed"
    );
    let state: String = db
        .query_row("SELECT state FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "pending");
}
