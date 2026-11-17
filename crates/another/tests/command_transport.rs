//! Command transports (docs/WAKE.md, "Command transports"): a transport made of commands
//! from `transports.json`, and what replaced the built-in tmux and cmux adapters.
//!
//! Every program here is a fake: a `/bin/sh` script in a temp directory that records its
//! arguments byte for byte. In-process tests name the fake by its absolute path; tests of
//! the binary run it with a PATH of the fake directory and the system's own tools only, so
//! no real tmux, cmux or Herdr can be reached. Timed behaviour runs on a store with a
//! manual clock.
#![cfg(unix)]

#[path = "common/hosted_ci.rs"]
mod hosted_ci;
#[path = "common/transports.rs"]
mod transports;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use another::transport::command::{CommandTransport, COMMAND_CONFIRMS_GONE};
use another::transport::{load_config, Config, ListResult, State, Transport};
use another::wake::{target_in, wake, wake_all, WakeOptions, Waker, CONFIG_REFUSED};
use another_core::*;
use serde_json::{json, Value};
use transports::via;

/// One fake for every role. It records each call under `calls/<n>/`: one file per
/// argument, and the two environment values. Its first argument picks what it does, and
/// control files in its directory pick the exit code.
const TOOL: &str = r#"#!/bin/sh
dir='@DIR@'
n=$(ls "$dir/calls" | wc -l | tr -d ' ')
mkdir "$dir/calls/$n"
i=0
for a in "$@"; do printf '%s' "$a" > "$dir/calls/$n/$i"; i=$((i+1)); done
printf '%s' "${ANOTHER_TARGET-<unset>}" > "$dir/calls/$n/env-target"
printf '%s' "${ANOTHER_WAKE_MESSAGE-<unset>}" > "$dir/calls/$n/env-message"
code() { if [ -f "$dir/$1" ]; then head -n 1 "$dir/$1"; else echo 0; fi; }
case "$1" in
  status)
    # `status-seq` holds one exit code per line, used up one line per call; after it,
    # `status-code`.
    if [ -s "$dir/status-seq" ]; then
      c=$(head -n 1 "$dir/status-seq")
      tail -n +2 "$dir/status-seq" > "$dir/status-seq.next"
      mv "$dir/status-seq.next" "$dir/status-seq"
      exit "$c"
    fi
    exit "$(code status-code)" ;;
  list)
    if [ -f "$dir/list-out" ]; then cat "$dir/list-out"; fi
    exit "$(code list-code)" ;;
  one) exit "$(code one-code)" ;;
  two) exit "$(code two-code)" ;;
  hang) exec sleep 30 ;;
esac
exit 0
"#;

/// A fake tmux: panes are files under `panes/`. `server-dead` makes every call fail the
/// way tmux does with no server.
const FAKE_TMUX: &str = r#"#!/bin/sh
dir='@DIR@'
printf 'tmux' >> "$dir/calls.log"
for a in "$@"; do printf ' [%s]' "$a" >> "$dir/calls.log"; done
echo >> "$dir/calls.log"
if [ -f "$dir/server-dead" ]; then echo "no server running on /tmp/tmux-0/default" >&2; exit 1; fi
case "$1" in
  list-panes)
    for f in "$dir"/panes/*; do
      if [ -f "$f" ]; then basename "$f"; fi
    done
    exit 0 ;;
  display-message)
    if [ -f "$dir/panes/$4" ]; then echo "$4"; exit 0; fi
    echo "can't find pane: $4" >&2; exit 1 ;;
  send-keys)
    if [ -f "$dir/panes/$3" ]; then exit 0; fi
    echo "can't find pane: $3" >&2; exit 1 ;;
esac
exit 1
"#;

/// A fake Herdr that answers every prompt as started.
const FAKE_HERDR: &str = r#"#!/bin/sh
printf '{"id":"cli:agent:prompt","result":{"agent":{"agent_status":"working"},"type":"agent_prompted"}}\n'
"#;

fn script(path: &Path, text: &str, dir: &Path) {
    std::fs::write(path, text.replace("@DIR@", &dir.to_string_lossy())).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// One recorded call of the fake tool.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    args: Vec<String>,
    target: String,
    message: String,
}

/// A temp directory with the fake tool, a home, and a store on a manual clock.
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["calls", "panes"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        script(&root.join("tool"), TOOL, &root);
        script(&root.join("tmux"), FAKE_TMUX, &root);
        script(&root.join("herdr"), FAKE_HERDR, &root);
        let home = root.join("home");
        let (clock, handle) = Clock::manual(real_now());
        let store = Store::open_with_clock(&home, clock).unwrap();
        let fx = Fx {
            _dir: dir,
            root,
            home,
            store,
            clock: handle,
        };
        fx.agent("sender");
        fx
    }

    fn tool(&self) -> String {
        self.root.join("tool").to_string_lossy().into_owned()
    }

    fn agent(&self, name: &str) -> String {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .address
    }

    fn mail(&self, to: &str, subject: &str) -> String {
        self.store
            .send_message(SendMessage {
                from: "sender".into(),
                to: vec![to.into()],
                subject: subject.into(),
                body: "secret body".into(),
                ..Default::default()
            })
            .unwrap()
            .message_id
    }

    /// What a hook of that session writes when it runs inside `transport`.
    fn stamp(&self, agent: &str, session: &str, transport: &str, target: &str) {
        self.store
            .stamp_transport_session(agent, session, transport, target, "claude", "/work/app")
            .unwrap();
    }

    fn row(&self, agent: &str, session: &str) -> TransportSession {
        self.store
            .transport_sessions()
            .unwrap()
            .into_iter()
            .find(|r| r.agent == agent && r.session_id == session)
            .unwrap_or_else(|| panic!("no row for {agent} {session:?}"))
    }

    fn confirmed(&self, agent: &str, session: &str) -> Option<ConfirmedGone> {
        let id = self.store.agent(agent).unwrap().agent_id;
        self.store.transport_gone_confirmed(&id, session).unwrap()
    }

    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    fn sweeps(&self, waker: &mut Waker, times: i64, every: i64) {
        for _ in 0..times {
            self.advance(every);
            waker.sweep(&self.store).unwrap();
        }
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.store
            .events(1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    }

    /// Set a control file of the fake tool to `text`, or remove it.
    fn control(&self, name: &str, text: Option<&str>) {
        let path = self.root.join(name);
        match text {
            Some(text) => std::fs::write(path, text).unwrap(),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Forget every recorded call and every control file.
    fn reset(&self) {
        std::fs::remove_dir_all(self.root.join("calls")).unwrap();
        std::fs::create_dir_all(self.root.join("calls")).unwrap();
        for name in [
            "status-seq",
            "status-code",
            "list-out",
            "list-code",
            "one-code",
            "two-code",
        ] {
            self.control(name, None);
        }
    }

    /// Every call of the fake tool so far, in order.
    fn calls(&self) -> Vec<Call> {
        let mut calls = BTreeMap::new();
        for entry in std::fs::read_dir(self.root.join("calls")).unwrap() {
            let entry = entry.unwrap();
            let n: usize = entry.file_name().to_str().unwrap().parse().unwrap();
            let text = |name: &str| {
                String::from_utf8(std::fs::read(entry.path().join(name)).unwrap()).unwrap()
            };
            let mut args = Vec::new();
            while entry.path().join(args.len().to_string()).exists() {
                args.push(text(&args.len().to_string()));
            }
            calls.insert(
                n,
                Call {
                    args,
                    target: text("env-target"),
                    message: text("env-message"),
                },
            );
        }
        calls.into_values().collect()
    }

    /// The first argument of each call: `status`, `one`, `two`, `list`.
    fn roles(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|c| c.args[0].clone())
            .collect()
    }

    fn tmux_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn pane(&self, pane: &str, there: bool) {
        let path = self.root.join("panes").join(pane);
        if there {
            std::fs::write(path, "").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The documented example with `tmux` replaced by the fake's absolute path, written as
    /// the home's config. Refuses to go on unless every tmux command names the fake.
    fn write_tmux_example(&self) -> Config {
        let fake = self.root.join("tmux").to_string_lossy().into_owned();
        let text = transports::example().replace("[\"tmux\",", &format!("[\"{fake}\","));
        transports::write(&self.home, &text);
        let config = load_config(&self.home);
        assert_eq!(config.error, None);
        assert_eq!(config.get("tmux").unwrap().programs(), [fake.as_str()]);
        config
    }

    /// A transport of `config`, asking `status` every 20 ms.
    fn transport(&self, config: &Config, name: &str) -> CommandTransport {
        let spec = config.get(name).unwrap_or_else(|| panic!("no {name}"));
        for program in spec.programs() {
            assert!(
                Path::new(program).starts_with(&self.root),
                "{program} is not a fake"
            );
        }
        CommandTransport::new(spec.clone()).with_timing(
            Duration::from_secs(10),
            Duration::from_secs(3),
            Duration::from_millis(20),
        )
    }
}

/// Characters a target may not hold beyond controls: zero-width and other invisible
/// format characters, bidirectional marks, overrides and isolates, the line and
/// paragraph separators, and the Arabic letter mark.
const INVISIBLE: [char; 13] = [
    '\u{200B}', '\u{200D}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2060}', '\u{2066}', '\u{2069}',
    '\u{FEFF}', '\u{2028}', '\u{2029}', '\u{200E}', '\u{061C}',
];

fn config(value: Value) -> Config {
    let config = Config::from_json(&value.to_string());
    assert_eq!(config.error, None);
    config
}

/// Every state a status can be mapped to, one exit code each.
fn every_state() -> Value {
    json!({
        "0": "idle", "3": "working", "4": "blocked", "5": "present",
        "6": "unknown", "7": "gone", "8": "no_answer"
    })
}

// ---------------------------------------------------------------------------
// SPEC-Q9 section 7.

/// A value with shell syntax, quotes, placeholder text and spaces arrives as one unchanged
/// argument, in the argument the config put it in, and nothing in it is run. Values that
/// could press a key, hide text or read as an option never reach a command.
#[test]
fn command_wake_runs_argv_without_a_shell() {
    let fx = Fx::new();
    let tool = fx.tool();
    let config = config(json!({"version": 1, "transports": {
        "t": {"wake": [[tool, "one", "-t", "{target}", "--", "{message}"],
                       [tool, "two", "pre {message} post", "{target}{target}"]]},
        "bare": {"wake": [[tool, "one", "{message}"]]},
        "slash": {"wake": [[tool, "one", "{message}\\n"]], "backslash_to_slash": true}
    }}));
    let t = fx.transport(&config, "t");
    let marker = fx.root.join("marker");
    let m = marker.display();
    let message = format!(
        "it's \"$(touch {m})\"; `touch {m}` && {{target}} {{message}} | tee {m} > {m} \\n $HOME * ~"
    );
    let target = "p 1 \"q\" $(id) ;{message} 'x'";

    let result = t.wake(target, &message, None);
    assert_eq!(result.outcome, WakeOutcome::Unconfirmed, "{result:?}");
    let calls = fx.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].args, ["one", "-t", target, "--", message.as_str()]);
    assert_eq!(
        calls[1].args,
        [
            "two".to_string(),
            format!("pre {message} post"),
            format!("{target}{target}")
        ]
    );
    for call in &calls {
        assert_eq!(call.target, target);
        assert_eq!(call.message, message);
    }
    assert!(!marker.exists(), "something in the value was run");
    // `--dry-run` shows the same arguments and runs nothing.
    assert_eq!(t.wake_argv(target, &message)[0][1..], calls[0].args);
    assert_eq!(fx.calls().len(), 2);

    // Nothing that could press a key or reorder text reaches the command: the message is
    // one safe line (D75), still one argument.
    fx.reset();
    let keys = "a\nb\rc\x1b[2Jd\te\u{202e}f\u{2066}g\u{7f}h\u{0}i\u{85}j";
    assert_eq!(t.wake("pane", keys, None).outcome, WakeOutcome::Unconfirmed);
    let typed = fx.calls()[0].args[4].clone();
    assert_eq!(typed, single_line(keys, 1000));
    assert!(!typed.chars().any(is_unsafe_text_char), "{typed:?}");
    assert_eq!(fx.calls()[0].args.len(), 5);
    assert_eq!(fx.calls()[0].message, typed);

    // Very long text is cut to 1,000 characters.
    fx.reset();
    t.wake("pane", &"\u{e9}".repeat(5000), None);
    let typed = fx.calls()[0].args[4].clone();
    assert_eq!(typed.chars().count(), 1000);
    assert!(typed.ends_with('\u{2026}'));

    // A message that would begin an argument with `-` is refused before anything runs.
    fx.reset();
    for dash in ["-rf", "--help", "-"] {
        let result = t.wake("pane", dash, None);
        assert_eq!(result.outcome, WakeOutcome::Undelivered, "{dash}");
        assert!(result.detail.contains("read as an option"), "{result:?}");
        let bare = fx.transport(&config, "bare").wake("pane", dash, None);
        assert_eq!(bare.outcome, WakeOutcome::Undelivered, "{dash}");
    }
    // A target is held to the rule for every target.
    let long = "p".repeat(201);
    for bad in ["-t", "--help", "a\nb", "a\x1bb", "", "  ", long.as_str()] {
        let result = t.wake(bad, "hi", None);
        assert_eq!(result.outcome, WakeOutcome::Undelivered, "{bad:?}");
    }
    // Nor a character that cannot be seen or that reorders what is shown.
    for hidden in INVISIBLE {
        let bad = format!("p{hidden}x");
        let result = t.wake(&bad, "hi", None);
        assert_eq!(result.outcome, WakeOutcome::Undelivered, "{bad:?}");
    }
    assert!(fx.calls().is_empty(), "{:?}", fx.calls());

    // `backslash_to_slash`: the only escape left is the one the config wrote.
    let slash = fx.transport(&config, "slash");
    slash.wake("pane", "a\\nb\\", None);
    let call = &fx.calls()[0];
    assert_eq!(call.args, ["one", "a/nb/\\n"]);
    assert_eq!(call.message, "a/nb/");

    // Through `another wake`, with a subject stored as an older build could have stored
    // it: one argument, equal to the line the plan shows.
    fx.reset();
    let worker = fx.agent("worker");
    let id = fx.mail(&worker, "placeholder");
    rusqlite::Connection::open(fx.home.join(DB_FILE))
        .unwrap()
        .execute(
            "UPDATE messages SET subject = ?1 WHERE message_id = ?2",
            rusqlite::params!["Lunch?\r\x1b[2J$(touch x); -rf\n{target}", id],
        )
        .unwrap();
    let options = WakeOptions {
        via: Some(via("t")),
        target: Some("pane 9".into()),
        config: Some(config.clone()),
        ..Default::default()
    };
    let plan = wake(&fx.store, &worker, &options).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
    let calls = fx.calls();
    let line = plan.message.unwrap();
    assert!(line.starts_with("Another: 1 unread message(s)"), "{line}");
    assert!(line.contains("$(touch x); -rf {target}"), "{line}");
    assert_eq!(calls[0].args, ["one", "-t", "pane 9", "--", line.as_str()]);
    assert!(!line.contains("secret body"));
}

/// Every refusal of the load checks leaves no command transport at all: no command runs,
/// Herdr still works, and the reason is shown by the daemon (once per change, and at its
/// start), by `another doctor`, by `another transport status` and by `another wake list`.
#[test]
fn command_config_load_checks() {
    let fx = Fx::new();
    let tool = fx.tool();
    let worker = fx.agent("worker");
    fx.mail(&worker, "Hello");
    let good = json!({"version": 1, "transports": {
        "t": {"wake": [[tool, "one", "{target}", "{message}"]]}
    }})
    .to_string();
    let with = |body: Value| json!({"version": 1, "transports": {"t": body}}).to_string();
    let to_t = WakeOptions {
        via: Some(via("t")),
        target: Some("p1".into()),
        ..Default::default()
    };

    // No file: no command transports, and no error.
    let none = load_config(&fx.home);
    assert!(none.error.is_none() && none.transports.is_empty());
    let plan = wake(&fx.store, &worker, &to_t).unwrap();
    let why = plan.error.unwrap();
    assert!(
        why.contains("no transport named \"t\" is configured") && why.contains("docs/WAKE.md"),
        "{why}"
    );

    let refused: Vec<(String, &str)> = vec![
        ("{ not json".into(), "not valid JSON"),
        (
            json!({"version": 2, "transports": {}}).to_string(),
            "version 2",
        ),
        (
            json!({"transports": {}}).to_string(),
            "`version` is missing",
        ),
        (
            json!({"version": 1, "shell": true}).to_string(),
            "unknown key \"shell\"",
        ),
        (
            with(json!({"wake": [[tool, "{message}"]], "shell": true})),
            "unknown key \"shell\"",
        ),
        (
            with(json!({"list": {"argv": [tool, "list"]}})),
            "`wake` is required",
        ),
        (with(json!({"wake": []})), "non-empty list of commands"),
        (
            with(json!({"wake": [[tool]], "confirm_secs": 31})),
            "0 to 30",
        ),
        (
            with(json!({"wake": [[tool]], "status": {"argv": [tool]}})),
            "needs `exit_codes`",
        ),
        (
            with(
                json!({"wake": [[tool]], "status": {"argv": [tool], "exit_codes": {"0": "busy"}}}),
            ),
            "not a state",
        ),
        (
            with(json!({"wake": [[tool]], "capture_env": "A B"})),
            "environment variable",
        ),
        (
            with(json!({"wake": [["{target}", "x"]]})),
            "placeholder in its program name",
        ),
        (
            with(json!({"wake": [["sh", "-c", "{message}"]]})),
            "shell code",
        ),
        (
            with(json!({"wake": [["scripts/wake.sh", "{target}"]]})),
            "absolute path",
        ),
        (
            with(json!({"wake": [["bash", "-lc", "tool {target}"]]})),
            "shell code",
        ),
        (
            json!({"version": 1, "transports": {"herdr": {"wake": [[tool]]}}}).to_string(),
            "reserved",
        ),
        (
            json!({"version": 1, "transports": {"rex": {"wake": [[tool]]}}}).to_string(),
            "reserved",
        ),
        (
            json!({"version": 1, "transports": {"Bad Name": {"wake": [[tool]]}}}).to_string(),
            "a name is",
        ),
        // One bad transport takes the good one beside it down too.
        (
            json!({"version": 1, "transports": {
                "t": {"wake": [[tool, "one", "{target}", "{message}"]]},
                "u": {"wake": [["sh", "-c", "{message}"]]}
            }})
            .to_string(),
            "transport \"u\"",
        ),
    ];
    for (text, want) in &refused {
        transports::write(&fx.home, text);
        let config = load_config(&fx.home);
        let error = config
            .error
            .clone()
            .unwrap_or_else(|| panic!("accepted: {text}"));
        assert!(error.contains(want), "{text}\n  gave {error:?}");
        assert!(error.contains("transports.json"), "{error}");
        assert!(config.transports.is_empty(), "{text}");
        let plan = wake(&fx.store, &worker, &to_t).unwrap();
        assert!(!plan.sent && plan.outcome.is_none(), "{plan:?}");
        let why = plan.error.unwrap();
        assert!(why.contains(want) && why.contains("docs/WAKE.md"), "{why}");
    }

    // Who may write the file: a file that group or others can write is refused, whatever
    // it says. Readable by others is fine.
    let path = fx.home.join("transports.json");
    for mode in [0o620, 0o602, 0o666, 0o660] {
        transports::write(&fx.home, &good);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let error = load_config(&fx.home).error.unwrap();
        assert!(
            error.contains("writable by group or others"),
            "{mode:o}: {error}"
        );
        assert!(wake(&fx.store, &worker, &to_t).unwrap().error.is_some());
    }
    transports::write(&fx.home, &good);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(load_config(&fx.home).error, None);
    // Larger than 1 MiB: refused, whatever it says.
    let padded = format!("{good}{}", " ".repeat(1024 * 1024));
    transports::write(&fx.home, &padded);
    let error = load_config(&fx.home).error.unwrap();
    assert!(error.contains("larger than 1 MiB"), "{error}");
    assert!(wake(&fx.store, &worker, &to_t).unwrap().error.is_some());
    // Not a regular file.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = load_config(&fx.home).error.unwrap();
    assert!(error.contains("not a regular file"), "{error}");
    std::fs::remove_dir(&path).unwrap();
    assert!(fx.calls().is_empty(), "a command ran: {:?}", fx.calls());

    // While the file is refused Herdr works.
    transports::write(&fx.home, "{ not json");
    let herdr = WakeOptions {
        via: Some(WakeVia::herdr()),
        target: Some("w1:p1".into()),
        herdr: Some(fx.root.join("herdr").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let plan = wake(&fx.store, &worker, &herdr).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Started), "{plan:?}");

    // The daemon reports the refusal at its first sweep, then only when the reason
    // changes; a corrected file is quiet, and a file broken again is reported again.
    let refusals = |waker: &mut Waker| -> Vec<String> {
        fx.advance(30);
        waker
            .sweep(&fx.store)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == CONFIG_REFUSED)
            .map(|e| e.detail)
            .collect()
    };
    let mut waker = Waker::with_transports(Vec::new());
    let first = refusals(&mut waker);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("not valid JSON") && first[0].contains("docs/WAKE.md"));
    assert!(refusals(&mut waker).is_empty(), "the same reason, once");
    transports::write(&fx.home, &json!({"version": 3}).to_string());
    let changed = refusals(&mut waker);
    assert_eq!(changed.len(), 1);
    assert!(changed[0].contains("version 3"), "{changed:?}");
    transports::write(&fx.home, &good);
    assert!(refusals(&mut waker).is_empty());
    transports::write(&fx.home, &json!({"version": 3}).to_string());
    assert_eq!(refusals(&mut waker).len(), 1, "broken again");
    // A daemon that starts on a refused file says so at once.
    let mut restarted = Waker::with_transports(Vec::new());
    let events = restarted.run(&fx.store).unwrap();
    assert!(
        events.iter().any(|e| e.action == CONFIG_REFUSED),
        "{events:?}"
    );

    // The commands that print it.
    let bin = Bin::at(&fx.root, &fx.home);
    let (_, report) = bin.doctor();
    let row = check(&report, "transports");
    assert_eq!(row["status"], "warn", "{report}");
    assert!(
        row["detail"].as_str().unwrap().contains("version 3"),
        "{row}"
    );
    assert!(
        row["fix"].as_str().unwrap().contains("transports.json"),
        "{row}"
    );
    let status = bin.json(&["transport", "status"]);
    assert!(
        status["config_error"]
            .as_str()
            .unwrap()
            .contains("version 3"),
        "{status}"
    );
    let text = bin.text(&["transport", "status"]);
    assert!(
        text.contains("version 3") && text.contains("docs/WAKE.md"),
        "{text}"
    );
    let listed = bin.run(&["wake", "list"]);
    assert!(listed.status.success());
    assert!(
        String::from_utf8_lossy(&listed.stderr).contains("version 3"),
        "{listed:?}"
    );
    let woke = bin.run(&["wake", "--agent", "worker", "--via", "herdr", "--dry-run"]);
    assert!(
        String::from_utf8_lossy(&woke.stderr).contains("version 3"),
        "{woke:?}"
    );

    // A good file: the transport exists and its command runs.
    transports::write(&fx.home, &good);
    let (_, report) = bin.doctor();
    assert_eq!(check(&report, "transports")["status"], "ok", "{report}");
    let plan = wake(&fx.store, &worker, &to_t).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
    assert_eq!(fx.calls().len(), 1);
    assert_eq!(fx.calls()[0].args[..2], ["one", "p1"]);
}

/// The exit-code map, the outcomes of a wake with and without a status to ask, and the
/// lines of a list.
#[test]
fn command_results_are_read_as_specified() {
    let fx = Fx::new();
    let tool = fx.tool();
    let nope = fx.root.join("nope").to_string_lossy().into_owned();
    let status = json!({"argv": [tool, "status", "{target}"], "exit_codes": every_state()});
    let config = config(json!({"version": 1, "transports": {
        "full": {"wake": [[tool, "one", "{target}", "{message}"], [tool, "two", "{target}"]],
                 "status": status, "list": {"argv": [tool, "list"]}, "confirm_secs": 1},
        "plain": {"wake": [[tool, "one", "{target}", "{message}"]]},
        "noconfirm": {"wake": [[tool, "one", "{target}", "{message}"]],
                      "status": status, "confirm_secs": 0},
        "presence": {"wake": [[tool, "one", "{target}", "{message}"]],
                     "status": {"argv": [tool, "status", "{target}"],
                                "exit_codes": {"0": "present", "2": "idle"}}},
        "missing": {"wake": [[nope, "{message}"]],
                    "status": {"argv": [nope], "exit_codes": {"0": "idle"}},
                    "list": {"argv": [nope]}}
    }}));
    let full = fx.transport(&config, "full");
    let plain = fx.transport(&config, "plain");

    // status: the exit code is looked up in the map.
    for (code, state) in [
        ("0", State::Idle),
        ("3", State::Working),
        ("4", State::Blocked),
        ("5", State::Present),
        ("6", State::Unknown),
        ("7", State::Gone),
        ("8", State::NoAnswer),
        // Not in the map: no answer, never `gone`.
        ("1", State::NoAnswer),
        ("9", State::NoAnswer),
        ("127", State::NoAnswer),
    ] {
        fx.control("status-code", Some(code));
        let seen = full.status("%1", None);
        assert_eq!(seen.state, state, "exit {code}: {seen:?}");
        // It never says who is there.
        assert_eq!(
            (seen.session_id, seen.program, seen.cwd),
            (None, None, None)
        );
    }
    let asked = fx.calls();
    assert_eq!(asked[0].args, ["status", "%1"]);
    assert_eq!(
        (asked[0].target.as_str(), asked[0].message.as_str()),
        ("%1", "<unset>")
    );
    fx.reset();
    assert_eq!(
        fx.transport(&config, "missing").status("%1", None).state,
        State::NoAnswer
    );
    assert_eq!(
        plain.status("%1", None).state,
        State::NoAnswer,
        "no status command"
    );
    assert_eq!(full.status("--help", None).state, State::NoAnswer);
    assert!(fx.calls().is_empty());

    // wake: what the commands' exit codes mean.
    let wake = |t: &CommandTransport| t.wake("%1", "hello", None);
    fx.control("one-code", Some("1"));
    let result = wake(&full);
    assert_eq!(result.outcome, WakeOutcome::Undelivered, "{result:?}");
    assert_eq!(
        fx.roles(),
        ["status", "one"],
        "nothing after the failed first command"
    );
    fx.reset();
    fx.control("two-code", Some("1"));
    assert_eq!(
        wake(&full).outcome,
        WakeOutcome::Unknown,
        "a later command failed"
    );
    assert_eq!(fx.roles(), ["status", "one", "two"]);
    fx.reset();
    let cannot_start = wake(&fx.transport(&config, "missing"));
    assert_eq!(
        cannot_start.outcome,
        WakeOutcome::Undelivered,
        "{cannot_start:?}"
    );
    assert!(
        cannot_start.detail.contains("is it installed"),
        "{cannot_start:?}"
    );

    // All commands exit 0: delivered. Then status is asked until it shows a start.
    fx.control("status-seq", Some("0\n3\n"));
    let started = wake(&full);
    assert_eq!(started.outcome, WakeOutcome::Started, "{started:?}");
    assert_eq!(
        fx.roles(),
        ["status", "one", "two", "status"],
        "it stops at the first start"
    );
    fx.reset();
    fx.control("status-seq", Some("0\n0\n0\n4\n"));
    assert_eq!(
        wake(&full).outcome,
        WakeOutcome::Started,
        "blocked after the line counts"
    );
    fx.reset();
    // Every answer idle for the whole second: delivered, did not start. A failure.
    let not_started = wake(&full);
    assert_eq!(
        not_started.outcome,
        WakeOutcome::NotStarted,
        "{not_started:?}"
    );
    assert!(not_started.outcome.failure());
    assert!(
        fx.roles().len() > 5,
        "asked more than once: {:?}",
        fx.roles()
    );
    fx.reset();
    // Anything else among the answers: not confirmed, and not a failure.
    fx.control("status-seq", Some("0\n0\n9\n"));
    let mixed = wake(&full);
    assert_eq!(mixed.outcome, WakeOutcome::Unconfirmed, "{mixed:?}");
    fx.reset();
    fx.control("status-seq", Some("0\n"));
    fx.control("status-code", Some("5"));
    assert_eq!(
        wake(&full).outcome,
        WakeOutcome::Unconfirmed,
        "present says nothing"
    );
    fx.reset();

    // An agent waiting on a person, or nothing at the target: nothing is typed.
    fx.control("status-code", Some("4"));
    let blocked = wake(&full);
    assert_eq!(blocked.outcome, WakeOutcome::RefusedBlocked, "{blocked:?}");
    fx.control("status-code", Some("7"));
    assert_eq!(wake(&full).outcome, WakeOutcome::Gone);
    assert_eq!(fx.roles(), ["status", "status"], "no wake command ran");
    fx.reset();

    // No status, `confirm_secs` 0, or a status that cannot show activity: not confirmed,
    // and no status is asked after the line.
    let unconfirmed = wake(&plain);
    assert_eq!(unconfirmed.outcome, WakeOutcome::Unconfirmed);
    assert_eq!(
        unconfirmed.detail,
        "plain cannot report whether the agent started"
    );
    assert_eq!(fx.roles(), ["one"]);
    fx.reset();
    fx.control("status-code", Some("3"));
    assert_eq!(
        wake(&fx.transport(&config, "noconfirm")).outcome,
        WakeOutcome::Unconfirmed
    );
    assert_eq!(fx.roles(), ["status", "one"], "asked before, not after");
    fx.reset();
    fx.control("status-code", Some("2"));
    assert_eq!(
        wake(&fx.transport(&config, "presence")).outcome,
        WakeOutcome::Unconfirmed,
        "idle alone cannot tell a start from none"
    );
    assert_eq!(
        fx.roles(),
        ["one"],
        "a status that can refuse nothing is not asked"
    );
    fx.reset();

    // list: lines with and without a state and a session id.
    fx.control("list-out", Some("%1\n\n%2\tidle\n%3\tworking\tsess-3\n"));
    let ListResult::Sessions(sessions) = full.list(None) else {
        panic!("a good list");
    };
    let brief: Vec<(&str, State, Option<&str>)> = sessions
        .iter()
        .map(|s| {
            (
                s.target.as_str(),
                s.observation.state,
                s.observation.session_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        brief,
        [
            ("%1", State::Present, None),
            ("%2", State::Idle, None),
            ("%3", State::Working, Some("sess-3")),
        ]
    );
    let listed = fx.calls();
    assert_eq!(listed[0].args, ["list"]);
    assert_eq!(listed[0].target, "<unset>");
    fx.control("list-out", Some(""));
    assert_eq!(full.list(None), ListResult::Sessions(Vec::new()));
    fx.control("list-out", Some("%1\tbusy\n%2\n"));
    assert!(
        matches!(full.list(None), ListResult::NoAnswer(_)),
        "a malformed line"
    );
    fx.control("list-out", Some("%1\n"));
    fx.control("list-code", Some("1"));
    assert!(
        matches!(full.list(None), ListResult::NoAnswer(_)),
        "a non-zero exit"
    );
    assert_eq!(plain.list(None), ListResult::Unsupported);
    assert!(matches!(
        fx.transport(&config, "missing").list(None),
        ListResult::NoAnswer(_)
    ));
}

/// With the tmux example a session is shown as present, then gone and in attention when a
/// good list no longer has its pane. A dead server is no answer. The row is never matched,
/// so never alive, never confirmed gone and never woken automatically. A wake by hand
/// works and is not confirmed.
#[test]
fn tmux_rows_are_shown_but_never_matched() {
    let fx = Fx::new();
    fx.write_tmux_example();
    let worker = fx.agent("worker");
    let other = fx.agent("other");
    fx.stamp(&worker, "s1", "tmux", "%1");
    fx.stamp(&other, "s2", "tmux", "%2");
    fx.pane("%1", true);
    fx.pane("%2", true);
    // Both switches on, as a build that confirms gone sessions would have them.
    let mut waker = Waker::with_transports(Vec::new());
    waker.confirm_after = Some(Duration::from_secs(900));
    waker.confirm_command_after = Some(Duration::from_secs(900));

    waker.sweep(&fx.store).unwrap();
    for (agent, session) in [(&worker, "s1"), (&other, "s2")] {
        let row = fx.row(agent, session);
        assert_eq!(row.state.as_deref(), Some("present"), "{row:?}");
        assert_eq!(row.matched, 0, "never matched, not even weakly");
        assert_eq!(row.attention, None);
        assert_eq!((row.seen_live_at, row.last_alive_at), (None, None));
    }
    assert_eq!(
        fx.tmux_calls(),
        ["tmux [list-panes] [-a] [-F] [#{pane_id}]"]
    );

    // Mail waits: the daemon and `--all` type nothing, and say why.
    fx.mail(&worker, "Hello");
    let mut reasons = Vec::new();
    for _ in 0..10 {
        fx.advance(3);
        reasons.extend(waker.tick(&fx.store).unwrap());
    }
    let reason = reasons.iter().find(|e| e.agent == worker).unwrap();
    assert_eq!(reason.action, "skipped", "{reasons:?}");
    assert!(
        reason.detail.contains("not woken automatically"),
        "{reason:?}"
    );
    let plans = wake_all(&fx.store, &WakeOptions::default()).unwrap();
    let plan = plans.iter().find(|p| p.agent == worker).unwrap();
    assert!(!plan.sent && plan.outcome.is_none(), "{plan:?}");
    assert!(
        plan.skipped
            .as_deref()
            .unwrap()
            .contains("not woken automatically"),
        "{plan:?}"
    );
    let forced = WakeOptions {
        force: true,
        via: Some(via("tmux")),
        ..Default::default()
    };
    assert!(wake_all(&fx.store, &forced)
        .unwrap()
        .iter()
        .all(|p| !p.sent));
    assert!(
        !fx.tmux_calls().iter().any(|c| c.contains("send-keys")),
        "{:?}",
        fx.tmux_calls()
    );
    assert!(fx.events(TRANSPORT_WAKE_EVENT).is_empty());
    assert_eq!(
        fx.store.unannounced(&worker, "", 10).unwrap().items.len(),
        1,
        "no claim was taken"
    );

    // The pane goes: gone at the next good list, in attention 60 seconds later.
    fx.pane("%1", false);
    fx.sweeps(&mut waker, 1, 30);
    let row = fx.row(&worker, "s1");
    assert_eq!(row.state.as_deref(), Some("gone"), "{row:?}");
    assert_eq!(row.attention, None, "not yet");
    fx.sweeps(&mut waker, 2, 30);
    assert_eq!(fx.row(&worker, "s1").attention.as_deref(), Some("gone"));
    assert_eq!(fx.row(&other, "s2").state.as_deref(), Some("present"));

    // A dead server is no answer for every row, never gone.
    std::fs::write(fx.root.join("server-dead"), "").unwrap();
    fx.sweeps(&mut waker, 1, 30);
    for (agent, session) in [(&worker, "s1"), (&other, "s2")] {
        let row = fx.row(agent, session);
        assert_eq!(row.state.as_deref(), Some("no_answer"), "{row:?}");
        assert_eq!(row.first_gone_at, None);
    }
    std::fs::remove_file(fx.root.join("server-dead")).unwrap();

    // Twenty watched minutes gone, with another session still listed: never confirmed.
    fx.sweeps(&mut waker, 40, 30);
    let row = fx.row(&worker, "s1");
    assert_eq!(row.state.as_deref(), Some("gone"));
    assert_eq!(row.attention.as_deref(), Some("gone"));
    assert_eq!(
        (row.gone_confirmed_at, row.seen_live_at, row.last_alive_at),
        (None, None, None)
    );
    assert_eq!(fx.confirmed(&worker, "s1"), None);
    assert!(fx.events(TRANSPORT_GONE_EVENT).is_empty());

    // A wake by hand reaches the session's pane with the old two commands, and ends
    // "delivered, not confirmed".
    fx.mail(&other, "For other");
    let plan = wake(&fx.store, &other, &WakeOptions::default()).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
    assert_eq!(plan.session_id.as_deref(), Some("s2"));
    assert!(plan.sent && !plan.failure);
    assert_eq!(
        WakeOutcome::Unconfirmed.meaning(),
        "delivered, not confirmed"
    );
    assert_eq!(
        plan.detail.as_deref(),
        Some("tmux cannot report whether the agent started")
    );
    let calls = fx.tmux_calls();
    let typed = &calls[calls.len() - 2..];
    assert!(
        typed[0].starts_with("tmux [send-keys] [-t] [%2] [-l] [Another: 1 unread message(s)"),
        "{typed:?}"
    );
    assert_eq!(typed[1], "tmux [send-keys] [-t] [%2] [Enter]");
    let recorded = fx.events(TRANSPORT_WAKE_EVENT);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["transport"], "tmux");
    assert_eq!(recorded[0]["trigger"], "manual");
    assert_eq!(
        fx.row(&other, "s2").last_wake_outcome.as_deref(),
        Some("unconfirmed")
    );
    // To the vanished pane: the row was last seen gone, so the wake is refused and
    // nothing runs.
    let before = fx.tmux_calls().len();
    let plan = wake(&fx.store, &worker, &WakeOptions::default()).unwrap();
    assert!(plan.outcome.is_none() && !plan.sent, "{plan:?}");
    assert!(plan.error.as_deref().unwrap().contains("gone"), "{plan:?}");
    assert_eq!(fx.tmux_calls().len(), before);
    // Forced: nothing sent, a failed wake, and Enter is not pressed.
    let forced = WakeOptions {
        force: true,
        ..Default::default()
    };
    let plan = wake(&fx.store, &worker, &forced).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Undelivered), "{plan:?}");
    assert!(plan.failure);
    assert!(
        fx.tmux_calls().last().unwrap().contains("[-l]"),
        "{:?}",
        fx.tmux_calls()
    );
}

/// A transport whose `list` prints session ids is matched by session id, and its row
/// follows the session. It is still never woken automatically. Confirming one gone is
/// switched off in this build; with the switch on it follows the rules Herdr rows follow.
#[test]
fn a_list_with_session_ids_is_matched_and_still_not_woken_automatically() {
    let fx = Fx::new();
    let tool = fx.tool();
    transports::write(
        &fx.home,
        &json!({"version": 1, "transports": {"ids": {
            "wake": [[tool, "one", "{target}", "{message}"]],
            "status": {"argv": [tool, "status", "{target}"],
                       "exit_codes": {"0": "idle", "3": "working"}},
            "list": {"argv": [tool, "list"]}
        }}})
        .to_string(),
    );
    assert_eq!(load_config(&fx.home).error, None);
    let worker = fx.agent("worker");
    let anchor = fx.agent("anchor");
    fx.stamp(&worker, "s1", "ids", "t1");
    fx.stamp(&anchor, "s2", "ids", "t2");
    // As shipped: nothing is confirmed.
    const { assert!(!COMMAND_CONFIRMS_GONE, "off in this build") };
    let mut waker = Waker::with_transports(Vec::new());
    assert_eq!(waker.confirm_command_after, None);

    fx.control("list-out", Some("t1\tidle\ts1\nt2\tidle\ts2\n"));
    waker.sweep(&fx.store).unwrap();
    let row = fx.row(&worker, "s1");
    assert_eq!(
        (row.matched, row.state.as_deref()),
        (1, Some("idle")),
        "{row:?}"
    );
    assert!(row.seen_live_at.is_some() && row.last_alive_at.is_some());

    // The session moves: the row follows it.
    fx.control("list-out", Some("t9\tidle\ts1\nt2\tidle\ts2\n"));
    fx.sweeps(&mut waker, 1, 30);
    assert_eq!(fx.row(&worker, "s1").target, "t9");
    // Another session id at the row's target: unknown, not this session.
    fx.control("list-out", Some("t9\tidle\tsomeone-else\nt2\tidle\ts2\n"));
    fx.sweeps(&mut waker, 1, 30);
    let row = fx.row(&worker, "s1");
    assert_eq!(
        (row.matched, row.state.as_deref()),
        (0, Some("unknown")),
        "{row:?}"
    );
    fx.control("list-out", Some("t9\tidle\ts1\nt2\tidle\ts2\n"));
    fx.sweeps(&mut waker, 1, 30);

    // Idle, matched, mail waiting: still not woken automatically.
    fx.mail(&worker, "Hello");
    for _ in 0..10 {
        fx.advance(3);
        waker.tick(&fx.store).unwrap();
    }
    assert!(!fx.roles().contains(&"one".to_string()), "{:?}", fx.roles());
    assert!(fx.events(TRANSPORT_WAKE_EVENT).is_empty());

    // It vanishes while the anchor stays listed. As shipped: gone, shown, not confirmed.
    fx.control("list-out", Some("t2\tidle\ts2\n"));
    fx.sweeps(&mut waker, 40, 30);
    let row = fx.row(&worker, "s1");
    assert_eq!(row.state.as_deref(), Some("gone"));
    assert_eq!(row.attention.as_deref(), Some("gone"));
    assert_eq!(fx.confirmed(&worker, "s1"), None);
    assert!(fx.events(TRANSPORT_GONE_EVENT).is_empty());
    // With the switch on, the same walk confirms it, as for a Herdr session.
    waker.confirm_command_after = Some(Duration::from_secs(900));
    fx.sweeps(&mut waker, 1, 30);
    assert!(fx.confirmed(&worker, "s1").is_some());
    assert_eq!(fx.events(TRANSPORT_GONE_EVENT).len(), 1);
    assert_eq!(fx.confirmed(&anchor, "s2"), None);
}

/// Which transport a hook is in: `capture_env`, `TERM_PROGRAM` for nested multiplexers,
/// then config order. The hook then makes the session's row and stores no wake target; a
/// refused config leaves only Herdr and never fails the hook.
#[test]
fn capture_uses_capture_env_and_term_program() {
    fn found(config: &Config, env: &[(&str, &str)]) -> Option<(String, String)> {
        target_in(config, |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    }
    fn is(name: &str, target: &str) -> Option<(String, String)> {
        Some((name.to_string(), target.to_string()))
    }
    let example = transports::example_config();
    let herdr = ("HERDR_PANE_ID", "w1:p1");
    let tmux = ("TMUX_PANE", "%4");
    let cmux = ("CMUX_SURFACE_ID", "surface:2");
    let term = |program| ("TERM_PROGRAM", program);
    // One candidate wins outright, whatever TERM_PROGRAM says.
    assert_eq!(found(&example, &[]), None);
    assert_eq!(found(&example, &[herdr]), is("herdr", "w1:p1"));
    assert_eq!(found(&example, &[tmux]), is("tmux", "%4"));
    assert_eq!(found(&example, &[cmux]), is("cmux", "surface:2"));
    assert_eq!(found(&example, &[tmux, term("herdr")]), is("tmux", "%4"));
    assert_eq!(
        found(&example, &[cmux, term("Apple_Terminal")]),
        is("cmux", "surface:2")
    );
    // Nested: the one TERM_PROGRAM names, else Herdr, else the first the config lists.
    assert_eq!(found(&example, &[herdr, tmux]), is("herdr", "w1:p1"));
    assert_eq!(
        found(&example, &[herdr, tmux, term("tmux")]),
        is("tmux", "%4")
    );
    assert_eq!(
        found(&example, &[herdr, tmux, term("herdr")]),
        is("herdr", "w1:p1")
    );
    assert_eq!(
        found(&example, &[herdr, cmux, term("cmux")]),
        is("cmux", "surface:2")
    );
    assert_eq!(
        found(&example, &[herdr, cmux, term("tmux")]),
        is("herdr", "w1:p1")
    );
    assert_eq!(
        found(&example, &[cmux, tmux]),
        is("tmux", "%4"),
        "tmux is listed first"
    );
    assert_eq!(
        found(&example, &[tmux, cmux, term("cmux")]),
        is("cmux", "surface:2")
    );
    assert_eq!(
        found(&example, &[tmux, cmux, term("xterm")]),
        is("tmux", "%4")
    );
    assert_eq!(found(&example, &[herdr, tmux, cmux]), is("herdr", "w1:p1"));
    // A blank value is not a capture.
    assert_eq!(found(&example, &[("TMUX_PANE", "  ")]), None);
    assert_eq!(
        found(&example, &[("TMUX_PANE", ""), cmux]),
        is("cmux", "surface:2")
    );
    // The order is the config's, not the alphabet's.
    let reversed = Config::from_json(
        r#"{"version":1,"transports":{
            "zz":{"wake":[["z"]],"capture_env":"CMUX_SURFACE_ID"},
            "aa":{"wake":[["a"]],"capture_env":"TMUX_PANE"}}}"#,
    );
    assert_eq!(found(&reversed, &[tmux, cmux]), is("zz", "surface:2"));
    // No config, or a refused one: only Herdr.
    for config in [Config::default(), Config::from_json("{ not json")] {
        assert_eq!(found(&config, &[tmux, cmux]), None);
        assert_eq!(
            found(&config, &[herdr, tmux, term("tmux")]),
            is("herdr", "w1:p1")
        );
    }

    // Through the hooks of the real binary: the row is made.
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::at(&root, &root.join("home"));
    bin.json(&["agent", "register", "bob", "--program", "codex"]);
    let rows = |bin: &Bin| {
        bin.json(&["transport", "status"])["sessions"]
            .as_array()
            .unwrap()
            .clone()
    };
    // No config: a hook inside tmux records nothing, and stores no wake target.
    bin.hook("session-start", "s0", &[tmux]);
    assert!(rows(&bin).is_empty());
    assert!(bin.json(&["wake", "list"]).as_array().unwrap().is_empty());
    // Configured: a session row, as inside Herdr.
    transports::write_example(&bin.home);
    bin.hook("session-start", "s1", &[tmux]);
    let made = rows(&bin);
    assert_eq!(made.len(), 1, "{made:?}");
    assert_eq!(made[0]["transport"], "tmux");
    assert_eq!(made[0]["target"], "%4");
    assert_eq!(made[0]["session_id"], "s1");
    assert_eq!(made[0]["hook_made"], true);
    assert_eq!(made[0]["origin"], "hook");
    assert_eq!(made[0]["transport_configured"], true);
    assert!(bin.json(&["wake", "list"]).as_array().unwrap().is_empty());
    // Nested, in config order and by TERM_PROGRAM.
    bin.hook("session-start", "s2", &[herdr, tmux, term("tmux")]);
    bin.hook("session-start", "s3", &[herdr, tmux]);
    bin.hook("session-start", "s4", &[cmux, tmux]);
    bin.hook("prompt", "s5", &[cmux]);
    let place = |session: &str| {
        let row = rows(&bin)
            .into_iter()
            .find(|r| r["session_id"] == session)
            .unwrap_or_else(|| panic!("no row for {session}"));
        format!(
            "{} {}",
            row["transport"].as_str().unwrap(),
            row["target"].as_str().unwrap()
        )
    };
    assert_eq!(place("s2"), "tmux %4");
    assert_eq!(place("s3"), "herdr w1:p1");
    assert_eq!(place("s4"), "tmux %4");
    assert_eq!(place("s5"), "cmux surface:2");
    // The session-end hook deletes that session's row only.
    bin.hook("session-end", "s1", &[tmux]);
    assert_eq!(rows(&bin).len(), 4);
    // A refused config: the hook still exits 0, records a Herdr session, and nothing for
    // tmux.
    transports::write(&bin.home, "{ not json");
    bin.hook("session-start", "s6", &[herdr, tmux, term("tmux")]);
    bin.hook("session-start", "s7", &[tmux]);
    assert_eq!(place("s6"), "herdr w1:p1");
    assert!(!rows(&bin).iter().any(|r| r["session_id"] == "s7"));
    // A captured value with a character that cannot be seen is not a target: no row, and
    // the hook does not fail.
    transports::write_example(&bin.home);
    for (n, hidden) in INVISIBLE.iter().enumerate() {
        let session = format!("hidden-{n}");
        let pane = format!("p{hidden}evil");
        bin.hook("session-start", &session, &[("TMUX_PANE", &pane)]);
        assert!(
            !rows(&bin)
                .iter()
                .any(|r| r["session_id"] == session.as_str()),
            "{pane:?} became a target"
        );
    }
    assert!(
        bin.calls().is_empty(),
        "a hook ran a transport: {:?}",
        bin.calls()
    );
}

// ---------------------------------------------------------------------------
// The real binary, with fake tmux, cmux and herdr first on a PATH that holds nothing else
// but the system's own tools.

/// Records `argv` under `bin-calls/<n>/<i>`, the program's name first.
const FAKE_BIN: &str = r#"#!/bin/sh
dir='@DIR@'
n=$(ls "$dir/bin-calls" | wc -l | tr -d ' ')
mkdir "$dir/bin-calls/$n"
printf '%s' '@TOOL@' > "$dir/bin-calls/$n/0"
i=1
for a in "$@"; do printf '%s' "$a" > "$dir/bin-calls/$n/$i"; i=$((i+1)); done
if [ '@TOOL@' = herdr ]; then
  printf '{"id":"cli:agent:prompt","result":{"agent":{"agent_status":"working"},"type":"agent_prompted"}}\n'
fi
exit 0
"#;

struct Bin {
    root: PathBuf,
    home: PathBuf,
}

impl Bin {
    fn at(root: &Path, home: &Path) -> Bin {
        let bin = root.join("bin");
        for sub in ["bin", "bin-calls", "user", "cwd"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        for tool in ["tmux", "cmux", "herdr"] {
            script(&bin.join(tool), &FAKE_BIN.replace("@TOOL@", tool), root);
        }
        Bin {
            root: root.to_path_buf(),
            home: home.to_path_buf(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        // The fakes, then the system's own tools: no real tmux, cmux or herdr is on it.
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        c.args(args)
            .current_dir(self.root.join("cwd"))
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", self.root.join("user"))
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

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "another {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout).into_owned()
    }

    /// A command that must fail: its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "another {args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// Run one hook of bob's session `session` with `env` set.
    fn hook(&self, event: &str, session: &str, env: &[(&str, &str)]) {
        self.hook_as("bob", event, session, env);
    }

    /// Run one hook of `agent`'s session `session` with `env` set.
    fn hook_as(&self, agent: &str, event: &str, session: &str, env: &[(&str, &str)]) {
        let mut cmd = self.cmd(&["hook", "run", event, "--client", "codex"]);
        cmd.env("ANOTHER_AGENT", agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        let input = json!({ "session_id": session, "cwd": self.root.join("cwd") });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "{event} hook failed");
    }

    /// `doctor --json` against a closed loopback port.
    fn doctor(&self) -> (Option<i32>, Value) {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let out = self.run(&["--json", "doctor", "--bind", &format!("127.0.0.1:{port}")]);
        let report = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
        });
        (out.status.code(), report)
    }

    /// Every call of a fake so far, in order, each as its exact argv.
    fn calls(&self) -> Vec<Vec<String>> {
        let mut calls = BTreeMap::new();
        for entry in std::fs::read_dir(self.root.join("bin-calls")).unwrap() {
            let entry = entry.unwrap();
            let n: usize = entry.file_name().to_str().unwrap().parse().unwrap();
            let mut args = Vec::new();
            while let Ok(bytes) = std::fs::read(entry.path().join(args.len().to_string())) {
                args.push(String::from_utf8(bytes).unwrap());
            }
            calls.insert(n, args);
        }
        calls.into_values().collect()
    }

    /// A home as a build with tmux and cmux built in left it: wake targets stored through
    /// that build's `another wake set` and its tmux session hook, and the row a wake by
    /// hand left. The schema is this build's: the removal changed no table.
    fn old_home(root: &Path) -> Bin {
        let bin = Bin::at(root, &root.join("home"));
        for name in ["alice", "bob", "carol", "dave"] {
            bin.json(&["agent", "register", name, "--program", "claude"]);
        }
        let db = rusqlite::Connection::open(bin.home.join(DB_FILE)).unwrap();
        for (agent, transport, target, auto) in [
            ("bob", "tmux", "%3", 0),
            ("carol", "cmux", "notify", 0),
            ("dave", "tmux", "%7", 1),
        ] {
            let stored = db
                .execute(
                    "INSERT INTO wake_targets (agent_id, via, target, auto, updated_at)
                     SELECT agent_id, ?1, ?2, ?3, 0 FROM agents WHERE address = ?4",
                    rusqlite::params![transport, target, auto, agent],
                )
                .unwrap();
            assert_eq!(stored, 1, "{agent}");
        }
        let left = db
            .execute(
                "INSERT INTO transport_sessions
                   (agent_id, session_id, transport, target, last_wake_outcome, last_wake_at)
                 SELECT agent_id, '', 'tmux', '%3', 'unconfirmed', ?1 FROM agents
                 WHERE address = 'bob'",
                [real_now()],
            )
            .unwrap();
        assert_eq!(left, 1);
        drop(db);
        bin.json(&[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "Rebase",
        ]);
        bin
    }
}

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} row in {report}"))
}

/// A target whose transport nothing provides is refused by a wake, with the reason and
/// where to read on; `wake list` and `doctor` flag it; `doctor` names an old cmux
/// `notify` target; `rex` is reserved.
#[test]
fn targets_for_an_unconfigured_transport_are_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::old_home(&root);

    // A wake refuses, types nothing, and says what to set.
    let out = bin.run(&["--json", "wake", "--agent", "bob"]);
    assert!(!out.status.success(), "a refused wake does not exit 0");
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan[0]["sent"], false, "{plan}");
    assert_eq!(plan[0]["outcome"], Value::Null);
    let why = plan[0]["error"].as_str().unwrap();
    for part in [
        "nothing was typed",
        "no transport named \"tmux\" is configured",
        "transports.json",
        "docs/WAKE.md",
    ] {
        assert!(why.contains(part), "{part:?} is not in {why:?}");
    }
    assert!(String::from_utf8_lossy(&out.stderr).contains("docs/WAKE.md"));
    let text = bin.text(&["wake", "--agent", "bob"]);
    assert!(text.starts_with("failed bob: not woken"), "{text}");
    for agent in ["carol", "dave"] {
        let err = bin.refused(&["wake", "--agent", agent, "--force"]);
        assert!(
            err.contains("is configured") && err.contains("docs/WAKE.md"),
            "{err}"
        );
    }
    // `--all` wakes no stored target, as before.
    let all = bin.json(&["wake", "--all", "--force"]);
    assert!(
        all.as_array().unwrap().iter().all(|p| p["sent"] == false),
        "{all}"
    );

    // `wake list` marks each one.
    let listed = bin.json(&["wake", "list"]);
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 3, "the stored targets are kept: {listed:?}");
    for target in listed {
        assert_eq!(target["configured"], false, "{target}");
        let note = target["note"].as_str().unwrap();
        assert!(
            note.contains("transports.json") && note.contains("docs/WAKE.md"),
            "{note}"
        );
    }
    let text = bin.text(&["wake", "list"]);
    assert_eq!(
        text.matches("not usable: no transport named").count(),
        3,
        "{text}"
    );
    // So does `transport status`, for the row a wake by hand left.
    let status = bin.json(&["transport", "status"]);
    assert_eq!(
        status["sessions"][0]["transport_configured"], false,
        "{status}"
    );
    assert_eq!(status["config_error"], Value::Null);
    let text = bin.text(&["transport", "status"]);
    assert!(text.contains("transport not configured"), "{text}");

    // `doctor` names every target and row, and exits 0: a warning.
    let (code, report) = bin.doctor();
    assert_eq!(code, Some(0), "{report}");
    let row = check(&report, "transports");
    assert_eq!(row["status"], "warn", "{report}");
    let detail = row["detail"].as_str().unwrap();
    for part in [
        "the wake target of bob uses the transport \"tmux\", which is not configured",
        "the wake target of carol uses the transport \"cmux\", which is not configured",
        "the wake target of dave uses the transport \"tmux\", which is not configured",
        "session rows use the transport \"tmux\", which is not configured",
    ] {
        assert!(detail.contains(part), "{part:?} is not in {detail:?}");
    }
    assert!(
        row["fix"].as_str().unwrap().contains("transports.json"),
        "{row}"
    );

    // `--via`: herdr or a configured name. `rex` is reserved.
    for args in [
        vec!["wake", "--agent", "bob", "--via", "rex"],
        vec!["wake", "--all", "--via", "rex"],
        vec![
            "wake", "set", "--agent", "bob", "--via", "rex", "--target", "x",
        ],
    ] {
        let err = bin.refused(&args);
        assert!(
            err.contains("the rex transport is not in this build"),
            "{args:?}: {err}"
        );
    }
    for args in [
        vec!["wake", "--agent", "bob", "--via", "tmux", "--target", "%3"],
        vec!["wake", "--all", "--via", "tmux"],
        vec![
            "wake", "set", "--agent", "bob", "--via", "tmux", "--target", "%3",
        ],
        vec![
            "wake", "set", "--agent", "bob", "--via", "screen", "--target", "x",
        ],
    ] {
        let err = bin.refused(&args);
        assert!(
            err.contains("is configured") && err.contains("transports.json"),
            "{err}"
        );
    }
    let err = bin.refused(&[
        "wake", "set", "--agent", "bob", "--via", "No Such", "--target", "x",
    ]);
    assert!(err.contains("cannot name a transport"), "{err}");

    // Nothing ran, nothing was written for the owner, and the targets are as they were.
    assert!(bin.calls().is_empty(), "{:?}", bin.calls());
    assert!(
        !bin.home.join("transports.json").exists(),
        "Another wrote a config"
    );
    assert_eq!(bin.json(&["wake", "list"]).as_array().unwrap().len(), 3);

    // With the transports configured, doctor names what is left: the cmux `notify` target.
    transports::write_example(&bin.home);
    let (_, report) = bin.doctor();
    let row = check(&report, "transports");
    assert_eq!(row["status"], "warn", "{report}");
    let detail = row["detail"].as_str().unwrap();
    assert!(
        detail.contains("the wake target of carol is the cmux target `notify`"),
        "{detail}"
    );
    assert!(!detail.contains("not configured"), "{detail}");
    assert!(
        !detail.contains("was not found"),
        "the fakes are on PATH: {detail}"
    );
    let carol = bin.json(&["wake", "list"]);
    let carol = carol
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["agent"] == "carol")
        .unwrap()
        .clone();
    assert_eq!(carol["configured"], true);
    assert!(
        carol["note"].as_str().unwrap().contains("cmux-notify"),
        "{carol}"
    );
    // A program that is not there is named.
    transports::write(
        &bin.home,
        r#"{"version":1,"transports":{"tmux":{"wake":[["no-such-program-q9","{target}"]]},
           "cmux":{"wake":[["cmux","send","{target}"]]}}}"#,
    );
    let (_, report) = bin.doctor();
    let detail = check(&report, "transports")["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        detail.contains("tmux: the program \"no-such-program-q9\" was not found"),
        "{detail}"
    );
    assert!(!detail.contains("\"cmux\" was not found"), "{detail}");
}

/// Upgrade: a home that has tmux and cmux wake targets from a build with those adapters
/// built in. Nothing is migrated and nothing is lost. Until the owner writes
/// `transports.json` each wake is refused with what to set; with the documented example in
/// place the same commands run as before.
#[test]
fn an_upgraded_home_with_tmux_and_cmux_targets_works_once_the_example_is_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::old_home(&root);
    let before = bin.json(&["wake", "list"]);

    // Before: refused, with the file to write.
    let err = bin.refused(&["wake", "--agent", "bob"]);
    assert!(
        err.contains("no transport named \"tmux\" is configured")
            && err.contains(&bin.home.join("transports.json").display().to_string()),
        "{err}"
    );
    // A hook inside tmux records nothing yet, and is not an error.
    bin.hook("session-start", "s1", &[("TMUX_PANE", "%3")]);
    assert_eq!(
        bin.json(&["transport", "status"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(bin.calls().is_empty());

    // The owner copies the documented example.
    transports::write_example(&bin.home);
    let after = bin.json(&["wake", "list"]);
    for (old, new) in before
        .as_array()
        .unwrap()
        .iter()
        .zip(after.as_array().unwrap())
    {
        for key in ["agent", "via", "target", "auto", "updated_at"] {
            assert_eq!(old[key], new[key], "the stored target is untouched");
        }
    }

    // The target set by hand: the two tmux commands of the old adapter, exactly.
    let plan = bin.json(&["wake", "--agent", "bob"]);
    assert_eq!(plan[0]["outcome"], "unconfirmed", "{plan}");
    assert_eq!(plan[0]["via"], "tmux");
    let line = plan[0]["message"].as_str().unwrap();
    assert!(line.starts_with("Another: 1 unread message(s)") && line.contains("\"Rebase\""));
    assert_eq!(
        bin.calls(),
        [
            vec!["tmux", "send-keys", "-t", "%3", "-l", line],
            vec!["tmux", "send-keys", "-t", "%3", "Enter"],
        ]
    );
    let text = bin.text(&["wake", "--agent", "bob"]);
    assert!(
        text.contains("delivered, not confirmed")
            && text.contains("tmux cannot report whether the agent started"),
        "{text}"
    );
    // The pane an old hook captured is still reached by a wake by name.
    let plan = bin.json(&["wake", "--agent", "dave", "--force", "--message", "standup"]);
    assert_eq!(plan[0]["outcome"], "unconfirmed", "{plan}");
    let calls = bin.calls();
    assert_eq!(
        calls[calls.len() - 2],
        ["tmux", "send-keys", "-t", "%7", "-l", "standup"]
    );
    // cmux `send`, with its one escape.
    let plan = bin.json(&[
        "wake",
        "--agent",
        "alice",
        "--force",
        "--via",
        "cmux",
        "--target",
        "surface:1",
        "--message",
        "a\\nb",
    ]);
    assert_eq!(plan[0]["outcome"], "unconfirmed", "{plan}");
    assert_eq!(
        bin.calls().last().unwrap(),
        &["cmux", "send", "--surface", "surface:1", "a/nb\\n"]
    );

    // The cmux `notify` target is refused with what replaces it, and nothing runs.
    let ran = bin.calls().len();
    let err = bin.refused(&["wake", "--agent", "carol", "--force"]);
    assert!(
        err.contains("notify") && err.contains("cmux-notify"),
        "{err}"
    );
    let err = bin.refused(&[
        "wake", "set", "--agent", "carol", "--via", "cmux", "--target", "notify",
    ]);
    assert!(err.contains("cmux-notify"), "{err}");
    assert_eq!(bin.calls().len(), ran);
    bin.json(&[
        "wake",
        "set",
        "--agent",
        "carol",
        "--via",
        "cmux-notify",
        "--target",
        "notify",
    ]);
    bin.json(&["wake", "--agent", "carol", "--force", "--message", "ping"]);
    assert_eq!(
        bin.calls().last().unwrap(),
        &["cmux", "notify", "--title", "Another", "--body", "ping"]
    );

    // A hook inside tmux now gives the session a row, and stores no wake target.
    bin.hook("session-start", "s1", &[("TMUX_PANE", "%3")]);
    let status = bin.json(&["transport", "status"]);
    let row = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap_or_else(|| panic!("no row: {status}"))
        .clone();
    assert_eq!(row["transport"], "tmux");
    assert_eq!(row["target"], "%3");
    assert_eq!(bin.json(&["wake", "list"]).as_array().unwrap().len(), 3);

    // Everything is in order now.
    let (code, report) = bin.doctor();
    assert_eq!(code, Some(0));
    let row = check(&report, "transports");
    assert_eq!(row["status"], "ok", "{report}");
    assert_eq!(
        row["detail"],
        "herdr and tmux, cmux, cmux-notify from transports.json"
    );
    for target in bin.json(&["wake", "list"]).as_array().unwrap() {
        assert_eq!(target["configured"], true, "{target}");
        assert_eq!(target["note"], Value::Null, "{target}");
    }
}

/// Nothing but the owner's file can set a command: no command of the binary creates or
/// changes `transports.json`, and a transport name that is not in it is refused wherever
/// one can be given.
#[test]
fn another_never_writes_the_transports_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::old_home(&root);
    let path = bin.home.join("transports.json");
    let ran = |bin: &Bin| {
        let _ = bin.run(&["wake", "--agent", "bob"]);
        let _ = bin.run(&["wake", "--all", "--force"]);
        let _ = bin.run(&[
            "wake", "set", "--agent", "bob", "--via", "herdr", "--target", "p",
        ]);
        let _ = bin.run(&[
            "wake", "set", "--agent", "bob", "--via", "tmux", "--target", "%1",
        ]);
        let _ = bin.run(&["wake", "list"]);
        let _ = bin.run(&["wake", "prune"]);
        let _ = bin.run(&["wake", "clear", "--agent", "dave"]);
        let _ = bin.run(&["transport", "status"]);
        let _ = bin.doctor();
        bin.hook("session-start", "s1", &[("TMUX_PANE", "%3")]);
        bin.hook("prompt", "s1", &[("TMUX_PANE", "%3")]);
        bin.hook("session-end", "s1", &[("TMUX_PANE", "%3")]);
    };
    ran(&bin);
    assert!(!path.exists(), "a command created the config");
    transports::write_example(&bin.home);
    let before = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    ran(&bin);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

/// The reviewer's reproduction (review R1 of Q9, finding 1). A pane an earlier build
/// captured is forgotten when its agent's session ends, also with no `transports.json`:
/// forgetting runs no command and needs no config. So a config added later finds nothing
/// to type into. A target set by hand is kept, and a Herdr capture behaves as before.
#[test]
fn a_captured_pane_is_forgotten_at_session_end_with_no_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::old_home(&root);
    let db = rusqlite::Connection::open(bin.home.join(DB_FILE)).unwrap();
    db.execute(
        "INSERT INTO wake_targets (agent_id, via, target, auto, updated_at)
         SELECT agent_id, 'herdr', 'w1:p1', 1, 0 FROM agents WHERE address = 'alice'",
        [],
    )
    .unwrap();
    drop(db);
    bin.json(&[
        "send",
        "--from",
        "alice",
        "--to",
        "dave",
        "--subject",
        "x \"$(touch /tmp/never)\"",
    ]);
    let stored = |bin: &Bin| -> Vec<String> {
        bin.json(&["wake", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                format!(
                    "{} {} {}",
                    t["agent"].as_str().unwrap(),
                    t["via"].as_str().unwrap(),
                    t["target"].as_str().unwrap()
                )
            })
            .collect()
    };
    assert_eq!(stored(&bin).len(), 4);

    // dave's session starts and ends inside tmux, with no config.
    let in_tmux = [("TMUX_PANE", "%7")];
    bin.hook_as("dave", "session-start", "d1", &in_tmux);
    assert!(stored(&bin).contains(&"dave tmux %7".to_string()));
    bin.hook_as("dave", "session-end", "d1", &in_tmux);
    assert!(
        !stored(&bin).iter().any(|t| t.starts_with("dave ")),
        "the captured pane outlived its session: {:?}",
        stored(&bin)
    );
    // bob's target was set by hand: his session's end keeps it.
    bin.hook_as("bob", "session-start", "b1", &[("TMUX_PANE", "%3")]);
    bin.hook_as("bob", "session-end", "b1", &[("TMUX_PANE", "%3")]);
    assert!(stored(&bin).contains(&"bob tmux %3".to_string()));
    // A Herdr capture is forgotten only by a session that ends in that pane, as before.
    bin.hook_as(
        "alice",
        "session-start",
        "a1",
        &[("HERDR_PANE_ID", "w9:p9")],
    );
    bin.hook_as("alice", "session-end", "a1", &[("HERDR_PANE_ID", "w9:p9")]);
    assert!(stored(&bin).contains(&"alice herdr w1:p1".to_string()));
    bin.hook_as(
        "alice",
        "session-start",
        "a2",
        &[("HERDR_PANE_ID", "w1:p1")],
    );
    bin.hook_as("alice", "session-end", "a2", &[("HERDR_PANE_ID", "w1:p1")]);
    assert!(!stored(&bin).iter().any(|t| t.starts_with("alice ")));

    // The owner now writes the config and wakes dave: nothing is typed anywhere.
    transports::write_example(&bin.home);
    let plan = bin.json(&["wake", "--agent", "dave"]);
    assert_eq!(plan[0]["sent"], false, "{plan}");
    assert_eq!(plan[0]["outcome"], Value::Null, "{plan}");
    assert!(
        plan[0]["skipped"]
            .as_str()
            .unwrap()
            .contains("no wake target"),
        "{plan}"
    );
    let forced = bin.json(&["wake", "--agent", "dave", "--force"]);
    assert_eq!(forced[0]["sent"], false, "{forced}");
    assert!(bin.calls().is_empty(), "{:?}", bin.calls());
}

/// A wake by hand to a command transport cannot check who is in the pane. So when the
/// target is a session row or a pane an earlier build captured, it is refused, with
/// nothing typed, if the row was last seen gone or the session or agent has shown no sign
/// of life within the limit pruning uses. `--force` overrides. A target set by hand is
/// the owner's word and is not subject to this.
#[test]
fn a_wake_by_hand_refuses_a_gone_or_quiet_command_target_unless_forced() {
    let fx = Fx::new();
    fx.write_tmux_example();
    for pane in ["%1", "%2", "%5", "%6"] {
        fx.pane(pane, true);
    }
    let sends = |fx: &Fx| {
        fx.tmux_calls()
            .iter()
            .filter(|c| c.contains("[send-keys]"))
            .count()
    };
    let refused = |fx: &Fx, agent: &str, want: &str| {
        let before = sends(fx);
        let plan = wake(&fx.store, agent, &WakeOptions::default()).unwrap();
        assert!(plan.outcome.is_none() && !plan.sent, "{plan:?}");
        let why = plan.error.unwrap();
        assert!(why.contains("nothing was typed"), "{why}");
        assert!(why.contains(want), "{want:?} is not in {why:?}");
        assert!(why.contains("--force"), "{why}");
        assert_eq!(sends(fx), before, "something was typed");
    };
    let forced = |fx: &Fx, agent: &str| {
        let before = sends(fx);
        let options = WakeOptions {
            force: true,
            ..Default::default()
        };
        let plan = wake(&fx.store, agent, &options).unwrap();
        assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
        assert_eq!(sends(fx), before + 2);
    };
    let typed = |fx: &Fx, agent: &str| {
        let before = sends(fx);
        let plan = wake(&fx.store, agent, &WakeOptions::default()).unwrap();
        assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
        assert_eq!(sends(fx), before + 2);
    };
    let db = || rusqlite::Connection::open(fx.home.join(DB_FILE)).unwrap();

    // A session row last seen gone. The pane id is in use again (tmux reuses ids), which
    // is exactly when typing would reach someone else.
    let worker = fx.agent("worker");
    fx.mail(&worker, "Hello");
    fx.stamp(&worker, "s1", "tmux", "%1");
    typed(&fx, &worker);
    fx.pane("%1", false);
    let mut waker = Waker::with_transports(Vec::new());
    waker.sweep(&fx.store).unwrap();
    assert_eq!(fx.row(&worker, "s1").state.as_deref(), Some("gone"));
    fx.pane("%1", true);
    refused(&fx, &worker, "gone");
    forced(&fx, &worker);

    // A session row whose hooks have been quiet for longer than a day.
    let quiet = fx.agent("quiet");
    fx.mail(&quiet, "Hello");
    fx.stamp(&quiet, "s2", "tmux", "%2");
    fx.advance(23 * 3600);
    typed(&fx, &quiet);
    fx.advance(2 * 3600);
    refused(&fx, &quiet, "no hook of that session for over a day");
    forced(&fx, &quiet);
    // Its next hook makes it wakeable again.
    fx.stamp(&quiet, "s2", "tmux", "%2");
    typed(&fx, &quiet);

    // A pane an earlier build captured: refused once its agent ended its session or has
    // not been seen for a day, which is when pruning drops it.
    let old = fx.agent("old");
    fx.mail(&old, "Hello");
    fx.store
        .set_wake_target(&old, &via("tmux"), "%5", true)
        .unwrap();
    let seen = |status: &str, at: i64| {
        let changed = db()
            .execute(
                "UPDATE agents SET status = ?1, last_seen_at = ?2 WHERE name = 'old'",
                rusqlite::params![status, at],
            )
            .unwrap();
        assert_eq!(changed, 1);
    };
    let now = fx.clock.load(Ordering::SeqCst);
    seen("active", now);
    typed(&fx, &old);
    seen("offline", now);
    refused(&fx, &old, "ended its session");
    forced(&fx, &old);
    seen("active", now - 25 * 3_600_000);
    refused(&fx, &old, "not seen for over a day");
    let pruned = another::wake::prune(&fx.store, true).unwrap();
    let row = pruned.iter().find(|r| r.agent == old).unwrap();
    assert!(
        row.pruned,
        "a wake refuses what pruning would drop: {row:?}"
    );
    assert_eq!(row.reason, "agent not seen for over a day");

    // A target the owner set with `another wake set`: woken as asked, whatever the agent's
    // state.
    let hand = fx.agent("hand");
    fx.mail(&hand, "Hello");
    fx.store
        .set_wake_target(&hand, &via("tmux"), "%6", false)
        .unwrap();
    db().execute(
        "UPDATE agents SET status = 'offline', last_seen_at = 0 WHERE name = 'hand'",
        [],
    )
    .unwrap();
    typed(&fx, &hand);
    // So is `--target`.
    let to_target = WakeOptions {
        via: Some(via("tmux")),
        target: Some("%6".into()),
        ..Default::default()
    };
    let plan = wake(&fx.store, &old, &to_target).unwrap();
    assert_eq!(plan.outcome, Some(WakeOutcome::Unconfirmed), "{plan:?}");
}

/// A `transports.json` that is not a regular file is refused without being opened, so a
/// FIFO or a device there cannot hang a hook or any command.
#[test]
fn a_config_that_is_not_a_regular_file_is_refused_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let bin = Bin::at(&root, &root.join("home"));
    bin.json(&["agent", "register", "bob", "--program", "codex"]);
    let path = bin.home.join("transports.json");
    // Loading on another thread: a load that blocks fails the test instead of hanging it.
    let load = |home: &Path| {
        let (tx, rx) = std::sync::mpsc::channel();
        let home = home.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(load_config(&home));
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("load_config blocked on a file that is not regular")
    };

    // A FIFO with no writer: opening it for reading would block for ever.
    let made = Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(made.success());
    let config = load(&bin.home);
    assert!(config.transports.is_empty());
    let error = config.error.unwrap();
    assert!(error.contains("not a regular file"), "{error}");
    // Hooks, and the commands that print the reason, return.
    bin.hook("session-start", "s1", &[("TMUX_PANE", "%1")]);
    bin.hook("prompt", "s1", &[("HERDR_PANE_ID", "w1:p1")]);
    let status = bin.json(&["transport", "status"]);
    assert!(
        status["config_error"]
            .as_str()
            .unwrap()
            .contains("not a regular file"),
        "{status}"
    );
    let (_, report) = bin.doctor();
    assert_eq!(check(&report, "transports")["status"], "warn", "{report}");
    std::fs::remove_file(&path).unwrap();

    // A device, reached through a symbolic link.
    std::os::unix::fs::symlink("/dev/null", &path).unwrap();
    let error = load(&bin.home).error.unwrap();
    assert!(error.contains("not a regular file"), "{error}");
    std::fs::remove_file(&path).unwrap();
    // A directory.
    std::fs::create_dir(&path).unwrap();
    let error = load(&bin.home).error.unwrap();
    assert!(error.contains("not a regular file"), "{error}");
    std::fs::remove_dir(&path).unwrap();
    // A link to a regular file is read, and a link to nothing is no config.
    transports::write(&root, &transports::example());
    std::os::unix::fs::symlink(root.join("transports.json"), &path).unwrap();
    let linked = load(&bin.home);
    assert_eq!(linked.error, None);
    assert_eq!(linked.transports.len(), 3);
    std::fs::remove_file(root.join("transports.json")).unwrap();
    let dangling = load(&bin.home);
    assert!(dangling.error.is_none() && dangling.transports.is_empty());
}

/// A command that never returns is stopped at its deadline: a wake command is `unknown`
/// (the line may be in the terminal), and a status or list is no answer. Stops a child, so
/// it runs on a hosted runner only.
#[test]
fn a_command_that_hangs_is_stopped_at_its_deadline() {
    hosted_ci::require_hosted_runner();
    let fx = Fx::new();
    let tool = fx.tool();
    let config = config(json!({"version": 1, "transports": {
        "first": {"wake": [[tool, "hang", "{message}"]]},
        "later": {"wake": [[tool, "one", "{message}"], [tool, "hang"]]},
        "ask": {"wake": [[tool, "one", "{message}"]],
                "status": {"argv": [tool, "hang"], "exit_codes": {"0": "idle", "3": "working"}},
                "list": {"argv": [tool, "hang"]}, "confirm_secs": 1}
    }}));
    let quick = |name: &str| {
        CommandTransport::new(config.get(name).unwrap().clone()).with_timing(
            Duration::from_millis(300),
            Duration::from_millis(300),
            Duration::from_millis(20),
        )
    };
    for name in ["first", "later"] {
        let result = quick(name).wake("p", "hi", None);
        assert_eq!(result.outcome, WakeOutcome::Unknown, "{name}: {result:?}");
        assert!(result.detail.contains("did not finish"), "{result:?}");
    }
    let ask = quick("ask");
    assert_eq!(ask.status("p", None).state, State::NoAnswer);
    assert!(matches!(ask.list(None), ListResult::NoAnswer(_)));
    assert_eq!(ask.wake("p", "hi", None).outcome, WakeOutcome::Unconfirmed);
}
