//! External subcommands, the way git does them: `another <name> [args…]` runs an executable
//! called `another-<name>` found on `PATH`, when `<name>` is not a built-in command.
//!
//! The rules (docs/PLUGINS.md):
//! - built-in commands and `help` always win; an executable can never replace one;
//! - `<name>` must be the first argument and match `[a-z0-9][a-z0-9-]{0,62}`, so it can never
//!   name a path, an option or a hidden file;
//! - only absolute `PATH` entries are searched, in order, so the current directory is never
//!   searched by accident (an empty or `.` entry);
//! - on Unix the file must be a regular file with an execute bit; on Windows it must be
//!   `another-<name>.exe` (never a `.bat` or `.cmd`, which would go through `cmd.exe`);
//! - the arguments after `<name>` are passed unchanged, with no shell, and stdin, stdout and
//!   stderr are inherited. Nothing is added to the environment, and the database is not opened;
//! - the exit status is the executable's (on Unix the process is replaced with `exec`, so a
//!   signal is reported the same way too).
//!
//! When no such executable exists, nothing happens here and clap reports the unknown command
//! as before, with its spelling suggestions.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::CommandFactory;

/// Run `another-<name>` if the command line asks for an external subcommand that exists.
/// `None` means "not an external subcommand": parse the command line as usual.
pub fn dispatch() -> Option<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let name = args.get(1)?.to_str()?;
    if !valid_name(name) || is_builtin(name) {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let exe = find(name, &path)?;
    Some(run(&exe, &args[2..]))
}

/// A name that can only ever form a plain file name: lowercase ASCII letters, digits and
/// hyphens, not starting with a hyphen, at most 63 characters.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// A built-in command or alias, or `help`, which clap adds itself.
fn is_builtin(name: &str) -> bool {
    name == "help"
        || crate_cli()
            .get_subcommands()
            .any(|c| c.get_name() == name || c.get_all_aliases().any(|a| a == name))
}

fn crate_cli() -> clap::Command {
    another::cli::Cli::command()
}

/// The file name for `<name>` on this platform.
fn file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("another-{name}.exe")
    } else {
        format!("another-{name}")
    }
}

/// The first absolute `PATH` entry holding a runnable `another-<name>`.
pub fn find(name: &str, path: &OsStr) -> Option<PathBuf> {
    let file = file_name(name);
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&file))
        .find(|candidate| runnable(candidate))
}

#[cfg(unix)]
fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Replace this process with the executable, so its exit status and any signal are ours.
#[cfg(unix)]
fn run(exe: &Path, args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe).args(args).exec();
    eprintln!("another: could not run {}: {err}", exe.display());
    ExitCode::from(126)
}

/// Run the executable and exit with its status. Windows exit codes are 32-bit, wider than
/// `ExitCode` allows, so this exits directly.
#[cfg(not(unix))]
fn run(exe: &Path, args: &[OsString]) -> ExitCode {
    match std::process::Command::new(exe).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("another: could not run {}: {err}", exe.display());
            ExitCode::from(126)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_file_names_only() {
        for good in ["report", "a", "x1", "team-report", "0day", &"a".repeat(63)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "--json",
            ".hidden",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "Report",
            "rep ort",
            "rep.ort",
            "rep_ort",
            "é",
            &"a".repeat(64),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn built_in_commands_aliases_and_help_are_reserved() {
        for name in ["status", "board", "send", "inbox", "serve", "help"] {
            assert!(is_builtin(name), "{name}");
        }
        assert!(!is_builtin("team-report"));
        // Every built-in name is checked, aliases included.
        for c in crate_cli().get_subcommands() {
            assert!(is_builtin(c.get_name()));
            for a in c.get_all_aliases() {
                assert!(is_builtin(a), "{a}");
            }
        }
    }

    #[test]
    fn relative_path_entries_are_never_searched() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(file_name("rel"));
        std::fs::write(&exe, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A relative entry naming the same directory, as `.` would from inside it.
        let cwd = std::env::current_dir().unwrap();
        let relative = pathdiff(dir.path(), &cwd);
        let path = std::env::join_paths([relative.clone()]).unwrap();
        assert!(!relative.is_absolute());
        assert_eq!(find("rel", &path), None);
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find("rel", &path), Some(exe));
    }

    /// `to` relative to `from`, by climbing to the root: enough for a test.
    fn pathdiff(to: &Path, from: &Path) -> PathBuf {
        let mut rel = PathBuf::new();
        for _ in from.components().skip(1) {
            rel.push("..");
        }
        for c in to.components().skip(1) {
            rel.push(c);
        }
        rel
    }
}
