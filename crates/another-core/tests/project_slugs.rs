//! Project slugs: directories that share a name get numbered slugs, the slug a project gets
//! stays its own, and a name with nothing usable in it still gets a slug. Agents are
//! addressed by these slugs, so a collision must never make two projects share one.

use another_core::*;

fn store() -> Store {
    Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap()
}

fn path(dir: &tempfile::TempDir, rel: &[&str]) -> String {
    let mut p = dir.path().to_path_buf();
    for part in rel {
        p.push(part);
    }
    p.to_string_lossy().into_owned()
}

#[test]
fn directories_with_one_name_get_numbered_slugs_that_stay_put() {
    let dir = tempfile::tempdir().unwrap();
    let s = store();
    let a = path(&dir, &["a", "shop"]);
    let b = path(&dir, &["b", "shop"]);
    let c = path(&dir, &["c", "Shop"]);
    let slugs: Vec<String> = [&a, &b, &c]
        .iter()
        .map(|p| s.ensure_project("t", p).unwrap().slug)
        .collect();
    assert_eq!(slugs, ["shop", "shop-2", "shop-3"]);

    // Ensuring a known directory again returns its slug and adds no project.
    let again = s.ensure_project("t", &b).unwrap();
    assert_eq!(again.slug, "shop-2");
    assert_eq!(s.projects().unwrap().len(), 3);

    // Each slug resolves to its own directory.
    for (slug, p) in [("shop", &a), ("shop-2", &b), ("shop-3", &c)] {
        let found = s.project(slug).unwrap();
        assert_eq!(found.slug, slug);
        assert_eq!(found.abs_path, s.project(p).unwrap().abs_path, "{slug}");
    }
    assert_ne!(
        s.project("shop").unwrap().abs_path,
        s.project("shop-2").unwrap().abs_path
    );
}

#[test]
fn a_directory_named_like_a_numbered_slug_is_numbered_past_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = store();
    // A directory literally named shop-2 takes that slug first.
    let literal = s
        .ensure_project("t", &path(&dir, &["x", "shop-2"]))
        .unwrap();
    assert_eq!(literal.slug, "shop-2");
    let first = s.ensure_project("t", &path(&dir, &["a", "shop"])).unwrap();
    let second = s.ensure_project("t", &path(&dir, &["b", "shop"])).unwrap();
    assert_eq!(first.slug, "shop");
    assert_eq!(
        second.slug, "shop-3",
        "shop-2 is taken by the literal directory"
    );
}

#[test]
fn slugs_are_lowercase_and_a_name_with_nothing_usable_becomes_project() {
    let dir = tempfile::tempdir().unwrap();
    let s = store();
    let cases = [
        (vec!["My App"], "my-app"),
        (vec!["y", "my app"], "my-app-2"),
        (vec!["Rust_Lib.v2"], "rust_lib-v2"),
        (vec!["+++"], "project"),
        (vec!["z", "---"], "project-2"),
    ];
    for (rel, want) in cases {
        let p = s.ensure_project("t", &path(&dir, &rel)).unwrap();
        assert_eq!(p.slug, want, "{rel:?}");
    }
}

#[test]
fn agents_in_same_named_projects_get_distinct_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let s = store();
    let a = path(&dir, &["a", "shop"]);
    let b = path(&dir, &["b", "shop"]);
    let register = |project: &str| {
        s.register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: Some(project.into()),
            model: None,
            description: None,
        })
        .unwrap()
    };
    let first = register(&a);
    let second = register(&b);
    assert_eq!(first.address, "alice@shop");
    assert_eq!(second.address, "alice@shop-2");
    assert_ne!(first.agent_id, second.agent_id);

    // Mail to each address reaches only that agent.
    s.register_agent(RegisterAgent {
        name: "bob".into(),
        program: Program::Claude,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    s.send_message(SendMessage {
        from: "bob".into(),
        to: vec!["alice@shop-2".into()],
        subject: "for the second shop".into(),
        body: "hello".into(),
        ..Default::default()
    })
    .unwrap();
    let unread = |who: &str| s.inbox(who, InboxQuery::default()).unwrap().unread;
    assert_eq!(unread("alice@shop-2"), 1);
    assert_eq!(unread("alice@shop"), 0);
}
