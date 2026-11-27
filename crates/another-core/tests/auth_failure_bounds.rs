use std::time::{Duration, Instant};

use another_core::{AuditEntry, AuditRecord, Program, RegisterAgent, Store};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

fn setup() -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let token = store.create_token("alice", &[], None, None).unwrap().secret;
    (dir, store, token)
}

fn issue_access_token(store: &Store) -> String {
    store
        .register_agent(RegisterAgent {
            name: "oauth-agent".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let client = store
        .register_oauth_client(another_core::ClientRegistration {
            client_name: "test client".into(),
            redirect_uris: vec!["https://client.example/callback".into()],
            application_type: Some("web".into()),
            remote: None,
        })
        .unwrap();
    let verifier = "a".repeat(50);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let pending = store
        .begin_authorization(another_core::AuthorizeInput {
            client: another_core::ClientProfile {
                client_id: client.client_id.clone(),
                client_name: client.client_name.clone(),
                redirect_uris: client.redirect_uris.clone(),
            },
            redirect_uri: client.redirect_uris[0].clone(),
            state: None,
            scope: Some("another".into()),
            resource: "https://another.example/mcp".into(),
            code_challenge: challenge,
            code_challenge_method: "S256".into(),
            remote: None,
        })
        .unwrap();
    store
        .approve_oauth(another_core::Approval {
            user_code: pending.user_code.clone(),
            agent: "oauth-agent".into(),
            project_globs: vec!["**".into()],
            label: None,
        })
        .unwrap();
    let status = store
        .authorization_status(
            &pending.request_id,
            &pending.redirect_token,
            "https://another.example",
            true,
        )
        .unwrap();
    let redirect = status.redirect.unwrap();
    let code = redirect
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    store
        .exchange_code(another_core::CodeExchange {
            code: code.into(),
            client_id: client.client_id,
            redirect_uri: client.redirect_uris[0].clone(),
            code_verifier: verifier,
            resource: "https://another.example/mcp".into(),
            remote: None,
        })
        .unwrap()
        .access_token
}

#[test]
fn unauthenticated_audit_rows_are_bounded_across_store_clones() {
    let (_dir, store, _) = setup();
    let clone = store.clone();
    for i in 0..500 {
        let target = if i % 2 == 0 { &store } else { &clone };
        target
            .record_unauthenticated_audit(AuditRecord {
                operation: "POST /fed/pair".into(),
                detail: Some(format!("attacker detail {i}")),
                remote: Some(format!("attacker-{i}")),
                ..Default::default()
            })
            .unwrap();
    }
    let rows = store.audit_log(1000).unwrap();
    assert_eq!(rows.len(), 32);
    assert!(rows
        .iter()
        .all(|row| row.token_id.is_none() && row.agent.is_none()));
    assert!(
        rows.iter().all(|row| row.operation != "audit.budget"),
        "a still-open window must not write a summary row"
    );
}

#[test]
fn flood_a_store(store: &Store, n: usize, tag: &str) {
    for i in 0..n {
        store
            .record_unauthenticated_audit(AuditRecord {
                operation: "POST /fed/pair".into(),
                detail: Some(format!("{tag} {i}")),
                remote: Some(format!("{tag}-{i}")),
                ..Default::default()
            })
            .unwrap();
    }
}

fn budget_rows(store: &Store) -> Vec<AuditEntry> {
    store
        .audit_log(1000)
        .unwrap()
        .into_iter()
        .filter(|row| row.operation == "audit.budget")
        .collect()
}

#[test]
fn unauthenticated_audit_flood_stops_at_32_rows() {
    let (_dir, store, _) = setup();
    flood_a_store(&store, 32, "cap");
    assert_eq!(store.audit_log(1000).unwrap().len(), 32);
    flood_a_store(&store, 20, "over");
    let rows = store.audit_log(1000).unwrap();
    assert_eq!(
        rows.len(),
        32,
        "further refusals in an open window must not write rows"
    );
    assert!(
        budget_rows(&store).is_empty(),
        "a still-open window must not write a summary"
    );
}

#[test]
fn unauthenticated_audit_budget_writes_a_suppressed_count_when_the_window_closes() {
    let (_dir, store, _) = setup();
    flood_a_store(&store, 40, "flood");
    assert_eq!(store.audit_log(1000).unwrap().len(), 32);

    another_core::force_unauthenticated_audit_window_elapsed();
    store.flush_unauthenticated_audit_budget().unwrap();
    let rows = store.audit_log(1000).unwrap();
    assert_eq!(rows.len(), 33, "one summary and the original 32 refusals");
    let summaries = budget_rows(&store);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].outcome, "omitted");
    assert_eq!(
        summaries[0].detail.as_deref(),
        Some("8 unauthenticated refusals were omitted in the last 60 seconds")
    );
}

#[test]
fn unauthenticated_audit_budget_summarizes_a_sustained_flood_once_per_window() {
    let (_dir, store, _) = setup();
    flood_a_store(&store, 40, "w1"); // 32 written, 8 omitted
    another_core::force_unauthenticated_audit_window_elapsed();
    store.flush_unauthenticated_audit_budget().unwrap();
    assert_eq!(store.audit_log(1000).unwrap().len(), 33);
    assert_eq!(budget_rows(&store).len(), 1);

    // After the summary (1 slot), 31 refusals fit, then 9 are omitted.
    flood_a_store(&store, 40, "w2");
    assert_eq!(store.audit_log(1000).unwrap().len(), 64); // 32 + 1 summary + 31
    another_core::force_unauthenticated_audit_window_elapsed();
    store.flush_unauthenticated_audit_budget().unwrap();
    let rows = store.audit_log(1000).unwrap();
    assert_eq!(rows.len(), 65);
    let summaries = budget_rows(&store);
    assert_eq!(summaries.len(), 2, "exactly one summary per closed window");
    assert_eq!(
        summaries[1].detail.as_deref(),
        Some("9 unauthenticated refusals were omitted in the last 60 seconds")
    );

    // A flush without another elapsed window must not write a third summary.
    store.flush_unauthenticated_audit_budget().unwrap();
    assert_eq!(budget_rows(&store).len(), 2);
    assert_eq!(store.audit_log(1000).unwrap().len(), 65);
}

#[test]
fn invalid_token_rejection_does_not_wait_for_external_write_lock() {
    let (dir, store, token) = setup();
    let db = Connection::open(dir.path().join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();

    let clone = store.clone();
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let mut bad = token;
        bad.push('x');
        let result = clone.verify_token(&bad);
        (started.elapsed(), result)
    });
    let (elapsed, result) = worker.join().unwrap();
    db.execute_batch("ROLLBACK").unwrap();

    assert!(result.is_err());
    assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
}

#[test]
fn invalid_oauth_token_rejection_does_not_wait_for_external_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut token = issue_access_token(&store);
    token.push('x');
    let db = Connection::open(dir.path().join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();

    let clone = store.clone();
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let result = clone.verify_access_token(&token, "https://another.example/mcp");
        (started.elapsed(), result)
    });
    let (elapsed, result) = worker.join().unwrap();
    db.execute_batch("ROLLBACK").unwrap();

    assert!(result.is_err());
    assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
}
