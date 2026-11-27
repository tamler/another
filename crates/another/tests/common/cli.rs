use std::path::Path;
use std::process::Command;

const SCRUBBED_ENV: &[&str] = &[
    "ANOTHER_AGENT",
    "ANOTHER_LOG",
    "ANOTHER_ADMIT_KEY",
    "ANOTHER_NODE_NAME",
    "GROK_HOOK_EVENT",
    "HERDR_PANE_ID",
    "HERDR_BIN_PATH",
    "TERM_PROGRAM",
    "TMUX",
    "TMUX_PANE",
    "CMUX_SURFACE_ID",
];

pub fn another_cmd(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_another"));
    for variable in SCRUBBED_ENV {
        command.env_remove(variable);
    }
    command.env("ANOTHER_HOME", home);
    command
}
