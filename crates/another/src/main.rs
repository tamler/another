mod plugin;

fn main() -> std::process::ExitCode {
    // `another <name>` with no built-in `<name>` runs `another-<name>` from PATH, like git.
    if let Some(code) = plugin::dispatch() {
        return code;
    }
    another::cli::main()
}
