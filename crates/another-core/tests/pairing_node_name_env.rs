//! Q27 (5.4): `ANOTHER_NODE_NAME` names a new home only when a rename would accept the name.
//! This is its own test binary, with one test, because it sets a process-wide variable.

use another_core::*;

#[test]
fn an_invalid_another_node_name_falls_back_to_local() {
    let dir = tempfile::tempdir().unwrap();
    let open = |home: &str| {
        Store::open_with_clock(&dir.path().join(home), Clock::manual(1_800_000_000_000).0).unwrap()
    };

    for (n, bad) in [
        "two words",
        "evil@hub",
        "",
        "esc\u{1b}[2J",
        "a-name-that-is-longer-than-forty-characters-in-all",
    ]
    .into_iter()
    .enumerate()
    {
        std::env::set_var("ANOTHER_NODE_NAME", bad);
        let store = open(&format!("bad{n}"));
        assert_eq!(store.node_entry().unwrap().display_name, "local", "{bad:?}");
        // Like any node that is not named yet, it cannot pair until it is.
        let err = store
            .create_invite("https://hub.example", &Perm::ALL, 3600)
            .unwrap_err();
        assert_eq!(err.code(), "invalid", "{err}");
    }

    std::env::set_var("ANOTHER_NODE_NAME", "studio");
    let store = open("good");
    assert_eq!(store.node_entry().unwrap().display_name, "studio");
    store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();

    std::env::remove_var("ANOTHER_NODE_NAME");
    assert_eq!(open("unset").node_entry().unwrap().display_name, "local");
}
