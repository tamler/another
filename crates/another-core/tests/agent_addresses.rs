//! Agent names and addresses: which names register, and how `name`, `name@project` (slug or
//! absolute path), `name@node` and malformed addresses resolve. Mail recipients and every
//! acting tool go through this resolution, so a wrong match sends mail to the wrong agent.

use std::path::Path;

use another_core::*;

fn store(dir: &Path, name: &str) -> Store {
    let store = Store::open(&dir.join(name)).unwrap().unguarded_repair();
    store.set_node_name(name).unwrap();
    store
}

fn register(store: &Store, name: &str, project: Option<&Path>) -> Result<Agent> {
    store.register_agent(RegisterAgent {
        name: name.into(),
        program: Program::Claude,
        project: project.map(|p| p.to_string_lossy().into_owned()),
        model: None,
        description: None,
    })
}

#[test]
fn agent_names_are_1_to_64_ascii_letters_digits_dash_underscore_or_dot() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    let max = "a".repeat(64);
    for good in ["a", "A1", "a.b-c_d", max.as_str()] {
        let agent = register(&s, good, None).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        assert_eq!(agent.name, good);
    }
    let too_long = "a".repeat(65);
    for bad in [
        "",
        too_long.as_str(),
        "a b",
        "a@b",
        "a/b",
        "a\\b",
        "a:b",
        "caf\u{e9}",
        "a\nb",
    ] {
        let err = register(&s, bad, None).unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}: {err}");
    }
    // Nothing was registered for the refused names.
    assert_eq!(s.agents().unwrap().len(), 4);
}

/// Names are case-sensitive: `bob` and `Bob` are two agents, and an address must match the
/// case exactly (FINDINGS-199 note 2 asked for this to be pinned either way).
#[test]
fn agent_names_are_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    let lower = register(&s, "bob", None).unwrap();
    let upper = register(&s, "Bob", None).unwrap();
    assert_ne!(lower.agent_id, upper.agent_id);
    assert_eq!(s.agent("bob").unwrap().agent_id, lower.agent_id);
    assert_eq!(s.agent("Bob").unwrap().agent_id, upper.agent_id);
    assert_eq!(s.agent("BOB").unwrap_err().code(), "not_found");
}

#[test]
fn an_address_with_more_than_three_parts_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    register(&s, "alice", None).unwrap();
    for bad in ["alice@a@b@c", "alice@@@"] {
        let err = s.agent(bad).unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}: {err}");
    }
    let sent = s.send_message(SendMessage {
        from: "alice".into(),
        to: vec!["alice@a@b@c".into()],
        subject: "s".into(),
        body: "b".into(),
        ..Default::default()
    });
    assert_eq!(sent.unwrap_err().code(), "invalid");
}

/// One name in two projects: the bare name is ambiguous and the error lists both addresses;
/// `name@slug` and `name@/absolute/path` each pick one; surrounding whitespace is ignored.
#[test]
fn a_name_in_two_projects_resolves_by_slug_or_path_and_is_ambiguous_bare() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    let web = dir.path().join("work").join("web");
    let api = dir.path().join("work").join("api");
    let a_web = register(&s, "alice", Some(&web)).unwrap();
    let a_api = register(&s, "alice", Some(&api)).unwrap();
    assert_ne!(a_web.agent_id, a_api.agent_id);
    assert_eq!(a_web.address, "alice@web");
    assert_eq!(a_api.address, "alice@api");

    let err = s.agent("alice").unwrap_err();
    assert_eq!(err.code(), "ambiguous", "{err}");
    let text = err.to_string();
    assert!(
        text.contains("alice@web") && text.contains("alice@api"),
        "{text}"
    );

    assert_eq!(s.agent("alice@web").unwrap().agent_id, a_web.agent_id);
    assert_eq!(s.agent(" alice@api ").unwrap().agent_id, a_api.agent_id);
    let web_path = s.project(&web.to_string_lossy()).unwrap().abs_path;
    assert_eq!(
        s.agent(&format!("alice@{web_path}")).unwrap().agent_id,
        a_web.agent_id
    );
    // An unknown project is not found (it is tried as a node name, and no node has it).
    assert_eq!(s.agent("alice@nowhere").unwrap_err().code(), "not_found");
    assert_eq!(s.agent("bob").unwrap_err().code(), "not_found");
}

/// Two projects whose directories share a last component get distinct slugs, so their
/// agents get distinct addresses.
#[test]
fn projects_with_the_same_directory_name_get_distinct_slugs() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    let one = register(&s, "alice", Some(&dir.path().join("a").join("app"))).unwrap();
    let two = register(&s, "alice", Some(&dir.path().join("b").join("app"))).unwrap();
    assert_eq!(one.address, "alice@app");
    assert_ne!(two.address, one.address);
    assert!(two.address.starts_with("alice@app-"), "{}", two.address);
    assert_eq!(s.agent(&two.address).unwrap().agent_id, two.agent_id);
    assert_eq!(s.agent(&one.address).unwrap().agent_id, one.agent_id);
}

/// `name@node` reaches a project-less agent on another node once its registration has
/// synced, by the node's display name or its id; the same name on this node stays separate.
#[test]
fn name_at_node_reaches_a_projectless_agent_on_a_paired_node() {
    let dir = tempfile::tempdir().unwrap();
    let hub = store(dir.path(), "hub");
    let spoke = store(dir.path(), "spoke");
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();

    let remote = register(&spoke, "bob", None).unwrap();
    let local = register(&hub, "bob", None).unwrap();
    register(&hub, "carol", None).unwrap();
    let (events, _) = spoke.outbound_events(0, 10_000).unwrap();
    let report = hub.ingest(spoke.node_id(), &events, &[]).unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");

    assert_eq!(hub.agent("bob").unwrap().agent_id, local.agent_id);
    let by_name = hub.agent("bob@spoke").unwrap();
    assert_eq!(by_name.agent_id, remote.agent_id);
    assert_eq!(
        hub.agent(&format!("bob@{}", spoke.node_id()))
            .unwrap()
            .agent_id,
        remote.agent_id
    );
    assert_eq!(hub.agent("bob@@spoke").unwrap().agent_id, remote.agent_id);
    assert_eq!(hub.agent("nobody@spoke").unwrap_err().code(), "not_found");

    // Mail by that address goes to the remote bob, not the local one.
    let sent = hub
        .send_message(SendMessage {
            from: "carol".into(),
            to: vec!["bob@spoke".into()],
            subject: "hello".into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(sent.to, vec![by_name.address.clone()]);
    assert_ne!(by_name.address, local.address);
}
