//! FINDINGS-203 note 2: Herdr takes the wake target as a positional argument
//! (`herdr agent prompt <target> <message>`), so a target that starts with `-` would be read
//! as an option. No real tmux pane, cmux surface or Herdr pane starts with `-`, so such a
//! target is refused when it is stored and again when a wake is planned (a `--target`
//! override, a target stored earlier, or Herdr's default of the agent's name).
//! FINDINGS-203 note 1: the 200 limit counts characters, as its message says.

#[path = "common/transports.rs"]
mod transports;

use another::wake::{wake, WakeOptions};
use another_core::*;
use transports::via;

/// Herdr, and the two command transports of the documented example.
fn every_transport() -> [WakeVia; 3] {
    [via("tmux"), via("cmux"), WakeVia::herdr()]
}

fn store(names: &[&str]) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    for name in names {
        store
            .register_agent(RegisterAgent {
                name: (*name).into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    (dir, store)
}

fn plan(
    store: &Store,
    agent: &str,
    via: WakeVia,
    target: Option<&str>,
) -> std::result::Result<Vec<Vec<String>>, String> {
    wake(
        store,
        agent,
        &WakeOptions {
            via: Some(via),
            target: target.map(String::from),
            force: true,
            dry_run: true,
            config: Some(transports::example_config()),
            ..Default::default()
        },
    )
    .map(|p| p.commands)
    .map_err(|e| match e.downcast_ref::<Error>() {
        Some(core) => core.code().to_string(),
        None => format!("not a core error: {e}"),
    })
}

#[test]
fn a_target_that_starts_with_a_dash_is_not_stored() {
    let (_dir, s) = store(&["bob"]);
    for via in every_transport() {
        for bad in ["-h", "--help", "-", "--"] {
            for auto in [false, true] {
                let err = s.set_wake_target("bob", &via, bad, auto).unwrap_err();
                assert_eq!(err.code(), "invalid", "{bad:?} via {via:?}: {err}");
            }
        }
    }
    assert!(s.wake_targets().unwrap().is_empty());
    // A dash elsewhere is fine.
    s.set_wake_target("bob", &WakeVia::herdr(), "pane-1", false)
        .unwrap();
    s.set_wake_target("bob", &via("tmux"), "work:1.0-x", false)
        .unwrap();
}

/// Review R1 of Q9, finding 4: "printable" also rules out what cannot be seen or changes
/// the order text is shown in. Such a target is not stored, makes no session row, and a
/// wake through a command transport refuses it as `--target`.
#[test]
fn a_target_with_an_invisible_or_bidirectional_character_is_refused() {
    let (_dir, s) = store(&["bob"]);
    let hidden = [
        '\u{200B}',
        '\u{200C}',
        '\u{200D}',
        '\u{200E}',
        '\u{200F}',
        '\u{202A}',
        '\u{202B}',
        '\u{202C}',
        '\u{202D}',
        '\u{202E}',
        '\u{2060}',
        '\u{2061}',
        '\u{2066}',
        '\u{2067}',
        '\u{2068}',
        '\u{2069}',
        '\u{FEFF}',
        '\u{2028}',
        '\u{2029}',
        // Unicode tag characters: not seen by a person, read as letters by a model (D96).
        '\u{E0000}',
        '\u{E0001}',
        '\u{E0041}',
        '\u{E007F}',
    ];
    for c in hidden {
        for bad in [format!("p{c}x"), format!("{c}p"), format!("p{c}")] {
            for via in every_transport() {
                let err = s.set_wake_target("bob", &via, &bad, false).unwrap_err();
                assert_eq!(err.code(), "invalid", "{bad:?}");
            }
            let stamped = s.stamp_transport_session("bob", "s1", "tmux", &bad, "claude", "/w");
            assert_eq!(stamped.unwrap_err().code(), "invalid", "{bad:?}");
            for name in ["tmux", "cmux"] {
                let err = plan(&s, "bob", via(name), Some(&bad)).unwrap_err();
                assert_eq!(err, "invalid", "{name} {bad:?}");
            }
        }
    }
    assert!(s.wake_targets().unwrap().is_empty());
    assert!(s.transport_sessions().unwrap().is_empty());
    // Ordinary text outside ASCII is still a target.
    s.set_wake_target("bob", &via("tmux"), "fen\u{ea}tre:1.0", false)
        .unwrap();
}

#[test]
fn the_200_limit_counts_characters_not_bytes() {
    let (_dir, s) = store(&["bob"]);
    let two_byte = "\u{e9}".repeat(200);
    assert_eq!(two_byte.len(), 400);
    s.set_wake_target("bob", &WakeVia::herdr(), &two_byte, false)
        .expect("200 characters are allowed, whatever their byte length");
    let err = s
        .set_wake_target("bob", &WakeVia::herdr(), &"\u{e9}".repeat(201), false)
        .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
}

#[test]
fn a_wake_never_passes_a_dash_target_to_an_adapter() {
    let (_dir, s) = store(&["bob"]);
    for via in every_transport() {
        let err = plan(&s, "bob", via.clone(), Some("--help")).unwrap_err();
        assert_eq!(err, "invalid", "{via:?}");
    }
    // Controls: an ordinary target plans as before.
    let ok = plan(&s, "bob", WakeVia::herdr(), Some("bob")).unwrap();
    assert_eq!(&ok[0][1..4], ["agent", "prompt", "bob"]);
    let ok = plan(&s, "bob", via("tmux"), Some("%3")).unwrap();
    assert_eq!(&ok[0][..4], ["tmux", "send-keys", "-t", "%3"]);
}

/// Herdr's default target is the agent's name, and a name may start with `-`.
#[test]
fn herdrs_default_target_from_a_dash_name_is_refused() {
    let (_dir, s) = store(&["-x", "bob"]);
    let err = plan(&s, "-x", WakeVia::herdr(), None).unwrap_err();
    assert_eq!(err, "invalid");
    let ok = plan(&s, "bob", WakeVia::herdr(), None).unwrap();
    assert_eq!(ok[0][3], "bob");
}

/// REVIEW-R1 of #206: `herdr agent get <target>` and `herdr agent prompt <target>` take the
/// target positionally, and the transport is also called without going through `wake()`.
/// A target stored before the check must not reach it; an automatic one is pruned, a manual
/// one kept, and a session row cannot be made with one.
/// The fake Herdr is a `/bin/sh` script, so this test is Unix only; the refusals above run
/// everywhere.
#[cfg(unix)]
#[test]
fn stored_dash_targets_never_reach_the_herdr_probe() {
    use another::transport::herdr::HerdrTransport;
    use another::transport::{State, Transport};
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("herdr.log");
    let fake = dir.path().join("herdr");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrTransport::new(fake.to_string_lossy());

    let home = dir.path().join("home");
    let s = Store::open(&home).unwrap();
    let mut ids = Vec::new();
    for name in ["bob", "carol"] {
        ids.push(
            s.register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .agent_id,
        );
    }
    let db = rusqlite::Connection::open(home.join(DB_FILE)).unwrap();
    for (id, target, auto) in [(&ids[0], "-x", 1), (&ids[1], "--help", 0)] {
        db.execute(
            "INSERT INTO wake_targets (agent_id, via, target, auto, updated_at)
             VALUES (?1, 'herdr', ?2, ?3, 0)",
            rusqlite::params![id, target, auto],
        )
        .unwrap();
    }

    assert_eq!(herdr.status("--help", None).state, State::NoAnswer);
    assert_eq!(
        herdr.wake("--help", "hi", None).outcome,
        WakeOutcome::Undelivered
    );
    let stamped = s.stamp_transport_session("bob", "s1", "herdr", "-x", "claude", "/work/app");
    assert_eq!(stamped.unwrap_err().code(), "invalid");
    assert!(s.transport_sessions().unwrap().is_empty());
    let rows = another::wake::prune(&s, false).unwrap();
    let bob = rows.iter().find(|r| r.agent == "bob").unwrap();
    assert!(bob.pruned, "{bob:?}");
    assert!(bob.reason.contains('-'), "{bob:?}");
    let carol = rows.iter().find(|r| r.agent == "carol").unwrap();
    assert!(!carol.pruned, "{carol:?}");
    assert_eq!(s.wake_target("carol").unwrap().unwrap().target, "--help");
    assert!(s.wake_target("bob").unwrap().is_none());

    let seen = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        seen.is_empty(),
        "herdr was run with a dash target: {seen:?}"
    );
}
