mod plugin;

use std::process::ExitCode;

/// Stack for the thread that does the work: the Linux and macOS main-thread default.
/// Windows gives the main thread 1 MiB, and a debug build came within a few percent of
/// that building clap's command tree (`plugin::dispatch`) and running `another board`;
/// #158's Windows CI overflowed it.
const STACK_BYTES: usize = 8 << 20;

fn main() -> ExitCode {
    match std::thread::Builder::new()
        .name("another".into())
        .stack_size(STACK_BYTES)
        .spawn(another_main)
    {
        // A panic has already printed its message; 101 is Rust's panic exit status.
        Ok(thread) => thread.join().unwrap_or(ExitCode::from(101)),
        Err(_) => another_main(),
    }
}

fn another_main() -> ExitCode {
    // `another <name>` with no built-in `<name>` runs `another-<name>` from PATH, like git.
    if let Some(code) = plugin::dispatch() {
        return code;
    }
    another::cli::main()
}
