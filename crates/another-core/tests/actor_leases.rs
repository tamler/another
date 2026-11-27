use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use another_core::{
    ActorProof, CheckoutRight, Clock, EditOutcome, LeaseRef, RegisteredActor, Store, SubjectKind,
    DEFAULT_TTL_SECONDS,
};

struct Fixture {
    store: Store,
    now: Arc<AtomicI64>,
    root: RegisteredActor,
    child: RegisteredActor,
    sibling: RegisteredActor,
    /// File subjects are exact absolute targets; relative ones are refused.
    files: tempfile::TempDir,
}

impl Fixture {
    fn file(&self, name: &str) -> String {
        self.files.path().join(name).to_string_lossy().into_owned()
    }

    fn new() -> Self {
        let (clock, now) = Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let participant = store.enroll_participant().unwrap();
        let root = store
            .register_actor(&participant, "native-root", "codex", None)
            .unwrap();
        let child = store
            .register_actor(&participant, "native-child", "child", Some(&root.proof))
            .unwrap();
        let sibling = store
            .register_actor(&participant, "native-sibling", "sibling", Some(&root.proof))
            .unwrap();
        Self {
            store,
            now,
            root,
            child,
            sibling,
            files: tempfile::tempdir().unwrap(),
        }
    }

    fn advance(&self, seconds: i64) {
        self.now.fetch_add(seconds * 1000, Ordering::SeqCst);
    }
    fn checkout(
        &self,
        proof: &ActorProof,
        kind: SubjectKind,
        key: &str,
        ttl: Option<i64>,
    ) -> another_core::RightLease {
        let subject = if kind == SubjectKind::File {
            self.file(key)
        } else {
            key.into()
        };
        self.store
            .checkout_right(
                proof,
                key,
                CheckoutRight {
                    subject_kind: kind,
                    subject_key: subject,
                    ttl_seconds: ttl,
                },
            )
            .unwrap()
    }
}

#[test]
fn every_subject_uses_checkout_duration_and_expiry_without_a_sweep() {
    let f = Fixture::new();
    for (i, kind) in [
        SubjectKind::File,
        SubjectKind::Presence,
        SubjectKind::Resource,
        SubjectKind::Queue,
        SubjectKind::Request,
        SubjectKind::Wake,
        SubjectKind::Delivery,
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("subject-{i}");
        let lease = f.checkout(&f.root.proof, kind, &key, None);
        assert_eq!(lease.ttl_seconds, DEFAULT_TTL_SECONDS);
        assert_eq!(lease.expires_at, lease.last_update_at + 120_000);
        assert!(f
            .store
            .checkout_right(
                &f.child.proof,
                &key,
                CheckoutRight {
                    subject_kind: kind,
                    subject_key: if kind == SubjectKind::File {
                        f.file(&key)
                    } else {
                        key.clone()
                    },
                    ttl_seconds: None
                }
            )
            .is_err());
    }
    f.advance(120);
    for (i, kind) in [
        SubjectKind::File,
        SubjectKind::Presence,
        SubjectKind::Resource,
        SubjectKind::Queue,
        SubjectKind::Request,
        SubjectKind::Wake,
        SubjectKind::Delivery,
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("subject-{i}");
        let next = f.checkout(&f.child.proof, kind, &key, Some(600));
        assert_eq!(next.generation, 2);
        assert_eq!(next.ttl_seconds, 600);
        assert_eq!(next.expires_at, f.store.now() + 600_000);
    }
}

#[test]
fn replay_returns_original_deadline_and_expired_generation_cannot_revive() {
    let f = Fixture::new();
    let original = f.checkout(&f.root.proof, SubjectKind::File, "A", Some(600));
    f.advance(90);
    assert_eq!(
        f.checkout(&f.root.proof, SubjectKind::File, "A", Some(600)),
        original
    );
    let renewed = f
        .store
        .renew_right(&f.root.proof, "renew-A", LeaseRef::from(&original))
        .unwrap();
    assert_eq!(renewed.expires_at, f.store.now() + 600_000);
    f.advance(20);
    assert_eq!(
        f.store
            .renew_right(&f.root.proof, "renew-A", LeaseRef::from(&original))
            .unwrap(),
        renewed
    );
    f.advance(600);
    let replacement = f.checkout(&f.child.proof, SubjectKind::File, "A", None);
    assert!(f
        .store
        .renew_right(&f.root.proof, "late-renew", LeaseRef::from(&original))
        .is_err());
    assert!(f
        .store
        .release_right(&f.root.proof, "late-release", LeaseRef::from(&original))
        .is_err());
    assert_eq!(replacement.generation, original.generation + 1);
}

#[test]
fn pre_and_presence_and_work_on_b_never_renew_a() {
    let f = Fixture::new();
    let a = f
        .store
        .begin_actor_edit(&f.root.proof, "pre-A", &[f.file("A")], None)
        .unwrap();
    let before = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == a.targets[0].lease_id)
        .unwrap();
    f.advance(70);
    f.store.actor_activity(&f.root.proof).unwrap();
    f.store
        .begin_actor_edit(&f.root.proof, "repeat-A", &[f.file("A")], None)
        .unwrap();
    f.store
        .complete_actor_edit(&f.root.proof, "pre-A", EditOutcome::PermissionDenied)
        .unwrap();
    let b = f
        .store
        .begin_actor_edit(&f.root.proof, "pre-B", &[f.file("B")], Some(300))
        .unwrap();
    f.advance(10);
    let completed = f
        .store
        .complete_actor_edit(&f.root.proof, "pre-B", EditOutcome::Success)
        .unwrap();
    assert_eq!(completed.renewed, vec![b.targets[0].lease_id.clone()]);
    let after = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == before.lease_id)
        .unwrap();
    assert_eq!(after, before);
    assert!(f
        .store
        .complete_actor_edit(&f.root.proof, "unknown-shell", EditOutcome::FailedExecution)
        .is_err());
    f.advance(40);
    assert_eq!(
        f.checkout(&f.child.proof, SubjectKind::File, "A", None)
            .generation,
        2
    );
}

#[test]
fn end_closes_dependent_tree_and_late_failure_cannot_restore_rights() {
    let f = Fixture::new();
    for proof in [&f.root.proof, &f.child.proof, &f.sibling.proof] {
        f.checkout(
            proof,
            SubjectKind::Resource,
            &format!("resource-{}", proof.actor_id),
            Some(3600),
        );
        f.store
            .begin_actor_edit(
                proof,
                "pre",
                &[f.file(&format!("file-{}", proof.actor_id))],
                None,
            )
            .unwrap();
    }
    let released = f
        .store
        .end_actor_session(&f.root.proof, "root-end")
        .unwrap();
    assert_eq!(released.len(), 6);
    let late = f
        .store
        .complete_actor_edit(&f.child.proof, "pre", EditOutcome::FailedExecution)
        .unwrap();
    assert!(late.renewed.is_empty());
    assert!(f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .all(|l| l.release_reason.as_deref() == Some("session_end")));
    assert!(f
        .store
        .begin_actor_edit(&f.root.proof, "after", &[f.file("new")], None)
        .is_err());
    let resumed = f.store.resume_actor(&f.root.proof, None).unwrap();
    f.checkout(&resumed.proof, SubjectKind::Queue, "new-queue", None);
    assert!(f
        .store
        .end_actor_session(&f.root.proof, "late-old-end")
        .is_err());
    assert!(f
        .store
        .right_leases()
        .unwrap()
        .last()
        .unwrap()
        .released_at
        .is_none());
}

#[test]
fn missing_file_case_aliases_share_authority_on_case_insensitive_volumes() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("case-probe")).unwrap();
    if !dir.path().join("CASE-PROBE").is_dir() {
        return;
    }
    let targets = |name: &str| {
        vec![dir
            .path()
            .join("case-probe")
            .join(name)
            .to_string_lossy()
            .into_owned()]
    };
    f.store
        .begin_actor_edit(&f.root.proof, "upper", &targets("Foo"), None)
        .unwrap();
    assert!(f
        .store
        .begin_actor_edit(&f.sibling.proof, "lower", &targets("foo"), None)
        .is_err());
}

#[test]
fn child_end_releases_its_branch_only() {
    let f = Fixture::new();
    f.checkout(&f.root.proof, SubjectKind::File, "root", None);
    f.checkout(&f.child.proof, SubjectKind::File, "child", None);
    f.checkout(&f.sibling.proof, SubjectKind::File, "sibling", None);
    assert_eq!(
        f.store
            .end_actor_session(&f.child.proof, "child-end")
            .unwrap()
            .len(),
        1
    );
    let leases = f.store.right_leases().unwrap();
    assert!(leases
        .iter()
        .find(|l| std::path::Path::new(&l.subject_key).ends_with("root"))
        .unwrap()
        .released_at
        .is_none());
    assert!(leases
        .iter()
        .find(|l| std::path::Path::new(&l.subject_key).ends_with("sibling"))
        .unwrap()
        .released_at
        .is_none());
    assert_eq!(
        f.store
            .end_actor_session(&f.root.proof, "root-end")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn idle_reminder_is_read_only_until_attributed_activity_and_never_renews_files() {
    let f = Fixture::new();
    let lease = f.checkout(&f.root.proof, SubjectKind::File, "long", Some(3600));
    f.advance(120);
    let events = f.store.events(500).unwrap().len();
    for _ in 0..2 {
        let actors = f.store.contract_actors().unwrap();
        assert!(actors
            .iter()
            .find(|a| a.actor_id == f.root.proof.actor_id)
            .unwrap()
            .reminder
            .is_some());
    }
    assert_eq!(f.store.events(500).unwrap().len(), events);
    assert!(f.store.actor_activity(&f.root.proof).unwrap().is_some());
    assert!(f.store.actor_activity(&f.root.proof).unwrap().is_none());
    let after = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == lease.lease_id)
        .unwrap();
    assert_eq!(after, lease);
}

#[test]
fn forged_proof_and_parent_substitution_fail_closed() {
    let f = Fixture::new();
    let mut forged = f.root.proof.clone();
    forged.secret = f.root.actor.name.clone();
    assert!(f
        .store
        .checkout_right(
            &forged,
            "forged",
            CheckoutRight {
                subject_kind: SubjectKind::File,
                subject_key: f.file("A"),
                ttl_seconds: None
            }
        )
        .is_err());
    let child_lease = f.checkout(&f.child.proof, SubjectKind::File, "child-file", None);
    assert!(f
        .store
        .renew_right(&f.root.proof, "parent-renew", LeaseRef::from(&child_lease))
        .is_err());
}

#[test]
fn contract_rejects_unknown_version_and_status_never_prints_proofs() {
    use another_core::{AgentEnvelope, AgentOperation};
    let f = Fixture::new();
    let status = || AgentEnvelope {
        major: 1,
        minor: 0,
        request_id: "status".into(),
        required_features: vec![],
        operation: AgentOperation::Status,
    };
    let before = f.store.events(500).unwrap().len();
    let result = f.store.agent_contract(status()).unwrap().to_string();
    assert!(!result.contains(&f.root.proof.secret));
    assert!(!result.contains("proof_hash"));
    assert_eq!(f.store.events(500).unwrap().len(), before);
    let mut unknown = status();
    unknown.major = 2;
    assert!(f.store.agent_contract(unknown).is_err());
    let mut unknown = status();
    unknown.required_features.push("unimplemented".into());
    assert!(f.store.agent_contract(unknown).is_err());
}

#[test]
fn multi_target_admission_rolls_back_when_any_target_conflicts() {
    let f = Fixture::new();
    f.checkout(&f.child.proof, SubjectKind::File, "B", None);
    assert!(f
        .store
        .begin_actor_edit(&f.root.proof, "atomic", &[f.file("A"), f.file("B")], None)
        .is_err());
    assert!(!f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .any(|l| std::path::Path::new(&l.subject_key).ends_with("A")));
}

#[test]
fn cursor_newline_call_ids_and_late_outcomes_use_the_same_core() {
    let f = Fixture::new();
    let call = "first-id\nsecond-id";
    f.store
        .begin_actor_edit(&f.child.proof, call, &[f.file("cursor-file")], None)
        .unwrap();
    f.store
        .end_actor_session(&f.root.proof, "cursor-end")
        .unwrap();
    assert!(f
        .store
        .complete_actor_edit(&f.child.proof, call, EditOutcome::Unknown)
        .unwrap()
        .renewed
        .is_empty());
}

#[test]
fn a_completion_after_resume_records_old_history_without_touching_the_new_epoch() {
    let f = Fixture::new();
    f.store
        .begin_actor_edit(&f.root.proof, "old-call", &[f.file("A")], None)
        .unwrap();
    f.store.end_actor_session(&f.root.proof, "end").unwrap();
    let resumed = f.store.resume_actor(&f.root.proof, None).unwrap();
    let fresh = f.checkout(&resumed.proof, SubjectKind::File, "A", Some(600));
    let late = f
        .store
        .complete_actor_edit(&f.root.proof, "old-call", EditOutcome::FailedExecution)
        .unwrap();
    assert!(late.renewed.is_empty());
    assert_eq!(
        f.store
            .right_leases()
            .unwrap()
            .into_iter()
            .find(|l| l.lease_id == fresh.lease_id)
            .unwrap(),
        fresh
    );
}
