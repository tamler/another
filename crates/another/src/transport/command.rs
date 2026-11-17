//! Command transports: a transport made of commands the owner wrote into
//! `$ANOTHER_HOME/transports.json`, for tmux, cmux or anything else with a CLI.
//!
//! Another reads that file and never writes it. No command, no flag, no MCP tool and no
//! peer can set a command: the only way in is the file, and on Unix the file is refused
//! unless the user running Another owns it and nobody else can write it.
//!
//! Every command is started directly as argv, with no shell. `{target}` and `{message}`
//! are replaced inside argv elements in one pass after the argv is fixed, so a value never
//! adds or splits an argument and placeholder text inside a value is not replaced again.
//! The same two values are in the command's environment as `ANOTHER_TARGET` and
//! `ANOTHER_WAKE_MESSAGE`. Nothing else about the agent (its name, its session id) is
//! given to a command.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{bare, ListResult, Observation, Session, State, Transport, WakeOutcome, WakeResult};
use crate::wake::{run_bounded, Bounded};

/// The config file's name inside the home.
pub const CONFIG_FILE: &str = "transports.json";
/// Whether a session of a command transport may be confirmed gone. Only a transport whose
/// `list` prints session ids can be matched at all, and what such a script lists across
/// its own restart is not known to Another. Off: observation and attention work either
/// way, and nothing is confirmed.
pub const COMMAND_CONFIRMS_GONE: bool = false;

/// Names a command transport may not take: transports of their own.
pub const RESERVED_NAMES: [&str; 2] = ["herdr", "rex"];
/// Deadline for each `wake` command.
pub const WAKE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline for `status` and `list`.
pub const ASK_DEADLINE: Duration = Duration::from_secs(3);
/// How often `status` is asked after a wake.
const POLL_EVERY: Duration = Duration::from_millis(500);
const DEFAULT_CONFIRM_SECS: u64 = 8;
const MAX_CONFIRM_SECS: u64 = 30;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Longest wake line, in characters.
const MAX_LINE_CHARS: usize = 1000;
/// Longest session id a `list` line may carry, in characters.
const MAX_SESSION_ID_CHARS: usize = 200;

const TARGET: &str = "{target}";
const MESSAGE: &str = "{message}";

/// What `transports.json` says. A missing file is an empty config with no error; a refused
/// file is an empty config with `error` set, so no command transport exists at all.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The file that was looked for.
    pub path: Option<PathBuf>,
    /// The command transports, in the order the file lists them.
    pub transports: Vec<CommandSpec>,
    /// Why the file was refused.
    pub error: Option<String>,
}

impl Config {
    /// A config from JSON text, with no file behind it (for callers that hold the text).
    pub fn from_json(text: &str) -> Config {
        match parse_config(text) {
            Ok(transports) => Config {
                transports,
                ..Config::default()
            },
            Err(e) => Config {
                error: Some(format!("{CONFIG_FILE}: {e}")),
                ..Config::default()
            },
        }
    }

    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.transports.iter().find(|t| t.name == name)
    }

    /// Whether this build and this config provide a transport of that name.
    pub fn provides(&self, name: &str) -> bool {
        name == "herdr" || self.get(name).is_some()
    }

    fn shown_path(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => format!("$ANOTHER_HOME/{CONFIG_FILE}"),
        }
    }

    /// One line for the owner when the file was refused.
    pub fn refusal(&self) -> Option<String> {
        self.error.as_ref().map(|e| {
            format!("{e}. No command transport is available until it is fixed (docs/WAKE.md)")
        })
    }

    /// Why `name` cannot be used, in words that say what to set.
    pub fn why_missing(&self, name: &str) -> String {
        if name == "rex" {
            return "the rex transport is not in this build".to_string();
        }
        match self.refusal() {
            Some(refused) => format!("no transport named {name:?}: {refused}"),
            None => format!(
                "no transport named {name:?} is configured: define it in {} (docs/WAKE.md, \
                 \"Command transports\", has a tmux and a cmux example)",
                self.shown_path()
            ),
        }
    }
}

/// One command transport as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    /// The commands of one wake, run in order.
    pub wake: Vec<Vec<String>>,
    pub status: Option<StatusSpec>,
    pub list: Option<Vec<String>>,
    /// An environment variable whose value, in a hook's environment, is the session's
    /// target on this transport.
    pub capture_env: Option<String>,
    /// Replace `\` with `/` in the message, for tools that unescape backslash sequences.
    pub backslash_to_slash: bool,
    /// How long to ask `status` after a wake.
    pub confirm_secs: u64,
}

impl CommandSpec {
    /// Every program this transport starts, each once.
    pub fn programs(&self) -> Vec<&str> {
        let mut programs: Vec<&str> = self
            .wake
            .iter()
            .chain(self.status.iter().map(|s| &s.argv))
            .chain(self.list.iter())
            .map(|argv| argv[0].as_str())
            .collect();
        programs.sort_unstable();
        programs.dedup();
        programs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSpec {
    pub argv: Vec<String>,
    /// What each exit code means. A code that is not here is `no_answer`.
    pub exit_codes: BTreeMap<i32, State>,
}

impl StatusSpec {
    /// Whether this status can show that an agent started: it can say `working` or
    /// `blocked`.
    fn shows_activity(&self) -> bool {
        self.exit_codes
            .values()
            .any(|s| matches!(s, State::Working | State::Blocked))
    }

    /// Whether this status can give a reason to type nothing: it can say `blocked` or
    /// `gone`.
    fn can_refuse(&self) -> bool {
        self.exit_codes
            .values()
            .any(|s| matches!(s, State::Blocked | State::Gone))
    }
}

/// Read `transports.json` in `another_home`. Never fails: see [`Config`]. The file is read
/// each time; nothing is cached.
pub fn load_config(another_home: &Path) -> Config {
    let path = another_home.join(CONFIG_FILE);
    let read = read_checked(&path).and_then(|text| match text {
        Some(text) => parse_config(&text),
        None => Ok(Vec::new()),
    });
    let (transports, error) = match read {
        Ok(transports) => (transports, None),
        Err(e) => (Vec::new(), Some(format!("{}: {e}", path.display()))),
    };
    Config {
        path: Some(path),
        transports,
        error,
    }
}

/// Open for reading without ever waiting: a FIFO with no writer opens at once.
#[cfg(unix)]
fn open_without_waiting(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_without_waiting(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// The file's text, `None` when there is no file. Anything but a regular file (a FIFO, a
/// device, a directory) is refused before it is opened, so a hook can never wait on it;
/// the open itself does not wait either, in case the file is swapped in between. The
/// owner and size checks and the read then use one open handle, so the file that was
/// checked is the file that is read.
fn read_checked(path: &Path) -> Result<Option<String>, String> {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Err("is not a regular file".into()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot be read: {e}")),
    }
    let file = match open_without_waiting(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot be read: {e}")),
    };
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot be read: {e}"))?;
    if !meta.is_file() {
        return Err("is not a regular file".into());
    }
    owner_check(&meta)?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err("is larger than 1 MiB".into());
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot be read: {e}"))?;
    Ok(Some(text))
}

/// The commands in this file run as the user: refuse a file that someone else owns or can
/// write.
#[cfg(unix)]
fn owner_check(meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` takes no argument, touches no memory and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(format!(
            "is owned by user {}, not by the user running Another ({me})",
            meta.uid()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "is writable by group or others (mode {mode:04o}); run chmod go-w on it"
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn owner_check(_meta: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

fn only_keys(object: &serde_json::Map<String, Value>, known: &[&str]) -> Result<(), String> {
    match object.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(format!("unknown key {key:?} (known: {})", known.join(", "))),
        None => Ok(()),
    }
}

/// The command transports of a config text, in file order, or why the text is refused.
pub fn parse_config(text: &str) -> Result<Vec<CommandSpec>, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let root = root.as_object().ok_or("the top level must be an object")?;
    only_keys(root, &["version", "transports"])?;
    match root.get("version") {
        Some(version) if version.as_u64() == Some(1) => {}
        Some(version) => {
            return Err(format!(
                "version {version} is not known; this build reads version 1"
            ))
        }
        None => return Err("`version` is missing; it must be 1".into()),
    }
    let Some(transports) = root.get("transports") else {
        return Ok(Vec::new());
    };
    let transports = transports
        .as_object()
        .ok_or("`transports` must be an object")?;
    transports
        .iter()
        .map(|(name, value)| {
            parse_transport(name, value).map_err(|e| format!("transport {name:?}: {e}"))
        })
        .collect()
}

fn parse_transport(name: &str, value: &Value) -> Result<CommandSpec, String> {
    if !another_core::valid_transport_name(name) {
        return Err(
            "a name is a lowercase letter, then up to 31 lowercase letters, digits, `_` or `-`"
                .into(),
        );
    }
    if RESERVED_NAMES.contains(&name) {
        return Err("that name is reserved".into());
    }
    let object = value.as_object().ok_or("must be an object")?;
    only_keys(
        object,
        &[
            "wake",
            "status",
            "list",
            "capture_env",
            "backslash_to_slash",
            "confirm_secs",
        ],
    )?;
    let wake = object
        .get("wake")
        .ok_or("`wake` is required")?
        .as_array()
        .filter(|commands| !commands.is_empty())
        .ok_or("`wake` must be a non-empty list of commands")?
        .iter()
        .map(|command| argv(command, "wake", &[TARGET, MESSAGE]))
        .collect::<Result<Vec<_>, _>>()?;
    let status = object.get("status").map(parse_status).transpose()?;
    let list = match object.get("list") {
        Some(list) => {
            let list = list.as_object().ok_or("`list` must be an object")?;
            only_keys(list, &["argv"])?;
            let command = list.get("argv").ok_or("`list` needs `argv`")?;
            Some(argv(command, "list", &[])?)
        }
        None => None,
    };
    let capture_env = match object.get("capture_env") {
        Some(var) => {
            let var = var
                .as_str()
                .filter(|v| env_name(v))
                .ok_or("`capture_env` must be the name of an environment variable")?;
            Some(var.to_string())
        }
        None => None,
    };
    let backslash_to_slash = match object.get("backslash_to_slash") {
        Some(flag) => flag
            .as_bool()
            .ok_or("`backslash_to_slash` must be true or false")?,
        None => false,
    };
    let confirm_secs = match object.get("confirm_secs") {
        Some(secs) => secs
            .as_u64()
            .filter(|s| *s <= MAX_CONFIRM_SECS)
            .ok_or("`confirm_secs` must be a whole number from 0 to 30")?,
        None => DEFAULT_CONFIRM_SECS,
    };
    Ok(CommandSpec {
        name: name.to_string(),
        wake,
        status,
        list,
        capture_env,
        backslash_to_slash,
        confirm_secs,
    })
}

fn parse_status(value: &Value) -> Result<StatusSpec, String> {
    let object = value.as_object().ok_or("`status` must be an object")?;
    only_keys(object, &["argv", "exit_codes"])?;
    let command = object.get("argv").ok_or("`status` needs `argv`")?;
    let codes = object
        .get("exit_codes")
        .ok_or("`status` needs `exit_codes`")?
        .as_object()
        .filter(|codes| !codes.is_empty())
        .ok_or("`exit_codes` must be an object with at least one exit code")?;
    let mut exit_codes = BTreeMap::new();
    for (code, state) in codes {
        let number: i32 = code
            .parse()
            .map_err(|_| format!("exit code {code:?} is not a number"))?;
        let state = state
            .as_str()
            .and_then(|word| STATES.iter().find(|s| s.as_str() == word))
            .ok_or_else(|| {
                format!(
                    "exit code {code}: {state} is not a state (working, idle, blocked, present, \
                     unknown, gone, no_answer)"
                )
            })?;
        if exit_codes.insert(number, *state).is_some() {
            return Err(format!("exit code {number} is listed twice"));
        }
    }
    Ok(StatusSpec {
        argv: argv(command, "status", &[TARGET])?,
        exit_codes,
    })
}

const STATES: [State; 7] = [
    State::Working,
    State::Idle,
    State::Blocked,
    State::Present,
    State::Unknown,
    State::Gone,
    State::NoAnswer,
];

fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_placeholder(arg: &str) -> bool {
    arg.contains(TARGET) || arg.contains(MESSAGE)
}

/// `-c`, or a group of short flags that ends in `c` (`-lc`, `-ic`): what a shell takes
/// its code after.
fn shell_code_flag(arg: &str) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|flags| flags.ends_with('c') && flags.chars().all(|c| c.is_ascii_alphabetic()))
}

/// One command of the config: a non-empty list of strings whose first element is the
/// program. `allowed` are the placeholders that have a value for this command.
fn argv(value: &Value, what: &str, allowed: &[&str]) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(|| format!("a `{what}` command must be a non-empty list of strings"))?;
    let mut command = Vec::with_capacity(items.len());
    for item in items {
        let arg = item
            .as_str()
            .filter(|arg| !arg.contains('\0'))
            .ok_or_else(|| format!("a `{what}` command must be a non-empty list of strings"))?;
        command.push(arg.to_string());
    }
    if command[0].is_empty() {
        return Err(format!("a `{what}` command has an empty program name"));
    }
    if has_placeholder(&command[0]) {
        return Err(format!(
            "a `{what}` command has a placeholder in its program name ({:?})",
            command[0]
        ));
    }
    // A relative path would be looked up from whatever directory Another is run in, and
    // agents work in checkouts whose content other people chose.
    let program = Path::new(&command[0]);
    let has_separator = command[0].contains('/') || command[0].contains(std::path::MAIN_SEPARATOR);
    if has_separator && !program.is_absolute() {
        return Err(format!(
            "a `{what}` command names its program by a relative path ({:?}); give an \
             absolute path or a bare name that is looked up on PATH",
            command[0]
        ));
    }
    for (i, arg) in command.iter().enumerate().skip(1) {
        if let Some(placeholder) = [TARGET, MESSAGE]
            .into_iter()
            .find(|p| arg.contains(p) && !allowed.contains(p))
        {
            return Err(format!(
                "a `{what}` command uses {placeholder}, which has no value there"
            ));
        }
        // A lint, not a security boundary: it knows this one pattern.
        if has_placeholder(arg) && shell_code_flag(&command[i - 1]) {
            return Err(format!(
                "a `{what}` command puts {arg:?} right after {:?}, so a mail subject would run \
                 as shell code; have the script read ANOTHER_TARGET and ANOTHER_WAKE_MESSAGE \
                 from its environment instead",
                command[i - 1]
            ));
        }
    }
    Ok(command)
}

/// Replace `{target}` and `{message}` in one argv element, in a single pass over the
/// template: text that came from a value is never looked at again.
fn fill(template: &str, target: &str, message: &str) -> String {
    let mut out = String::with_capacity(template.len() + target.len() + message.len());
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        if let Some(tail) = rest.strip_prefix(TARGET) {
            out.push_str(target);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(MESSAGE) {
            out.push_str(message);
            rest = tail;
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// A command with its placeholders filled: as many arguments as the config has, each one
/// string. An argument that begins with a placeholder and comes out starting with `-`
/// would be read as an option, so it is refused.
fn filled(command: &[String], target: &str, message: &str) -> Result<Vec<String>, String> {
    command
        .iter()
        .map(|template| {
            let arg = fill(template, target, message);
            let leads = template.starts_with(TARGET) || template.starts_with(MESSAGE);
            if leads && arg.starts_with('-') {
                Err(format!(
                    "nothing was run: the argument made from {template:?} would start with \
                     '-' and be read as an option"
                ))
            } else {
                Ok(arg)
            }
        })
        .collect()
}

/// A transport that runs the configured commands of one [`CommandSpec`].
pub struct CommandTransport {
    spec: CommandSpec,
    wake_deadline: Duration,
    ask_deadline: Duration,
    poll_every: Duration,
}

impl CommandTransport {
    pub fn new(spec: CommandSpec) -> CommandTransport {
        CommandTransport {
            spec,
            wake_deadline: WAKE_DEADLINE,
            ask_deadline: ASK_DEADLINE,
            poll_every: POLL_EVERY,
        }
    }

    /// Other deadlines and another poll interval, for tests.
    pub fn with_timing(mut self, wake: Duration, ask: Duration, poll: Duration) -> Self {
        self.wake_deadline = wake;
        self.ask_deadline = ask;
        self.poll_every = poll;
        self
    }

    /// The line as this transport types it: one safe line (D75), and for a tool that
    /// unescapes backslash sequences, no backslash.
    fn line(&self, line: &str) -> String {
        let safe = another_core::single_line(line, MAX_LINE_CHARS);
        if self.spec.backslash_to_slash {
            safe.replace('\\', "/")
        } else {
            safe
        }
    }

    fn run(
        &self,
        argv: &[String],
        target: Option<&str>,
        message: Option<&str>,
        deadline: Duration,
        cancel: Option<&AtomicBool>,
    ) -> std::io::Result<Bounded> {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for (key, value) in [
            ("ANOTHER_TARGET", target),
            ("ANOTHER_WAKE_MESSAGE", message),
        ] {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        run_bounded(&mut cmd, deadline, cancel)
    }

    /// Ask `status` after a delivered line until it shows the agent started or the time is
    /// up.
    fn confirm(
        &self,
        status: &StatusSpec,
        target: &str,
        cancel: Option<&AtomicBool>,
    ) -> WakeResult {
        let name = &self.spec.name;
        let secs = self.spec.confirm_secs;
        let until = Instant::now() + Duration::from_secs(secs);
        let mut polls = 0;
        let mut only_idle = true;
        let mut last = State::NoAnswer;
        while Instant::now() < until && !cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            std::thread::sleep(self.poll_every);
            last = self.ask_status(status, target, cancel).state;
            polls += 1;
            match last {
                State::Working | State::Blocked => {
                    return WakeResult::new(
                        WakeOutcome::Started,
                        format!("{name}: status said {} after the line", last.as_str()),
                    )
                }
                State::Idle => {}
                _ => only_idle = false,
            }
        }
        if polls > 0 && only_idle {
            WakeResult::new(
                WakeOutcome::NotStarted,
                format!("{name}: status stayed idle for {secs}s after the line"),
            )
        } else {
            WakeResult::new(
                WakeOutcome::Unconfirmed,
                format!(
                    "{name}: status did not show whether the agent started (last: {})",
                    last.as_str()
                ),
            )
        }
    }

    fn ask_status(
        &self,
        status: &StatusSpec,
        target: &str,
        cancel: Option<&AtomicBool>,
    ) -> Observation {
        let name = &self.spec.name;
        if let Err(e) = another_core::validate_target(target) {
            return bare(State::NoAnswer, e.to_string());
        }
        let argv = match filled(&status.argv, target, "") {
            Ok(argv) => argv,
            Err(why) => return bare(State::NoAnswer, why),
        };
        match self.run(&argv, Some(target), None, self.ask_deadline, cancel) {
            Ok(Bounded::Done(ran)) => match ran.status.code() {
                Some(code) => match status.exit_codes.get(&code) {
                    Some(state) => bare(*state, format!("{name}: status exit {code}")),
                    None => bare(
                        State::NoAnswer,
                        format!("{name}: status exit {code} has no meaning in the config"),
                    ),
                },
                None => bare(
                    State::NoAnswer,
                    format!("{name}: status was stopped by a signal"),
                ),
            },
            Ok(Bounded::TimedOut) => bare(
                State::NoAnswer,
                format!(
                    "{name}: status did not answer within {}s",
                    self.ask_deadline.as_secs()
                ),
            ),
            Ok(Bounded::TooMuchOutput) => bare(
                State::NoAnswer,
                format!("{name}: status wrote more than 1 MiB"),
            ),
            Err(e) => bare(
                State::NoAnswer,
                format!("running {} (is it installed?): {e}", argv[0]),
            ),
        }
    }
}

/// The sessions of one `list` answer: each non-empty line is a target, then optionally a
/// tab and a state word, then optionally a tab and a session id.
fn parse_list(name: &str, stdout: &[u8]) -> ListResult {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return ListResult::NoAnswer(format!("{name}: list output is not UTF-8"));
    };
    let mut sessions = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let malformed = || {
            ListResult::NoAnswer(format!(
                "{name}: list printed a line that is not `target`, `target<tab>state` or \
                 `target<tab>state<tab>session id`: {line:?}"
            ))
        };
        let mut parts = line.split('\t');
        let target = parts.next().unwrap_or_default();
        if another_core::validate_target(target).is_err() {
            return malformed();
        }
        let state = match parts.next() {
            None => State::Present,
            Some(word) => {
                // What a listed session can be: `gone` and `no_answer` are not states of
                // something that is listed.
                let listed = [
                    State::Working,
                    State::Idle,
                    State::Blocked,
                    State::Present,
                    State::Unknown,
                ];
                match listed.into_iter().find(|s| s.as_str() == word) {
                    Some(state) => state,
                    None => return malformed(),
                }
            }
        };
        let session_id = match parts.next() {
            None => None,
            Some(id)
                if !id.is_empty()
                    && id.chars().count() <= MAX_SESSION_ID_CHARS
                    && !id.chars().any(char::is_control) =>
            {
                Some(id.to_string())
            }
            Some(_) => return malformed(),
        };
        if parts.next().is_some() {
            return malformed();
        }
        sessions.push(Session {
            target: target.to_string(),
            observation: Observation {
                state,
                session_id,
                program: None,
                cwd: None,
                detail: another_core::single_line(&format!("{name}: {}", state.as_str()), 300),
            },
        });
    }
    ListResult::Sessions(sessions)
}

impl Transport for CommandTransport {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn wake(&self, target: &str, line: &str, cancel: Option<&AtomicBool>) -> WakeResult {
        let name = &self.spec.name;
        if let Err(e) = another_core::validate_target(target) {
            return WakeResult::new(WakeOutcome::Undelivered, e.to_string());
        }
        let line = self.line(line);
        // Every command is built before the first one runs, so a refused argument stops
        // the wake with nothing typed.
        let commands: Result<Vec<Vec<String>>, String> = self
            .spec
            .wake
            .iter()
            .map(|command| filled(command, target, &line))
            .collect();
        let commands = match commands {
            Ok(commands) => commands,
            Err(why) => return WakeResult::new(WakeOutcome::Undelivered, why),
        };
        // Where the transport can say so, an agent waiting on a person is not typed into,
        // and neither is a target with nothing at it. A status that can say neither is
        // not asked.
        if let Some(status) = self.spec.status.as_ref().filter(|s| s.can_refuse()) {
            let seen = self.ask_status(status, target, cancel);
            match seen.state {
                State::Blocked => {
                    return WakeResult::new(
                        WakeOutcome::RefusedBlocked,
                        format!("{name}: the agent is waiting on a person; nothing was typed"),
                    )
                }
                State::Gone => {
                    return WakeResult::new(
                        WakeOutcome::Gone,
                        format!("{name}: nothing is at {target}; nothing was typed"),
                    )
                }
                _ => {}
            }
        }
        for (i, argv) in commands.iter().enumerate() {
            // Before the first command nothing was typed; after it something may have been.
            let failed = if i == 0 {
                WakeOutcome::Undelivered
            } else {
                WakeOutcome::Unknown
            };
            match self.run(argv, Some(target), Some(&line), self.wake_deadline, cancel) {
                Err(e) => {
                    return WakeResult::new(
                        failed,
                        format!("running {} (is it installed?): {e}", argv[0]),
                    )
                }
                Ok(Bounded::Done(ran)) if ran.status.success() => {}
                Ok(Bounded::Done(ran)) => {
                    return WakeResult::new(
                        failed,
                        format!("{} exited with {}", argv.join(" "), ran.status),
                    )
                }
                Ok(Bounded::TimedOut) => {
                    return WakeResult::new(
                        WakeOutcome::Unknown,
                        format!(
                        "{} did not finish within {}s (or the daemon is stopping); it was stopped",
                        argv[0],
                        self.wake_deadline.as_secs()
                    ),
                    )
                }
                Ok(Bounded::TooMuchOutput) => {
                    return WakeResult::new(
                        WakeOutcome::Unknown,
                        format!("{} wrote more than 1 MiB", argv[0]),
                    )
                }
            }
        }
        match &self.spec.status {
            Some(status) if self.spec.confirm_secs > 0 && status.shows_activity() => {
                self.confirm(status, target, cancel)
            }
            _ => WakeResult::new(
                WakeOutcome::Unconfirmed,
                format!("{name} cannot report whether the agent started"),
            ),
        }
    }

    fn status(&self, target: &str, cancel: Option<&AtomicBool>) -> Observation {
        match &self.spec.status {
            Some(status) => self.ask_status(status, target, cancel),
            None => bare(
                State::NoAnswer,
                format!("{} has no status command", self.spec.name),
            ),
        }
    }

    fn list(&self, cancel: Option<&AtomicBool>) -> ListResult {
        let name = &self.spec.name;
        let Some(argv) = &self.spec.list else {
            return ListResult::Unsupported;
        };
        match self.run(argv, None, None, self.ask_deadline, cancel) {
            Ok(Bounded::Done(ran)) if ran.status.success() => parse_list(name, &ran.stdout),
            Ok(Bounded::Done(ran)) => {
                ListResult::NoAnswer(format!("{name}: list exited with {}", ran.status))
            }
            Ok(Bounded::TimedOut) => ListResult::NoAnswer(format!(
                "{name}: list did not answer within {}s",
                self.ask_deadline.as_secs()
            )),
            Ok(Bounded::TooMuchOutput) => {
                ListResult::NoAnswer(format!("{name}: list wrote more than 1 MiB"))
            }
            Err(e) => ListResult::NoAnswer(format!("running {} (is it installed?): {e}", argv[0])),
        }
    }

    fn wake_argv(&self, target: &str, line: &str) -> Vec<Vec<String>> {
        let line = self.line(line);
        self.spec
            .wake
            .iter()
            .map(|command| command.iter().map(|t| fill(t, target, &line)).collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first `json` block of the "Command transports" section of docs/WAKE.md: the
    /// example the owner is told to copy.
    fn documented_example() -> String {
        let doc = include_str!("../../../../docs/WAKE.md");
        let section = doc
            .split("## Command transports")
            .nth(1)
            .expect("docs/WAKE.md has a Command transports section");
        let block = section
            .split("```json")
            .nth(1)
            .expect("the section has a json example");
        block.split("```").next().unwrap().to_string()
    }

    fn transport(config: &Config, name: &str) -> CommandTransport {
        CommandTransport::new(config.get(name).expect("configured").clone())
    }

    #[test]
    fn tmux_and_cmux_examples_reproduce_the_old_commands() {
        let config = Config::from_json(&documented_example());
        assert_eq!(config.error, None);
        let names: Vec<&str> = config.transports.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names[..2], ["tmux", "cmux"], "tmux first, as before");
        // What the removed `adapter_commands` test asserted of `wake::commands`.
        let t = transport(&config, "tmux").wake_argv("%3", "hi");
        assert_eq!(t[0], ["tmux", "send-keys", "-t", "%3", "-l", "hi"]);
        assert_eq!(t[1], ["tmux", "send-keys", "-t", "%3", "Enter"]);
        assert_eq!(t.len(), 2);
        let c = transport(&config, "cmux").wake_argv("surface-1", "hi");
        assert_eq!(c, [["cmux", "send", "--surface", "surface-1", "hi\\n"]]);
        // The old cmux rule of D75: a backslash in the text becomes `/`, so the one escape
        // left is the one that submits the line. tmux types the text as it is.
        let c = transport(&config, "cmux").wake_argv("s", "a\\nb\\");
        assert_eq!(c[0][4], "a/nb/\\n");
        let t = transport(&config, "tmux").wake_argv("%3", "a\\nb");
        assert_eq!(t[0][5], "a\\nb");
        // The old `notify` target is a transport of its own now.
        let n = transport(&config, "cmux-notify").wake_argv("notify", "hi");
        assert_eq!(
            n,
            [["cmux", "notify", "--title", "Another", "--body", "hi"]]
        );
        // tmux can say a pane is there, and nothing more; cmux can say nothing.
        let tmux = config.get("tmux").unwrap();
        assert_eq!(tmux.capture_env.as_deref(), Some("TMUX_PANE"));
        let status = tmux.status.as_ref().unwrap();
        assert_eq!(
            status.exit_codes,
            BTreeMap::from([(0, State::Present)]),
            "exit 1 is unmapped: it can mean no pane or no server"
        );
        assert!(!status.shows_activity());
        assert_eq!(
            tmux.list.as_deref().unwrap(),
            ["tmux", "list-panes", "-a", "-F", "#{pane_id}"]
        );
        let cmux = config.get("cmux").unwrap();
        assert_eq!(cmux.capture_env.as_deref(), Some("CMUX_SURFACE_ID"));
        assert!(cmux.status.is_none() && cmux.list.is_none());
    }

    #[test]
    fn placeholders_are_filled_in_one_pass_and_never_split() {
        // A value that holds placeholder text is not filled again, in either order.
        assert_eq!(
            fill("{target}:{message}", "{message}", "{target}"),
            "{message}:{target}"
        );
        assert_eq!(fill("a{message}b{message}", "t", "x y"), "ax ybx y");
        // Braces that are not a placeholder are the tool's own (tmux formats).
        assert_eq!(
            fill("#{pane_id} {targets}", "t", "m"),
            "#{pane_id} {targets}"
        );
        let command: Vec<String> = ["prog", "-t", "{target}", "--text={message}", "{message}"]
            .map(String::from)
            .to_vec();
        let long = "x".repeat(5000);
        for hostile in [
            "two words",
            "it's \"quoted\"",
            "$(touch x); `id` && rm -rf ~ | cat > f",
            "{target} {message}",
            "trailing\\",
            "\u{202e}reversed",
            long.as_str(),
        ] {
            let args = filled(&command, "pane 1", hostile).unwrap();
            assert_eq!(args.len(), command.len(), "{hostile:?}");
            assert_eq!(args[2], "pane 1");
            assert_eq!(args[3], format!("--text={hostile}"));
            assert_eq!(args[4], hostile);
        }
        // A value at the start of an argument cannot turn it into an option.
        for dash in ["-x", "--help", "-"] {
            let err = filled(&command, "pane", dash).unwrap_err();
            assert!(err.contains("read as an option"), "{err}");
            // Inside an argument it is only text.
            let inside: Vec<String> = ["prog", "--text={message}"].map(String::from).to_vec();
            assert_eq!(
                filled(&inside, "pane", dash).unwrap()[1],
                format!("--text={dash}")
            );
        }
        assert!(filled(&command, "-t", "hi").is_err());
    }

    #[test]
    fn the_line_a_command_transport_types_is_one_safe_line() {
        let config = Config::from_json(&documented_example());
        let hostile = "Lunch?\r\x1b[2J\x07\x08\x7f\x15\u{202e}evil\u{2066}\t \\n then\nrm -rf ~";
        for name in ["tmux", "cmux", "cmux-notify"] {
            let argv = transport(&config, name).wake_argv("%3", hostile);
            for arg in argv.iter().flatten() {
                assert!(
                    !arg.chars().any(another_core::is_unsafe_text_char),
                    "{name}: {arg:?}"
                );
            }
        }
        let long = transport(&config, "tmux").wake_argv("%3", &"é".repeat(3000));
        assert_eq!(long[0][5].chars().count(), 1000);
    }

    fn refused(text: &str) -> String {
        parse_config(text).expect_err(text)
    }

    #[test]
    fn a_config_is_parsed_in_file_order_with_defaults() {
        assert_eq!(parse_config(r#"{"version":1}"#).unwrap(), Vec::new());
        let specs = parse_config(
            r#"{"version":1,"transports":{
                "zeta":{"wake":[["z","{target}"]]},
                "alpha":{"wake":[["a"]],"confirm_secs":0,"backslash_to_slash":true,
                         "status":{"argv":["a","st","{target}"],"exit_codes":{"0":"idle","3":"working","4":"gone"}},
                         "list":{"argv":["a","ls"]},"capture_env":"A_PANE"}}}"#,
        )
        .unwrap();
        assert_eq!(specs[0].name, "zeta", "file order, not sorted");
        assert_eq!(specs[0].confirm_secs, 8);
        assert!(!specs[0].backslash_to_slash);
        assert_eq!(specs[1].confirm_secs, 0);
        let status = specs[1].status.as_ref().unwrap();
        assert_eq!(status.exit_codes[&3], State::Working);
        assert_eq!(status.exit_codes[&4], State::Gone);
        assert!(status.shows_activity());
        assert_eq!(specs[1].programs(), ["a"]);
    }

    #[test]
    fn every_broken_rule_refuses_the_whole_file() {
        let wake = r#""wake":[["t","{target}","{message}"]]"#;
        let one = |body: &str| format!(r#"{{"version":1,"transports":{{"t":{{{body}}}}}}}"#);
        let cases: Vec<(String, &str)> = vec![
            ("not json".into(), "not valid JSON"),
            ("[]".into(), "top level"),
            (r#"{"transports":{}}"#.into(), "`version` is missing"),
            (r#"{"version":2}"#.into(), "version 2"),
            (r#"{"version":"1"}"#.into(), "version"),
            (
                r#"{"version":1,"extra":true}"#.into(),
                "unknown key \"extra\"",
            ),
            (
                r#"{"version":1,"transports":[]}"#.into(),
                "must be an object",
            ),
            (
                one(&format!(r#"{wake},"shell":true"#)),
                "unknown key \"shell\"",
            ),
            (
                one(r#""status":{"argv":["t"],"exit_codes":{"0":"idle"}}"#),
                "`wake` is required",
            ),
            (one(r#""wake":[]"#), "non-empty list of commands"),
            (one(r#""wake":[[]]"#), "non-empty list of strings"),
            (
                one(r#""wake":["tmux send-keys"]"#),
                "non-empty list of strings",
            ),
            (one(r#""wake":[["t",1]]"#), "non-empty list of strings"),
            (one(r#""wake":[[""]]"#), "empty program name"),
            (
                one(r#""wake":[["{target}","x"]]"#),
                "placeholder in its program name",
            ),
            (
                one(r#""wake":[["bin/{message}"]]"#),
                "placeholder in its program name",
            ),
            (one(r#""wake":[["sh","-c","{message}"]]"#), "shell code"),
            (
                one(r#""wake":[["scripts/wake.sh","{target}"]]"#),
                "absolute path",
            ),
            (one(r#""wake":[["./tool"]]"#), "absolute path"),
            (one(r#""wake":[["../bin/tool"]]"#), "absolute path"),
            (
                one(&format!(r#"{wake},"list":{{"argv":["bin/ls"]}}"#)),
                "absolute path",
            ),
            (
                one(r#""wake":[["bash","-lc","echo {target}"]]"#),
                "shell code",
            ),
            (
                one(&format!(r#"{wake},"status":{{"argv":["t"]}}"#)),
                "needs `exit_codes`",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"exit_codes":{{"0":"idle"}}}}"#
                )),
                "needs `argv`",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"argv":["t"],"exit_codes":{{}}}}"#
                )),
                "at least one",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"argv":["t"],"exit_codes":{{"0":"busy"}}}}"#
                )),
                "not a state",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"argv":["t"],"exit_codes":{{"zero":"idle"}}}}"#
                )),
                "not a number",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"argv":["t"],"exit_codes":{{"0":"idle"}},"x":1}}"#
                )),
                "unknown key \"x\"",
            ),
            (
                one(&format!(
                    r#"{wake},"status":{{"argv":["t","{{message}}"],"exit_codes":{{"0":"idle"}}}}"#
                )),
                "no value there",
            ),
            (
                one(&format!(r#"{wake},"list":{{"argv":["t","{{target}}"]}}"#)),
                "no value there",
            ),
            (
                one(&format!(r#"{wake},"list":["t"]"#)),
                "`list` must be an object",
            ),
            (
                one(&format!(r#"{wake},"list":{{}}"#)),
                "`list` needs `argv`",
            ),
            (
                one(&format!(r#"{wake},"capture_env":"NOT A NAME""#)),
                "environment variable",
            ),
            (
                one(&format!(r#"{wake},"capture_env":"""#)),
                "environment variable",
            ),
            (
                one(&format!(r#"{wake},"backslash_to_slash":"yes""#)),
                "true or false",
            ),
            (one(&format!(r#"{wake},"confirm_secs":31"#)), "0 to 30"),
            (one(&format!(r#"{wake},"confirm_secs":-1"#)), "0 to 30"),
            (one(&format!(r#"{wake},"confirm_secs":1.5"#)), "0 to 30"),
        ];
        for (text, want) in &cases {
            let err = refused(text);
            assert!(
                err.contains(want),
                "{text}\n  gave {err:?}, wanted {want:?}"
            );
        }
        for name in ["herdr", "rex"] {
            let err = refused(&format!(
                r#"{{"version":1,"transports":{{"{name}":{{{wake}}}}}}}"#
            ));
            assert!(err.contains("reserved"), "{err}");
        }
        for name in ["Tmux", "1x", "", "a b", "-x", "x".repeat(33).as_str()] {
            let err = refused(&format!(
                r#"{{"version":1,"transports":{{"{name}":{{{wake}}}}}}}"#
            ));
            assert!(err.contains("a name is"), "{name:?}: {err}");
        }
        // One bad transport refuses the good one beside it: the file is all or nothing.
        let mixed = format!(
            r#"{{"version":1,"transports":{{"good":{{{wake}}},"bad":{{"wake":[["sh","-c","{{message}}"]]}}}}}}"#
        );
        let config = Config::from_json(&mixed);
        assert!(config.transports.is_empty());
        assert!(config.error.unwrap().contains("transport \"bad\""));
        // The lint knows `-c` and short-flag groups ending in `c`, and nothing else.
        assert!(shell_code_flag("-c") && shell_code_flag("-lc") && shell_code_flag("-ic"));
        assert!(!shell_code_flag("--c") && !shell_code_flag("-l") && !shell_code_flag("c"));
        assert!(!shell_code_flag("-") && !shell_code_flag("-1c"));
    }

    #[test]
    fn list_lines_are_read_as_specified() {
        let sessions = |text: &str| match parse_list("t", text.as_bytes()) {
            ListResult::Sessions(sessions) => sessions,
            other => panic!("{text:?} gave {other:?}"),
        };
        let got = sessions("%1\n\n%2\tidle\n%3\tworking\tsess-3\r\n%4\tpresent\ts 4\n");
        let brief: Vec<(&str, State, Option<&str>)> = got
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
                ("%4", State::Present, Some("s 4")),
            ]
        );
        assert!(got
            .iter()
            .all(|s| s.observation.program.is_none() && s.observation.cwd.is_none()));
        assert!(sessions("").is_empty(), "an empty list is a real answer");
        for bad in [
            "%1\tbusy\n",
            "%1\tgone\n",
            "%1\tno_answer\n",
            "%1\tidle\ts\textra\n",
            "%1\tidle\t\n",
            "%1\t\tsess\n",
            "-x\n",
            "   \n",
            "%1\tidle\ta\x1bb\n",
            "%1\n\tidle\n",
        ] {
            assert!(
                matches!(parse_list("t", bad.as_bytes()), ListResult::NoAnswer(_)),
                "{bad:?}"
            );
        }
        let long = format!("%1\tidle\t{}\n", "s".repeat(201));
        assert!(matches!(
            parse_list("t", long.as_bytes()),
            ListResult::NoAnswer(_)
        ));
        assert!(matches!(
            parse_list("t", &[0xff, 0xfe]),
            ListResult::NoAnswer(_)
        ));
    }

    #[test]
    fn a_name_that_is_not_provided_says_what_to_set() {
        let none = Config::default();
        assert!(none.provides("herdr") && !none.provides("tmux") && !none.provides("rex"));
        assert_eq!(
            none.why_missing("rex"),
            "the rex transport is not in this build"
        );
        let why = none.why_missing("tmux");
        assert!(
            why.contains("transports.json") && why.contains("docs/WAKE.md"),
            "{why}"
        );
        let broken = Config::from_json("{");
        let why = broken.why_missing("tmux");
        assert!(
            why.contains("not valid JSON") && why.contains("docs/WAKE.md"),
            "{why}"
        );
    }
}
