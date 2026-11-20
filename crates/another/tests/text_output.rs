//! Human (non-JSON) output never passes a terminal control sequence through. Held paths,
//! agent and project names, subjects and reasons come from other agents and other
//! machines; a file name can hold ESC or BEL.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
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
    assert!(err.contains("src/x?]0;pwned??gpj.rs"), "{err:?}");

    // --json keeps the exact text: JSON escapes the C0 controls (ESC, BEL); the bidi
    // override stays raw UTF-8 (D97), and the value decodes to what was held.
    let out = run(&["--json", "holds"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.to_string().contains("\\u001b]0;pwned"), "{v}");
    assert!(String::from_utf8_lossy(&out.stdout).contains('\u{202e}'));
    assert_eq!(v[0]["pattern"].as_str(), Some(hostile), "{v}");
}

/// Review R1 of #176 (P2): `sync --watch` keeps going after a failed round and prints the
/// error itself, outside `main`'s error path. The error can quote the hub's reply, which
/// another machine wrote. A fake hub answers 500 with an OSC title sequence in its
/// message; the watch line must carry it cleaned.
#[cfg(unix)]
#[test]
fn a_hub_error_in_sync_watch_cannot_retitle_the_terminal() {
    use std::io::{BufRead, BufReader, Read, Write};
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
    let dir = tempfile::tempdir().unwrap();
    let spoke_home = dir.path().join("spoke");
    {
        let hub_store = another_core::Store::open(&dir.path().join("hub")).unwrap();
        hub_store.set_node_name("hub").unwrap();
        let spoke = another_core::Store::open(&spoke_home).unwrap();
        spoke.set_node_name("spoke").unwrap();
        let code = hub_store
            .create_invite(&url, &another_core::Perm::ALL, 3600)
            .unwrap();
        let (invite, req) = spoke.prepare_join(&code).unwrap();
        let resp = hub_store.accept_pairing(&req).unwrap();
        spoke.complete_join(&invite, &resp).unwrap();
    }

    // Q19b: one round without `--watch` fails through `main`'s error path, which prints the
    // same reply.
    let once = another(&spoke_home, dir.path(), &["sync"]);
    let said = String::from_utf8_lossy(&once.stderr);
    assert!(!once.status.success(), "{said:?}");
    assert!(said.contains("hub says ?]0;pwned? ?[2J done"), "{said:?}");
    assert!(!has_control(&once.stderr), "{said:?}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
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
    assert!(got.contains("?]0;pwned?"), "{got:?}");
}
