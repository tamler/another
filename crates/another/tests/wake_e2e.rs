//! `another wake` end to end, through the real binary, for every transport: tmux, cmux
//! (`send` to a surface and `notify`) and Herdr. The fake adapters record each argument
//! byte for byte, so a control character that reached an adapter is seen exactly as it
//! would have been typed. One test drives a real tmux server, so the line discipline of a
//! real terminal is in the loop as well.
//!
//! Nothing here signals or force-stops a process: the fake adapters exit at once, and the
//! real tmux server runs on a private socket and is shut down with `tmux kill-server`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Records `argv` under `calls/<n>/<i>`, one file per argument, so no byte is lost to a
/// line-based log. Fails (exit 1, nothing typed) when `dead/<target>` exists; the target
/// is the third argument for every adapter form Another uses.
const FAKE: &str = r#"#!/bin/sh
dir='@DIR@'
n=$(ls "$dir/calls" | wc -l | tr -d ' ')
mkdir "$dir/calls/$n"
printf '%s' '@TOOL@' > "$dir/calls/$n/0"
i=1
for a in "$@"; do printf '%s' "$a" > "$dir/calls/$n/$i"; i=$((i+1)); done
if [ -n "$3" ] && [ -f "$dir/dead/$3" ]; then echo "@TOOL@: can't find pane $3" >&2; exit 1; fi
if [ '@TOOL@' = herdr ]; then
  printf '{"id":"cli:agent:prompt","result":{"agent":{"agent_status":"working"},"type":"agent_prompted"}}\n'
fi
exit 0
"#;

/// Everything a hostile sender could put in a line to press keys or hide text: CR and LF
/// (submit), ESC sequences (clear screen, move the cursor), BEL, backspace, DEL, NAK
/// (kill line), tab, bidi overrides and isolates, and the two characters `\n` that cmux
/// unescapes into a line break.
const HOSTILE: &str =
    "Lunch?\r\x1b[2J\x07\x08\x7f\x15\u{202e}evil\u{2066}\t \\n then\nrm -rf ~/src\x1b]0;x\x07";

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        for sub in ["bin", "calls", "dead"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        for tool in ["tmux", "cmux", "herdr"] {
            let script = bin.join(tool);
            std::fs::write(
                &script,
                FAKE.replace("@DIR@", &root.to_string_lossy())
                    .replace("@TOOL@", tool),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = Env {
            home: root.join("home"),
            root,
            bin,
            _dir: dir,
        };
        env.json(&["agent", "register", "mallory", "--program", "other"]);
        env.json(&["agent", "register", "codex", "--program", "codex"]);
        env.json(&["agent", "register", "claude", "--program", "claude"]);
        env
    }

    /// The binary with the fake adapters first on PATH (or `path` instead, for real tmux).
    fn cmd_with_path(&self, args: &[&str], path: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("ANOTHER_LOG", "off")
            .env_remove("ANOTHER_AGENT")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TERM_PROGRAM")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::null());
        c
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        self.cmd_with_path(args, &path)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all).output().unwrap();
        assert!(
            out.status.success(),
            "another {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn send(&self, to: &str, subject: &str) -> String {
        self.json(&[
            "send",
            "--from",
            "mallory",
            "--to",
            to,
            "--subject",
            subject,
            "--body",
            "private body",
        ])["message_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Store `subject` as an older build or peer could have: straight into SQLite, past the
    /// check `send` now makes.
    fn store_hostile_subject(&self, message_id: &str, subject: &str) {
        rusqlite::Connection::open(self.home.join(another_core::DB_FILE))
            .unwrap()
            .execute(
                "UPDATE messages SET subject = ?1 WHERE message_id = ?2",
                rusqlite::params![subject, message_id],
            )
            .unwrap();
    }

    fn kill_pane(&self, target: &str) {
        std::fs::write(self.root.join("dead").join(target), "").unwrap();
    }

    /// Every adapter call so far, in order, each as its exact argv.
    fn calls(&self) -> Vec<Vec<String>> {
        let mut calls = BTreeMap::new();
        for entry in std::fs::read_dir(self.root.join("calls")).unwrap() {
            let entry = entry.unwrap();
            let n: usize = entry.file_name().to_str().unwrap().parse().unwrap();
            let mut args = BTreeMap::new();
            for arg in std::fs::read_dir(entry.path()).unwrap() {
                let arg = arg.unwrap();
                let i: usize = arg.file_name().to_str().unwrap().parse().unwrap();
                let bytes = std::fs::read(arg.path()).unwrap();
                args.insert(i, String::from_utf8(bytes).expect("adapter argv is UTF-8"));
            }
            calls.insert(n, args.into_values().collect::<Vec<_>>());
        }
        calls.into_values().collect()
    }
}

/// Characters that must never reach a terminal from a wake.
fn unsafe_chars(s: &str) -> Vec<char> {
    s.chars()
        .filter(|&c| another_core::is_unsafe_text_char(c))
        .collect()
}

fn assert_clean(call: &[String]) {
    for arg in call {
        let bad = unsafe_chars(arg);
        assert!(bad.is_empty(), "{call:?} carries {bad:?}");
    }
}

/// The shape each transport must have: tmux types the text literally (`-l`) and presses
/// Enter as a separate key; cmux `send` ends with the one `\n` escape it unescapes and has
/// no other backslash; cmux `notify` and Herdr pass one argument.
fn assert_transport_shape(via: &str, target: &str, calls: &[Vec<String>]) -> String {
    for call in calls {
        assert_clean(call);
    }
    match (via, target) {
        ("tmux", _) => {
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert_eq!(calls[0][..5], ["tmux", "send-keys", "-t", target, "-l"]);
            assert_eq!(calls[0].len(), 6, "{calls:?}");
            assert_eq!(calls[1], ["tmux", "send-keys", "-t", target, "Enter"]);
            calls[0][5].clone()
        }
        ("cmux", "notify") => {
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(
                calls[0][..5],
                ["cmux", "notify", "--title", "Another", "--body"]
            );
            assert_eq!(calls[0].len(), 6, "{calls:?}");
            assert!(!calls[0][5].contains('\\'), "{calls:?}");
            calls[0][5].clone()
        }
        ("cmux", _) => {
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0][..4], ["cmux", "send", "--surface", target]);
            assert_eq!(calls[0].len(), 5, "{calls:?}");
            let text = &calls[0][4];
            assert!(text.ends_with("\\n"), "{text:?}");
            assert_eq!(text.matches('\\').count(), 1, "one escape only: {text:?}");
            text.trim_end_matches("\\n").to_string()
        }
        ("herdr", _) => {
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0][..4], ["herdr", "agent", "prompt", target]);
            // The line is one argument; after it come only Herdr's own wait options.
            assert_eq!(
                calls[0][5..],
                [
                    "--wait",
                    "--until",
                    "working",
                    "--until",
                    "blocked",
                    "--until",
                    "idle",
                    "--until",
                    "done",
                    "--until",
                    "unknown",
                    "--timeout",
                    "8000"
                ],
                "{calls:?}"
            );
            calls[0][4].clone()
        }
        _ => unreachable!(),
    }
}

const TRANSPORTS: [(&str, &str); 4] = [
    ("tmux", "%3"),
    ("cmux", "surface:1"),
    ("cmux", "notify"),
    ("herdr", "codex"),
];

/// A `--message` full of control characters is typed as one clean line by every transport,
/// and the plan printed for it is the same clean line.
#[test]
fn every_transport_types_a_hostile_message_as_one_clean_line() {
    let env = Env::new();
    let expected = another_core::single_line(HOSTILE, 1000);
    assert!(expected.contains("Lunch?") && expected.contains("rm -rf ~/src"));
    for (via, target) in TRANSPORTS {
        let before = env.calls().len();
        let plan = env.json(&[
            "wake",
            "--agent",
            "codex",
            "--force",
            "--via",
            via,
            "--target",
            target,
            "--message",
            HOSTILE,
        ]);
        assert_eq!(plan[0]["sent"], true, "{via} {target}: {plan}");
        assert_eq!(plan[0]["message"], expected.as_str(), "{via} {target}");
        let calls = env.calls()[before..].to_vec();
        let typed = assert_transport_shape(via, target, &calls);
        let want = if via == "cmux" {
            expected.replace('\\', "/")
        } else {
            expected.clone()
        };
        assert_eq!(typed, want, "{via} {target}");
    }
}

/// The default wake line quotes stored subjects. A subject stored before `send` refused
/// control characters (or by an older peer) is cleaned on every transport, and the body is
/// never typed.
#[test]
fn every_transport_cleans_a_stored_hostile_subject() {
    let env = Env::new();
    let id = env.send("codex", "placeholder");
    env.store_hostile_subject(&id, HOSTILE);
    for (via, target) in TRANSPORTS {
        let before = env.calls().len();
        let plan = env.json(&["wake", "--agent", "codex", "--via", via, "--target", target]);
        assert_eq!(plan[0]["sent"], true, "{via} {target}: {plan}");
        let calls = env.calls()[before..].to_vec();
        let typed = assert_transport_shape(via, target, &calls);
        assert!(typed.starts_with("Another: 1 unread message(s)"), "{typed}");
        assert!(
            typed.contains("\"Lunch?"),
            "the subject is still quoted: {typed}"
        );
        assert!(!typed.contains("private body"), "{typed}");
        assert!(typed.chars().count() <= 1000, "{typed}");
    }
}

/// When the adapter fails, the wake fails loudly and nothing further is typed: tmux does
/// not press Enter after text it could not type, and the mail stays unread for the hooks.
#[test]
fn a_failing_adapter_is_reported_and_nothing_more_is_typed() {
    let env = Env::new();
    env.send("codex", "Rebase");
    for (via, target) in [
        ("tmux", "%dead"),
        ("cmux", "surface:dead"),
        ("herdr", "dead"),
    ] {
        env.kill_pane(target);
        let before = env.calls().len();
        let out = env
            .cmd(&["wake", "--agent", "codex", "--via", via, "--target", target])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{via}: a failed wake must not exit 0"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(via) && err.contains("exited"), "{via}: {err}");
        let calls = env.calls()[before..].to_vec();
        assert_eq!(
            calls.len(),
            1,
            "{via}: nothing after the failed call: {calls:?}"
        );
    }
    let inbox = env.json(&["inbox", "--agent", "codex"]);
    assert_eq!(inbox["unread"], 1, "a failed wake reads nothing: {inbox}");
}

/// `wake --all` applies the daemon's rules, so it no longer types into targets set by
/// hand: tmux and cmux cannot say who is in the pane or whether it is busy. It lists each
/// such agent with the reason, types nothing, and a wake by name still works. A failed
/// wake by name is recorded with what was known: nothing sent when the first command
/// failed.
#[test]
fn wake_all_leaves_hand_set_targets_to_a_wake_by_name() {
    let env = Env::new();
    env.json(&[
        "wake", "set", "--agent", "claude", "--via", "tmux", "--target", "%dead",
    ]);
    env.json(&[
        "wake", "set", "--agent", "codex", "--via", "tmux", "--target", "%7",
    ]);
    env.kill_pane("%dead");
    env.send("claude", "For claude");
    env.send("codex", "For codex");

    let out = env.cmd(&["--json", "wake", "--all"]).output().unwrap();
    assert!(
        out.status.success(),
        "nothing was attempted, nothing failed"
    );
    let plans: Value = serde_json::from_slice(&out.stdout).unwrap();
    let plans = plans.as_array().unwrap();
    assert_eq!(plans.len(), 2, "{plans:?}");
    for plan in plans {
        assert_eq!(plan["sent"], false, "{plans:?}");
        assert!(
            plan["skipped"]
                .as_str()
                .unwrap()
                .contains("no session Another can identify"),
            "{plans:?}"
        );
    }
    assert!(env.calls().is_empty(), "{:?}", env.calls());
    let text = env.cmd(&["wake", "--all"]).output().unwrap().stdout;
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains("skipped codex: no session"), "{text}");

    // By name, each is woken through its stored target; tmux cannot confirm the start.
    let plan = env.json(&["wake", "--agent", "codex"]);
    assert_eq!(plan[0]["sent"], true, "{plan}");
    assert_eq!(plan[0]["outcome"], "unconfirmed");
    assert_eq!(plan[0]["delivered"], "yes");
    assert_eq!(plan[0]["started"], "unknown");
    assert_eq!(plan[0]["failure"], false);
    assert!(
        env.calls()
            .iter()
            .any(|c| c[..4] == ["tmux", "send-keys", "-t", "%7"]),
        "codex's pane was typed into"
    );
    let out = env
        .cmd(&["--json", "wake", "--agent", "claude"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a failed wake does not exit 0");
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan[0]["outcome"], "undelivered", "{plan}");
    assert_eq!(plan[0]["sent"], false);
    assert_eq!(plan[0]["failure"], true);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("claude") && err.contains("exited"), "{err}");

    // Each wake by name left its event and its outcome on a row.
    let status = env.json(&["transport", "status"]);
    let rows = status["sessions"].as_array().unwrap();
    let row = |agent: &str| rows.iter().find(|r| r["agent"] == agent).unwrap().clone();
    assert_eq!(row("codex")["transport"], "tmux");
    assert_eq!(row("codex")["last_wake_outcome"], "unconfirmed");
    assert_eq!(row("codex")["hook_made"], false);
    assert_eq!(row("claude")["last_wake_outcome"], "undelivered");
    let human = env
        .cmd(&["wake", "--agent", "codex"])
        .output()
        .unwrap()
        .stdout;
    let human = String::from_utf8_lossy(&human);
    assert!(
        human.contains("delivered, not confirmed")
            && human.contains("tmux cannot report whether the agent started"),
        "{human}"
    );
}

// ---------------------------------------------------------------------------
// Real tmux.

/// A real tmux server on a private socket directory, so nothing touches the user's own
/// tmux sessions. Shut down with `tmux kill-server` on that socket when dropped.
struct RealTmux {
    socket_dir: PathBuf,
}

impl RealTmux {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .env("TMUX_TMPDIR", &self.socket_dir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for RealTmux {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

fn tmux_installed() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs on hosted Linux CI (which installs tmux) or locally with `ANOTHER_TEST_REAL_TMUX=1`.
fn real_tmux_wanted() -> bool {
    let hosted = std::env::var_os("GITHUB_ACTIONS").as_deref() == Some("true".as_ref())
        && std::env::var_os("RUNNER_ENVIRONMENT").as_deref() == Some("github-hosted".as_ref())
        && std::env::consts::OS == "linux";
    if hosted {
        assert!(
            tmux_installed(),
            "hosted Linux CI must install tmux for this test"
        );
        return true;
    }
    if std::env::var_os("ANOTHER_TEST_REAL_TMUX").is_some_and(|v| v == "1") {
        assert!(
            tmux_installed(),
            "ANOTHER_TEST_REAL_TMUX=1 but tmux is not on PATH"
        );
        return true;
    }
    eprintln!("skipped: real tmux runs on hosted Linux CI or with ANOTHER_TEST_REAL_TMUX=1");
    false
}

fn wait_for_file(path: &Path, limit: Duration) -> Option<Vec<u8>> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// The pane runs a shell that reads lines from its terminal. A wake with a hostile message
/// must arrive as exactly one line, equal to the clean text: a CR or LF that got through
/// would submit early, and DEL, backspace or NAK would edit the line in the terminal's
/// line discipline.
#[test]
fn a_real_tmux_pane_receives_one_clean_line() {
    if !real_tmux_wanted() {
        return;
    }
    let env = Env::new();
    let socket_dir = env.root.join("tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let server = RealTmux { socket_dir };
    let first = env.root.join("line1");
    let second = env.root.join("line2");
    let ready = env.root.join("ready");
    let script = env.root.join("pane.sh");
    std::fs::write(
        &script,
        format!(
            "stty -echo\n: > '{ready}'\nIFS= read -r a; printf '%s' \"$a\" > '{first}'\nIFS= read -r b; printf '%s' \"$b\" > '{second}'\nsleep 30\n",
            ready = ready.display(),
            first = first.display(),
            second = second.display(),
        ),
    )
    .unwrap();
    let started = server.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "wake",
        "-x",
        "250",
        "-y",
        "20",
        &format!("sh '{}'", script.display()),
    ]);
    assert!(
        started.status.success(),
        "tmux new-session: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        wait_for_file(&ready, Duration::from_secs(10)).is_some(),
        "the pane's shell never started"
    );
    let pane = String::from_utf8(
        server
            .tmux(&["display-message", "-p", "-t", "wake", "#{pane_id}"])
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(pane.starts_with('%'), "pane id {pane:?}");

    // The real tmux binary, reached through PATH without the fakes, on the private socket.
    let path = std::env::var("PATH").unwrap_or_default();
    let out = env
        .cmd_with_path(
            &[
                "--json",
                "wake",
                "--agent",
                "codex",
                "--force",
                "--via",
                "tmux",
                "--target",
                &pane,
                "--message",
                HOSTILE,
            ],
            &path,
        )
        .env("TMUX_TMPDIR", &server.socket_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "wake through real tmux: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = wait_for_file(&first, Duration::from_secs(10)).expect("no line reached the pane");
    let got = String::from_utf8(got).unwrap();
    assert_eq!(got, another_core::single_line(HOSTILE, 1000));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !second.exists(),
        "the wake submitted more than one line: {:?}",
        std::fs::read_to_string(&second)
    );
}
