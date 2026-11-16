//! Wake targets in the store: which targets are accepted, how automatic captures and manual
//! settings interact, and what each clear removes. A wake target ends up in a tmux, cmux or
//! Herdr command line, so control characters must never be stored.

use another_core::*;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("node")).unwrap();
    for name in ["bob", "carol"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    (dir, store)
}

fn target(store: &Store, agent: &str) -> Option<(WakeVia, String, bool)> {
    store
        .wake_target(agent)
        .unwrap()
        .map(|t| (t.via, t.target, t.auto))
}

#[test]
fn a_wake_target_must_be_1_to_200_printable_characters() {
    let (_dir, s) = store();
    let longest = "x".repeat(200);
    s.set_wake_target("bob", WakeVia::Tmux, &longest, false)
        .expect("200 characters are allowed");
    s.clear_wake_target("bob").unwrap();

    let too_long = "x".repeat(201);
    for bad in [
        "",
        "   ",
        too_long.as_str(),
        "%3\n",
        "%3\r",
        "\u{1b}[31m%3",
        "%3\tx",
        "%3\u{0}",
        "%3\u{7f}",
        "%3\u{9b}",
    ] {
        let err = s
            .set_wake_target("bob", WakeVia::Tmux, bad, false)
            .unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}: {err}");
        let err = s
            .set_wake_target("bob", WakeVia::Herdr, bad, true)
            .unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?} (auto): {err}");
    }
    assert_eq!(target(&s, "bob"), None);
    assert!(s.wake_targets().unwrap().is_empty());
}

#[test]
fn wake_adapter_names_are_exact() {
    for (text, via) in [
        ("tmux", WakeVia::Tmux),
        ("cmux", WakeVia::Cmux),
        ("herdr", WakeVia::Herdr),
    ] {
        assert_eq!(text.parse::<WakeVia>().unwrap(), via);
        assert_eq!(via.as_str(), text);
    }
    for bad in ["TMUX", " tmux", "screen", ""] {
        let err = bad.parse::<WakeVia>().unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}");
    }
}

/// An automatic capture never replaces a manual setting, but a manual setting replaces an
/// automatic capture, and a later capture replaces an earlier one.
#[test]
fn automatic_captures_never_replace_a_manual_target() {
    let (_dir, s) = store();
    s.set_wake_target("bob", WakeVia::Tmux, "%1", true).unwrap();
    s.set_wake_target("bob", WakeVia::Herdr, "pane-2", true)
        .unwrap();
    assert_eq!(
        target(&s, "bob"),
        Some((WakeVia::Herdr, "pane-2".into(), true))
    );

    s.set_wake_target("bob", WakeVia::Cmux, "surface:7", false)
        .unwrap();
    let ignored = s.set_wake_target("bob", WakeVia::Tmux, "%9", true).unwrap();
    assert!(ignored.is_none(), "{ignored:?}");
    assert_eq!(
        target(&s, "bob"),
        Some((WakeVia::Cmux, "surface:7".into(), false))
    );
    // The other agent is untouched throughout.
    assert_eq!(target(&s, "carol"), None);
}

#[test]
fn clearing_reports_whether_a_target_was_set() {
    let (_dir, s) = store();
    assert!(!s.clear_wake_target("bob").unwrap());
    s.set_wake_target("bob", WakeVia::Tmux, "%1", false)
        .unwrap();
    s.set_wake_target("carol", WakeVia::Tmux, "%2", false)
        .unwrap();
    assert!(s.clear_wake_target("bob").unwrap());
    assert!(!s.clear_wake_target("bob").unwrap());
    assert_eq!(target(&s, "bob"), None);
    assert_eq!(
        target(&s, "carol"),
        Some((WakeVia::Tmux, "%2".into(), false))
    );
    assert_eq!(
        s.clear_wake_target("nobody").unwrap_err().code(),
        "not_found"
    );
}

/// Clearing an automatic capture never drops a manual target, and with `only` it drops the
/// capture only while it still names that adapter and target.
#[test]
fn clearing_an_automatic_capture_spares_manual_and_newer_targets() {
    let (_dir, s) = store();
    s.set_wake_target("bob", WakeVia::Tmux, "%1", false)
        .unwrap();
    assert!(!s.clear_auto_wake_target("bob", None).unwrap());
    assert!(!s
        .clear_auto_wake_target("bob", Some((WakeVia::Tmux, "%1")))
        .unwrap());
    assert_eq!(target(&s, "bob"), Some((WakeVia::Tmux, "%1".into(), false)));

    s.set_wake_target("carol", WakeVia::Tmux, "%5", true)
        .unwrap();
    // An older session in another pane, or on another adapter, ends: the newer capture stays.
    assert!(!s
        .clear_auto_wake_target("carol", Some((WakeVia::Tmux, "%4")))
        .unwrap());
    assert!(!s
        .clear_auto_wake_target("carol", Some((WakeVia::Cmux, "%5")))
        .unwrap());
    assert_eq!(
        target(&s, "carol"),
        Some((WakeVia::Tmux, "%5".into(), true))
    );
    // The session that made the capture ends: it goes.
    assert!(s
        .clear_auto_wake_target("carol", Some((WakeVia::Tmux, "%5")))
        .unwrap());
    assert_eq!(target(&s, "carol"), None);

    s.set_wake_target("carol", WakeVia::Herdr, "p", true)
        .unwrap();
    assert!(s.clear_auto_wake_target("carol", None).unwrap());
    assert_eq!(target(&s, "carol"), None);
}
