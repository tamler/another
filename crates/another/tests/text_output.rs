//! Human (non-JSON) output never passes a terminal control sequence through. Held paths,
//! agent and project names, subjects and reasons come from other agents and other
//! machines; a file name can hold ESC or BEL.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(args)
    .current_dir(cwd)
    .env("ANOTHER_HOME", home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .output()
    .expect("run another")
}

/// ESC, BEL, CSI and the bidi overrides D75 refuses in subjects.
fn has_control(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .any(|c| c != '\n' && c != '\t' && another_core::is_unsafe_text_char(c))
}

#[test]
fn a_held_file_name_cannot_retitle_or_clear_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let run = |args: &[&str]| another(&home, &shop, args);
    assert!(run(&["init"]).status.success());
    assert!(run(&[
        "agent",
        "register",
        "a",
        "--program",
        "claude",
        "--project",
        "."
    ])
    .status
    .success());
    assert!(run(&[
        "agent",
        "register",
        "b",
        "--program",
        "codex",
        "--project",
        "."
    ])
    .status
    .success());
    // OSC 0 (set the terminal title), ended by BEL; and a right-to-left override.
    let hostile = "src/x\u{1b}]0;pwned\u{7}\u{202e}gpj.rs";
    let out = run(&["hold", "acquire", "-a", "a", hostile]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `holds` and `board` print the stored path, cleaned here. An activity summary is
    // already one shown line when it leaves the core, with spaces for those characters.
    for (args, shown) in [
        (&["holds"][..], "src/x?]0;pwned??gpj.rs"),
        (&["board"][..], "src/x?]0;pwned??gpj.rs"),
        (&["activity", "--since", "1h"][..], "src/x ]0;pwned  gpj.rs"),
    ] {
        let out = run(args);
        assert!(out.status.success(), "{args:?}");
        assert!(
            !has_control(&out.stdout),
            "{args:?} printed a control character: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(shown),
            "{args:?}: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // A conflict report goes to stdout too, and a release's note to stderr: this one
    // repeats the path it was given.
    let out = run(&["hold", "check", "-a", "b", hostile]);
    assert!(
        !has_control(&out.stdout),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = run(&["hold", "release", "-a", "b", hostile]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!has_control(&out.stderr), "{err:?}");
    // The note shows the path with the platform's separator.
    assert!(err.contains("x?]0;pwned??gpj.rs"), "{err:?}");

    // --json keeps the exact text: JSON escapes the C0 controls (ESC, BEL); the bidi
    // override stays raw UTF-8 (D97), and the value decodes to what was held.
    let out = run(&["--json", "holds"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.to_string().contains("\\u001b]0;pwned"), "{v}");
    assert!(String::from_utf8_lossy(&out.stdout).contains('\u{202e}'));
    assert_eq!(v[0]["pattern"].as_str(), Some(hostile), "{v}");
}

/// A spoke's home under `dir`, paired with a fake hub that answers every request with a
/// 500 whose message holds an OSC title sequence and a clear-screen.
fn spoke_of_a_hostile_hub(dir: &Path) -> std::path::PathBuf {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", hub.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in hub.incoming().take(20) {
            let Ok(mut s) = stream else { continue };
            // Read the whole request before answering: closing on an unread body resets
            // the connection, and the client then reports that instead of the 500.
            let mut req = Vec::new();
            let mut buf = [0u8; 8192];
            let mut want = None;
            while want.is_none_or(|n| req.len() < n) {
                let Ok(got @ 1..) = s.read(&mut buf) else {
                    break;
                };
                req.extend_from_slice(&buf[..got]);
                if want.is_none() {
                    if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        want = Some(end + 4 + len);
                    }
                }
            }
            let body ="{\"error\":{\"code\":\"internal\",\"message\":\"hub says \u{1b}]0;pwned\u{7} \u{1b}[2J done\"}}";
            let _ = write!(
                s,
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });

    // A spoke paired with a hub at `url`, through the store (no hub process).
    let spoke_home = dir.join("spoke");
    let hub_store = another_core::Store::open(&dir.join("hub")).unwrap();
    hub_store.set_node_name("hub").unwrap();
    let spoke = another_core::Store::open(&spoke_home).unwrap();
    spoke.set_node_name("spoke").unwrap();
    let code = hub_store
        .create_invite(&url, &another_core::Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub_store.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
    spoke_home
}

/// Q19b: a failed command ends with `another: <message>` on stderr, and the message can
/// quote what another machine wrote, here a hub's error body. One `another sync` fails
/// through that path. It starts no long-running process, so it runs on every platform.
#[test]
fn a_hub_error_in_a_failed_command_cannot_retitle_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let spoke_home = spoke_of_a_hostile_hub(dir.path());
    let once = another(&spoke_home, dir.path(), &["sync"]);
    let said = String::from_utf8_lossy(&once.stderr);
    assert!(!once.status.success(), "{said:?}");
    assert!(said.starts_with("another: "), "{said:?}");
    // The hub's error body is not signed: it is quoted as one line, each control character
    // a space, before the message reaches the terminal.
    assert!(said.contains("hub says  ]0;pwned   [2J done"), "{said:?}");
    assert!(!has_control(&once.stderr), "{said:?}");
}

/// Review R1 of #176 (P2): `sync --watch` keeps going after a failed round and prints the
/// error itself, outside `main`'s error path. The error can quote the hub's reply, which
/// another machine wrote; the watch line must carry it cleaned.
#[cfg(unix)]
#[test]
fn a_hub_error_in_sync_watch_cannot_retitle_the_terminal() {
    use std::io::{BufRead, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let spoke_home = spoke_of_a_hostile_hub(dir.path());

    let mut child = {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(["sync", "--watch"])
    .current_dir(dir.path())
    .env("ANOTHER_HOME", &spoke_home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    let mut got = String::new();
    while err.read_line(&mut line).unwrap_or(0) > 0 {
        if line.contains("sync failed") {
            got = line.clone();
            break;
        }
        line.clear();
    }
    // Stop the one process this test started, by its own id, with SIGTERM.
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();

    assert!(got.contains("hub says"), "no watch error line: {got:?}");
    assert!(!has_control(got.trim_end().as_bytes()), "{got:?}");
    assert!(got.contains(" ]0;pwned "), "{got:?}");
}

/// Q26b, 4.8 (D97): a spoke repeats its hub's `sync.skipped` markers after every sync, in
/// `another needs` and in the activity feed, and a hub chose every text in them. Here the
/// hub's marker has an id, a kind, an actor and a short name at their full lengths, each
/// holding escape, control, text-direction and invisible characters. None of those reaches
/// the terminal, and each value is cut at its shown length: id 40, kind 80, actor 80,
/// short name 120.
#[test]
fn a_peers_marker_text_is_shown_escaped_and_bounded() {
    use another_core::crypto;
    use std::net::SocketAddr;

    let dir = tempfile::tempdir().unwrap();
    let hub_home = dir.path().join("hub");
    let spoke_home = dir.path().join("spoke");
    let hub = another_core::Store::open(&hub_home).unwrap();
    hub.set_node_name("hub").unwrap();
    let spoke = another_core::Store::open(&spoke_home).unwrap();
    spoke.set_node_name("spoke").unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = another::fed::router(hub.clone());
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let code = hub
        .create_invite(&url, &another_core::Perm::ALL, 3600)
        .unwrap();
    another::fed::join(&spoke, &code).unwrap();

    let pad = |head: &str, fill: char, chars: usize| -> String {
        let mut text = head.to_string();
        while text.chars().count() < chars {
            text.push(fill);
        }
        text
    };
    let event_id = pad("01\u{1b}]0;pwned\u{7}", 'I', 128);
    let kind = pad("agent\u{202e}.registered\u{200b}", 'K', 80);
    let actor = pad("al\u{2066}ice\u{0}", 'A', 128);
    let about = pad("agent \u{1b}[2Jwor\u{202e}dy", 'B', 120);
    let hub_id = hub.node_id().to_string();
    let payload = serde_json::json!({
        "event_id": event_id,
        "event_node": hub_id,
        "event_kind": kind,
        "event_actor": actor,
        "bytes": 9_437_500,
        "direction": "pull",
        "about": about,
        "route": "directory",
        "route_nodes": [],
    })
    .to_string();
    let db = rusqlite::Connection::open(hub_home.join(another_core::DB_FILE)).unwrap();
    let secret: String = db
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let ts = hub.now();
    let sig = crypto::sign(
        &key,
        &crypto::event_message(
            "01HOSTILEMARKER",
            &hub_id,
            ts,
            "system",
            "sync.skipped",
            &payload,
        ),
    );
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
         VALUES ('01HOSTILEMARKER', ?1, ?2, 'system', 'sync.skipped', ?3, NULL, ?4)",
        rusqlite::params![hub_id, ts, payload, sig],
    )
    .unwrap();

    let synced = another(&spoke_home, dir.path(), &["sync"]);
    let needs = another(&spoke_home, dir.path(), &["needs"]);
    let feed = another(&spoke_home, dir.path(), &["activity", "--kinds", "sync"]);
    rt.shutdown_timeout(std::time::Duration::from_secs(5));

    // Each unsafe character became a space, and each value was cut with `…`.
    let id = format!("01 ]0;pwned {}…", "I".repeat(27));
    let kind = format!("agent .registered {}", "K".repeat(62));
    let actor = format!("al ice {}…", "A".repeat(72));
    let about = format!("agent  [2Jwor dy{}", "B".repeat(104));
    assert_eq!(id.chars().count(), 40);
    assert_eq!(kind.chars().count(), 80);
    assert_eq!(actor.chars().count(), 80);
    assert_eq!(about.chars().count(), 120);

    for (what, run) in [("sync", &synced), ("needs", &needs), ("activity", &feed)] {
        let text = String::from_utf8_lossy(&run.stdout);
        assert!(run.status.success(), "{what}: {:?}", run.stderr);
        assert!(!has_control(&run.stdout), "{what}: {text:?}");
        for raw in [
            '\u{1b}', '\u{7}', '\u{0}', '\u{202e}', '\u{2066}', '\u{200b}',
        ] {
            assert!(!text.contains(raw), "{what}: {raw:?} in {text:?}");
        }
        assert!(text.contains(&id), "{what}: {text}");
        assert!(text.contains(&kind), "{what}: {text}");
        // Nothing past a cut is printed.
        assert!(!text.contains(&"I".repeat(28)), "{what}: {text}");
        assert!(!text.contains(&"A".repeat(73)), "{what}: {text}");
        assert!(!text.contains(&"K".repeat(63)), "{what}: {text}");
        assert!(!text.contains(&"B".repeat(105)), "{what}: {text}");
    }
    let text = String::from_utf8_lossy(&synced.stdout);
    assert!(
        text.contains(&format!(
            "\nskipped {id} ({kind}, 9437500 bytes, written by {actor}): it is in node hub's \
             log and is larger than a spoke reads in one pull answer (33554432 bytes), so node \
             hub did not send it; the events after it were sent\n  about: {about}\n  note: the \
             event id in this report is not an id this software makes, so <event-id> is left \
             for you to fill\n  missing on this node: this event and whatever it changed on \
             other nodes; "
        )),
        "{text}"
    );
    // The id is not one this software makes, so it is in no printed command.
    assert!(
        text.contains("\n  then, on node hub: another sync ack <event-id> --owner\n"),
        "{text}"
    );
    assert!(!text.contains("sync ack 01"), "{text}");
    let text = String::from_utf8_lossy(&needs.stdout);
    assert!(
        text.contains(&format!(
            "  skipped event  {id} ({kind}, 9437500 bytes) in node hub's log was not sent to \
             this node: larger than one pull answer\n"
        )),
        "{text}"
    );
    let text = String::from_utf8_lossy(&feed.stdout);
    assert!(
        text.contains(&format!(
            "[hub] skipped event {id} ({kind}, 9437500 bytes, written by {actor}): too large \
             to send; {about}"
        )),
        "{text}"
    );
}
