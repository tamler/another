//! The Herdr adapter. Herdr is called with exactly three subcommands, each as its own
//! process with no shell: `agent prompt`, `agent get` and `agent list`. Never bare, and
//! never to focus a pane.
//!
//! Herdr answers a success as JSON on stdout with exit 0 and every server error as JSON on
//! stderr with exit 1; a syntax error exits 2.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{bare, ListResult, Observation, Session, State, Transport, WakeOutcome, WakeResult};
use crate::wake::{run_bounded, Bounded};

/// Whether a session Herdr stops listing may be confirmed gone. Off until the restart
/// check of Herdr (builder check B6) has a safe outcome and the architect has said so:
/// observation, attention and last-alive work either way.
pub const HERDR_CONFIRMS_GONE: bool = false;

/// Another's own deadline for `agent prompt`; Herdr is given `--timeout 8000`.
pub const WAKE_DEADLINE: Duration = Duration::from_secs(12);
/// Deadline for `agent get` and `agent list`.
pub const ASK_DEADLINE: Duration = Duration::from_secs(3);
/// Longest wake line, in characters.
const MAX_LINE_CHARS: usize = 1000;

pub struct HerdrTransport {
    program: String,
    wake_deadline: Duration,
    ask_deadline: Duration,
}

impl HerdrTransport {
    /// A transport that runs `program` as Herdr.
    pub fn new(program: impl Into<String>) -> HerdrTransport {
        HerdrTransport {
            program: program.into(),
            wake_deadline: WAKE_DEADLINE,
            ask_deadline: ASK_DEADLINE,
        }
    }

    /// `$HERDR_BIN_PATH`, else `herdr` on `PATH`.
    pub fn from_env() -> HerdrTransport {
        HerdrTransport::new(
            std::env::var("HERDR_BIN_PATH")
                .ok()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "herdr".to_string()),
        )
    }

    /// Other deadlines, for tests of a Herdr that never answers.
    pub fn with_deadlines(mut self, wake: Duration, ask: Duration) -> HerdrTransport {
        self.wake_deadline = wake;
        self.ask_deadline = ask;
        self
    }

    fn ask(&self, args: &[&str], cancel: Option<&AtomicBool>) -> Result<Answer, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        match run_bounded(&mut cmd, self.ask_deadline, cancel) {
            Ok(Bounded::Done(ran)) => Ok(Answer {
                code: ran.status.code(),
                stdout: ran.stdout,
                stderr: ran.stderr,
            }),
            Ok(Bounded::TimedOut) => Err(format!(
                "herdr did not answer within {}s",
                self.ask_deadline.as_secs()
            )),
            Ok(Bounded::TooMuchOutput) => Err("herdr wrote more than 1 MiB".into()),
            Err(e) => Err(format!("herdr: {e}")),
        }
    }
}

/// What one Herdr process left behind.
struct Answer {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Answer {
    /// The JSON of a success (stdout, exit 0).
    fn result(&self) -> Option<Value> {
        (self.code == Some(0))
            .then(|| serde_json::from_slice(&self.stdout).ok())
            .flatten()
    }

    /// The error code and message of a server error (stderr, a non-zero exit).
    fn error(&self) -> Option<(String, String)> {
        if self.code == Some(0) {
            return None;
        }
        let reply: Value = serde_json::from_slice(&self.stderr).ok()?;
        let text = |p: &str| reply.pointer(p).and_then(Value::as_str).map(str::to_string);
        Some((
            text("/error/code")?,
            text("/error/message").unwrap_or_default(),
        ))
    }

    fn exit(&self) -> String {
        match self.code {
            Some(code) => format!("herdr exited with exit status: {code}"),
            None => "herdr was stopped by a signal".into(),
        }
    }
}

/// One `AgentInfo` object as an observation.
fn observation(agent: &Value) -> Observation {
    let text = |k: &str| {
        agent
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let status = text("agent_status").unwrap_or_else(|| "missing".into());
    let state = match status.as_str() {
        "working" => State::Working,
        // After a turn Herdr says `done`; `idle` only at a fresh start.
        "idle" | "done" => State::Idle,
        "blocked" => State::Blocked,
        _ => State::Unknown,
    };
    // Only an id identifies a session; a transcript path does not.
    let session_id = agent
        .pointer("/agent_session/value")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty() && agent.pointer("/agent_session/kind") == Some(&Value::from("id"))
        })
        .map(str::to_string);
    Observation {
        state,
        session_id,
        program: text("agent"),
        cwd: text("cwd"),
        detail: another_core::single_line(&format!("herdr: {status}"), 300),
    }
}

fn map_wake(answer: &Answer) -> WakeResult {
    if answer.code == Some(0) {
        let reply = answer.result();
        let kind = reply
            .as_ref()
            .and_then(|r| r.pointer("/result/type"))
            .and_then(Value::as_str);
        return if kind == Some("agent_prompted") {
            let status = reply
                .as_ref()
                .and_then(|r| r.pointer("/result/agent/agent_status"))
                .and_then(Value::as_str)
                .unwrap_or("no status");
            WakeResult::new(
                WakeOutcome::Started,
                format!("herdr: agent_prompted ({status})"),
            )
        } else {
            WakeResult::new(
                WakeOutcome::Unknown,
                "herdr exited with exit status: 0 and no readable answer",
            )
        };
    }
    if answer.code == Some(2) {
        return WakeResult::new(
            WakeOutcome::Undelivered,
            format!("{} (it refused the command)", answer.exit()),
        );
    }
    let Some((code, message)) = answer.error() else {
        return WakeResult::new(
            WakeOutcome::Unknown,
            format!("{} and no readable answer", answer.exit()),
        );
    };
    let outcome = match code.as_str() {
        "agent_prompt_stalled" => WakeOutcome::NotStarted,
        "agent_blocked" => WakeOutcome::RefusedBlocked,
        "agent_not_found" => WakeOutcome::Gone,
        // The prompt was delivered; Herdr did not see what followed in time.
        "timeout" => WakeOutcome::Unconfirmed,
        _ => WakeOutcome::Unknown,
    };
    WakeResult::new(outcome, format!("herdr: {code}: {message}"))
}

fn map_status(answer: &Answer) -> Observation {
    if let Some(agent) = answer
        .result()
        .as_ref()
        .and_then(|r| r.pointer("/result/agent"))
    {
        return observation(agent);
    }
    match answer.error() {
        // The same code for a missing pane and for a pane whose agent has exited.
        Some((code, message)) if code == "agent_not_found" => {
            bare(State::Gone, format!("herdr: {code}: {message}"))
        }
        Some((code, message)) => bare(State::NoAnswer, format!("herdr: {code}: {message}")),
        None => bare(
            State::NoAnswer,
            format!("{} and no readable answer", answer.exit()),
        ),
    }
}

fn map_list(answer: &Answer) -> ListResult {
    let reply = answer.result();
    let Some(agents) = reply
        .as_ref()
        .and_then(|r| r.pointer("/result/agents"))
        .and_then(Value::as_array)
    else {
        return ListResult::NoAnswer(match answer.error() {
            Some((code, message)) => format!("herdr: {code}: {message}"),
            None => format!("{} and no readable list", answer.exit()),
        });
    };
    let mut sessions = Vec::new();
    for agent in agents {
        let Some(target) = agent.get("pane_id").and_then(Value::as_str) else {
            return ListResult::NoAnswer("herdr listed an agent with no pane_id".into());
        };
        sessions.push(Session {
            target: target.to_string(),
            observation: observation(agent),
        });
    }
    ListResult::Sessions(sessions)
}

impl Transport for HerdrTransport {
    fn name(&self) -> &str {
        "herdr"
    }

    fn wake(&self, target: &str, line: &str, cancel: Option<&AtomicBool>) -> WakeResult {
        // Herdr reads the target positionally: one that starts with `-` would be an option.
        if let Err(e) = another_core::refuse_option_like_target(target) {
            return WakeResult::new(WakeOutcome::Undelivered, e.to_string());
        }
        let argv = &self.wake_argv(target, line)[0];
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        match run_bounded(&mut cmd, self.wake_deadline, cancel) {
            Ok(Bounded::Done(ran)) => map_wake(&Answer {
                code: ran.status.code(),
                stdout: ran.stdout,
                stderr: ran.stderr,
            }),
            Ok(Bounded::TimedOut) => WakeResult::new(
                WakeOutcome::Unknown,
                format!(
                    "herdr did not finish within {}s (or the daemon is stopping); it was stopped",
                    self.wake_deadline.as_secs()
                ),
            ),
            Ok(Bounded::TooMuchOutput) => {
                WakeResult::new(WakeOutcome::Unknown, "herdr wrote more than 1 MiB")
            }
            Err(e) => WakeResult::new(
                WakeOutcome::Undelivered,
                format!("running {} (is it installed?): {e}", self.program),
            ),
        }
    }

    fn status(&self, target: &str, cancel: Option<&AtomicBool>) -> Observation {
        if let Err(e) = another_core::refuse_option_like_target(target) {
            return bare(State::NoAnswer, e.to_string());
        }
        match self.ask(&["agent", "get", target], cancel) {
            Ok(answer) => map_status(&answer),
            Err(why) => bare(State::NoAnswer, why),
        }
    }

    fn list(&self, cancel: Option<&AtomicBool>) -> ListResult {
        match self.ask(&["agent", "list"], cancel) {
            Ok(answer) => map_list(&answer),
            Err(why) => ListResult::NoAnswer(why),
        }
    }

    fn wake_argv(&self, target: &str, line: &str) -> Vec<Vec<String>> {
        let mut argv = vec![
            self.program.clone(),
            "agent".into(),
            "prompt".into(),
            target.to_string(),
            another_core::single_line(line, MAX_LINE_CHARS),
            "--wait".into(),
        ];
        // With the states listed, Herdr returns on the first one it sees after the line
        // instead of waiting for the turn to end.
        for state in ["working", "blocked", "idle", "done", "unknown"] {
            argv.push("--until".into());
            argv.push(state.into());
        }
        argv.push("--timeout".into());
        argv.push("8000".into());
        vec![argv]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use another_core::Tri;

    fn out(code: i32, stdout: &str, stderr: &str) -> Answer {
        Answer {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn error(code: &str) -> Answer {
        out(
            1,
            "",
            &format!(r#"{{"error":{{"code":"{code}","message":"m"}},"id":"cli:agent:prompt"}}"#),
        )
    }

    /// An `AgentInfo` as Herdr 0.9.5 prints it for a Claude Code pane.
    fn claude(status: &str) -> String {
        format!(
            r#"{{"agent":"claude","agent_session":{{"agent":"claude","kind":"id","source":"herdr:claude","value":"76a09219"}},"agent_status":"{status}","cwd":"/work/app","focused":false,"foreground_cwd":"/work/app/sub","pane_id":"w90:p1","revision":1}}"#
        )
    }

    /// A Codex pane: no `agent_session` at all.
    const CODEX: &str =
        r#"{"agent":"codex","agent_status":"done","cwd":"/work/lanes","pane_id":"w93:p1"}"#;

    #[test]
    fn herdr_maps_every_answer() {
        // wake: every row of the table.
        let prompted = format!(
            r#"{{"id":"cli:agent:prompt","result":{{"agent":{},"type":"agent_prompted"}}}}"#,
            claude("working")
        );
        let started = map_wake(&out(0, &prompted, ""));
        assert_eq!(started.outcome, WakeOutcome::Started);
        assert_eq!(
            (started.delivered(), started.started()),
            (Tri::Yes, Tri::Yes)
        );
        assert!(started.detail.contains("working"), "{started:?}");
        for unreadable in ["", "not json", r#"{"result":{"type":"ok"}}"#] {
            assert_eq!(
                map_wake(&out(0, unreadable, "")).outcome,
                WakeOutcome::Unknown,
                "{unreadable:?}"
            );
        }
        for (code, outcome) in [
            ("agent_prompt_stalled", WakeOutcome::NotStarted),
            ("agent_blocked", WakeOutcome::RefusedBlocked),
            ("agent_not_found", WakeOutcome::Gone),
            ("timeout", WakeOutcome::Unconfirmed),
            ("pane_closed", WakeOutcome::Unknown),
        ] {
            let got = map_wake(&error(code));
            assert_eq!(got.outcome, outcome, "{code}");
            assert!(got.detail.contains(code), "{got:?}");
        }
        assert!(map_wake(&error("agent_prompt_stalled")).outcome.failure());
        assert!(!map_wake(&error("timeout")).outcome.failure());
        assert!(!map_wake(&error("agent_blocked")).outcome.failure());
        // An error is read from stderr only: the same JSON on stdout says nothing.
        let on_stdout = out(
            1,
            r#"{"error":{"code":"agent_not_found","message":"m"}}"#,
            "",
        );
        assert_eq!(map_wake(&on_stdout).outcome, WakeOutcome::Unknown);
        assert_eq!(map_status(&on_stdout).state, State::NoAnswer);
        assert_eq!(map_wake(&out(1, "", "")).outcome, WakeOutcome::Unknown);
        assert_eq!(
            map_wake(&out(2, "", "usage")).outcome,
            WakeOutcome::Undelivered
        );
        let signalled = Answer {
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(map_wake(&signalled).outcome, WakeOutcome::Unknown);

        // status: every Herdr status, `done` counted as `idle`.
        let info = |status: &str| {
            format!(
                r#"{{"id":"cli:agent:get","result":{{"agent":{},"type":"agent_info"}}}}"#,
                claude(status)
            )
        };
        for (status, state) in [
            ("working", State::Working),
            ("idle", State::Idle),
            ("done", State::Idle),
            ("blocked", State::Blocked),
            ("unknown", State::Unknown),
            ("new-word", State::Unknown),
        ] {
            let seen = map_status(&out(0, &info(status), ""));
            assert_eq!(seen.state, state, "{status}");
            assert_eq!(seen.session_id.as_deref(), Some("76a09219"));
            assert_eq!(seen.program.as_deref(), Some("claude"));
            assert_eq!(
                seen.cwd.as_deref(),
                Some("/work/app"),
                "cwd, not foreground_cwd"
            );
        }
        assert_eq!(map_status(&error("agent_not_found")).state, State::Gone);
        assert_eq!(map_status(&error("timeout")).state, State::NoAnswer);
        assert_eq!(map_status(&out(1, "", "")).state, State::NoAnswer);
        assert_eq!(map_status(&out(0, "garbage", "")).state, State::NoAnswer);

        // The session id comes from kind `id` only.
        let agent = |session: &str| -> Value {
            serde_json::from_str(&format!(
                r#"{{"agent":"claude","agent_status":"idle","cwd":"/w","pane_id":"p"{session}}}"#
            ))
            .unwrap()
        };
        let id = |session: &str| observation(&agent(session)).session_id;
        assert_eq!(
            id(r#","agent_session":{"kind":"id","value":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(
            id(r#","agent_session":{"kind":"path","value":"/t/x.jsonl"}"#),
            None
        );
        assert_eq!(id(r#","agent_session":null"#), None);
        assert_eq!(id(r#","agent_session":{"kind":"id","value":""}"#), None);
        assert_eq!(id(""), None);

        // list: one session per entry; a Codex entry has no session id.
        let list = format!(
            r#"{{"id":"cli:agent:list","result":{{"agents":[{CODEX},{}],"type":"agent_list"}}}}"#,
            claude("idle")
        );
        let ListResult::Sessions(sessions) = map_list(&out(0, &list, "")) else {
            panic!("a good list");
        };
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].target, "w93:p1");
        assert_eq!(sessions[0].observation.session_id, None);
        assert_eq!(sessions[0].observation.program.as_deref(), Some("codex"));
        assert_eq!(sessions[0].observation.state, State::Idle, "done is idle");
        assert_eq!(sessions[1].target, "w90:p1");
        assert_eq!(
            sessions[1].observation.session_id.as_deref(),
            Some("76a09219")
        );
        let empty = r#"{"id":"cli:agent:list","result":{"agents":[],"type":"agent_list"}}"#;
        assert_eq!(
            map_list(&out(0, empty, "")),
            ListResult::Sessions(Vec::new())
        );
        for bad in [out(1, "", ""), out(0, "nope", ""), error("timeout")] {
            assert!(matches!(map_list(&bad), ListResult::NoAnswer(_)));
        }
    }

    #[test]
    fn the_prompt_names_every_state_and_herdrs_own_timeout() {
        let argv = HerdrTransport::new("herdr").wake_argv("w1:p1", "two\nlines");
        assert_eq!(
            argv,
            [[
                "herdr",
                "agent",
                "prompt",
                "w1:p1",
                "two lines",
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
            ]]
        );
    }
}
