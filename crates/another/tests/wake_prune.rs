//! `another wake prune` for tmux and cmux targets, through the real binary. Herdr targets
//! are covered in `push.rs`. A fake `tmux` answers `display-message` (the read-only probe)
//! with success for a live pane and failure for one listed as dead. cmux has no probe, so
//! its targets are judged only by whether their agent ended its session.
//!
//! Nothing here signals a process: the fake exits at once.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

const FAKE_TMUX: &str = r#"#!/bin/sh
dir='@DIR@'
printf '%s\n' "$*" >> "$dir/calls.log"
if [ "$1" = "display-message" ]; then
  # display-message -p -t <pane> '#{pane_id}'
  if [ -f "$dir/dead/$4" ]; then echo "can't find pane: $4" >&2; exit 1; fi
  echo "$4"
  exit 0
fi
exit 0
"#;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["bin", "dead", "empty"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let tmux = root.join("bin/tmux");
        std::fs::write(&tmux, FAKE_TMUX.replace("@DIR@", &root.to_string_lossy())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = Env {
            home: root.join("home"),
            root,
            _dir: dir,
        };
        for name in ["alice", "bob", "carol", "dave"] {
            env.json(&["agent", "register", name, "--program", "codex"]);
        }
        env
    }

    /// The binary with the fake tmux first on PATH, or with only an empty directory on PATH
    /// so no tmux can be found, wherever the machine installs a real one (`/usr/bin` on
    /// Linux CI, which installs tmux for `wake_e2e`).
    fn cmd(&self, args: &[&str], with_tmux: bool) -> Command {
        let path = if with_tmux {
            format!("{}:/usr/bin:/bin", self.root.join("bin").display())
        } else {
            self.root.join("empty").display().to_string()
        };
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
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("TERM_PROGRAM")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::null());
        c
    }

    fn json_with(&self, args: &[&str], with_tmux: bool) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all, with_tmux).output().unwrap();
        assert!(
            out.status.success(),
            "another {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        self.json_with(args, true)
    }

    /// A session start inside a terminal, as a client's hook runs it: the target is
    /// captured automatically.
    fn start_in(&self, agent: &str, var: &str, target: &str) {
        let status = self
            .cmd(&["hook", "run", "session-start", "--client", "codex"], true)
            .env("ANOTHER_AGENT", agent)
            .env(var, target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn kill_pane(&self, pane: &str) {
        std::fs::write(self.root.join("dead").join(pane), "").unwrap();
    }

    fn tmux_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn targets(&self) -> Vec<String> {
        self.json(&["wake", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                format!(
                    "{}={}",
                    t["agent"].as_str().unwrap(),
                    t["target"].as_str().unwrap()
                )
            })
            .collect()
    }
}

fn row(rows: &Value, agent: &str) -> Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent"] == agent)
        .unwrap_or_else(|| panic!("no prune row for {agent}: {rows}"))
        .clone()
}

/// tmux: a live pane is kept and a gone one is dropped, `--dry-run` drops nothing, and the
/// probe only ever asks (`display-message`); it never types into a pane.
#[test]
fn tmux_targets_are_probed_and_only_gone_panes_are_dropped() {
    let env = Env::new();
    env.start_in("alice", "TMUX_PANE", "%1");
    env.start_in("bob", "TMUX_PANE", "%2");
    env.kill_pane("%2");
    assert_eq!(env.targets().len(), 2, "both targets were captured");

    let dry = env.json(&["wake", "prune", "--dry-run"]);
    assert_eq!(row(&dry, "alice")["pruned"], false, "{dry}");
    assert_eq!(row(&dry, "alice")["reason"], "live");
    assert_eq!(row(&dry, "bob")["pruned"], true, "{dry}");
    assert!(
        row(&dry, "bob")["reason"]
            .as_str()
            .unwrap()
            .contains("tmux reports the pane is gone"),
        "{dry}"
    );
    assert_eq!(env.targets().len(), 2, "a dry run drops nothing");

    env.json(&["wake", "prune"]);
    assert_eq!(env.targets(), ["alice=%1"]);
    assert!(
        env.tmux_calls()
            .iter()
            .all(|c| c.starts_with("display-message -p -t ")),
        "prune only probes: {:?}",
        env.tmux_calls()
    );
}

/// Without a tmux binary the pane cannot be asked about: the target is kept and the reason
/// says why, rather than being dropped as gone.
#[test]
fn a_missing_tmux_keeps_the_target() {
    let env = Env::new();
    env.start_in("alice", "TMUX_PANE", "%1");
    let rows = env.json_with(&["wake", "prune"], false);
    let r = row(&rows, "alice");
    assert_eq!(r["pruned"], false, "{rows}");
    assert!(
        r["reason"].as_str().unwrap().starts_with("kept (tmux"),
        "{rows}"
    );
    assert_eq!(env.targets(), ["alice=%1"]);
}

/// cmux targets are not probed: one whose agent is still around is kept, one whose agent
/// ended its session is dropped.
#[test]
fn cmux_targets_are_dropped_only_when_their_agent_left() {
    let env = Env::new();
    env.start_in("carol", "CMUX_SURFACE_ID", "surface:3");
    env.start_in("dave", "CMUX_SURFACE_ID", "surface:4");
    // dave goes offline elsewhere: his captured surface is left behind.
    env.json(&["heartbeat", "--agent", "dave", "--end"]);

    let rows = env.json(&["wake", "prune"]);
    let carol = row(&rows, "carol");
    assert_eq!(carol["pruned"], false, "{rows}");
    assert!(
        carol["reason"]
            .as_str()
            .unwrap()
            .contains("cmux targets are not probed"),
        "{rows}"
    );
    let dave = row(&rows, "dave");
    assert_eq!(dave["pruned"], true, "{rows}");
    assert_eq!(dave["reason"], "agent ended its session", "{rows}");
    assert_eq!(env.targets(), ["carol=surface:3"]);
}

/// A target set by hand is listed but never removed, and its pane is not even probed.
#[test]
fn hand_set_targets_are_kept_without_a_probe() {
    let env = Env::new();
    env.json(&[
        "wake", "set", "--agent", "bob", "--via", "tmux", "--target", "%9",
    ]);
    env.kill_pane("%9");
    let rows = env.json(&["wake", "prune"]);
    let r = row(&rows, "bob");
    assert_eq!(r["pruned"], false, "{rows}");
    assert_eq!(r["reason"], "set by hand; kept", "{rows}");
    assert_eq!(env.targets(), ["bob=%9"]);
    assert!(env.tmux_calls().is_empty(), "{:?}", env.tmux_calls());
}
