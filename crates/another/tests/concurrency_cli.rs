//! Concurrency stress through the real binary: 50 separate `another` processes race for one
//! exclusive hold and for one request. Exactly one process wins (exit 0), every other one
//! reports a conflict (exit 3), none fails any other way, and the stored state agrees.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

const AGENTS: usize = 50;

fn agent(i: usize) -> String {
    format!("agent{i:02}")
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("shop");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let env = Env {
            _dir: dir,
            home,
            project,
        };
        env.ok(&["init"]);
        env.ok(&["project", "ensure", &env.project.to_string_lossy()]);
        for name in std::iter::once("lead".to_string()).chain((0..AGENTS).map(agent)) {
            env.ok(&["agent", "register", &name, "--program", "other"]);
        }
        env
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .current_dir(&self.project)
            .env("ANOTHER_HOME", &self.home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_BIN_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "another {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Start one process per agent, all before waiting on any; return each exit code.
    fn race(&self, args_for: impl Fn(&str) -> Vec<String>) -> Vec<(String, i32, String)> {
        let children: Vec<(String, Child)> = (0..AGENTS)
            .map(|i| {
                let name = agent(i);
                let args = args_for(&name);
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let child = self.cmd(&refs).spawn().unwrap();
                (name, child)
            })
            .collect();
        children
            .into_iter()
            .map(|(name, child)| {
                let out = child.wait_with_output().unwrap();
                (
                    name,
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            })
            .collect()
    }
}

fn path_of(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Exactly one exit 0 and the rest exit 3; returns the winner.
fn one_winner(results: &[(String, i32, String)]) -> String {
    let winners: Vec<&String> = results
        .iter()
        .filter(|(_, code, _)| *code == 0)
        .map(|(n, _, _)| n)
        .collect();
    for (name, code, err) in results {
        assert!(
            *code == 0 || *code == 3,
            "{name} exited {code}, neither won nor conflicted: {err}"
        );
    }
    assert_eq!(winners.len(), 1, "winners: {winners:?}");
    winners[0].clone()
}

#[test]
fn fifty_processes_racing_for_one_hold_leave_one_holder() {
    let env = Env::new();
    let project = path_of(&env.project);
    let results = env.race(|name| {
        [
            "hold",
            "acquire",
            "--agent",
            name,
            "--project",
            &project,
            "src/race.rs",
        ]
        .map(String::from)
        .to_vec()
    });
    let winner = one_winner(&results);
    let holds: Value = serde_json::from_str(&env.ok(&["--json", "holds"])).unwrap();
    let on_file: Vec<&Value> = holds
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["pattern"] == "src/race.rs")
        .collect();
    assert_eq!(on_file.len(), 1, "{holds}");
    assert!(
        on_file[0]["agent"].as_str().unwrap().starts_with(&winner),
        "{} vs {winner}",
        on_file[0]["agent"]
    );
}

#[test]
fn fifty_processes_claiming_one_request_leave_one_owner() {
    let env = Env::new();
    let to: Vec<String> = (0..AGENTS).map(agent).collect();
    let sent: Value = serde_json::from_str(&env.ok(&[
        "--json",
        "send",
        "--from",
        "lead",
        "--to",
        &to.join(","),
        "--subject",
        "First one takes it",
        "--request",
    ]))
    .unwrap();
    let rid = sent["request_id"].as_str().unwrap().to_string();
    let results = env.race(|name| {
        ["request", "--agent", name, "claim", &rid]
            .map(String::from)
            .to_vec()
    });
    let winner = one_winner(&results);
    let requests: Value = serde_json::from_str(&env.ok(&["--json", "requests"])).unwrap();
    let req = requests
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == rid.as_str())
        .unwrap();
    assert_eq!(req["status"], "claimed");
    assert!(
        req["owner"].as_str().unwrap().starts_with(&winner),
        "{} vs {winner}",
        req["owner"]
    );
}
