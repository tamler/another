mod plugin;

fn main() -> std::process::ExitCode {
    // On a thread with a known stack: see `another::cli::with_cli_stack`.
    another::cli::with_cli_stack(run)
}

fn run() -> std::process::ExitCode {
    // `another <name>` with no built-in `<name>` runs `another-<name>` from PATH, like git.
    if let Some(code) = plugin::dispatch() {
        return code;
    }
    another::cli::main()
}
