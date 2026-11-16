//! External subcommands (docs/PLUGINS.md): `another <name>` runs `another-<name>` from PATH
//! when `<name>` is not built in.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_another");

/// Run `another` with exactly `path` as PATH and a home that does not exist.
fn another(path: &[&Path], home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("ANOTHER_HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A copy of the `another` binary itself installed as `another-<name>`. This is a real
/// executable on every platform, so the dispatch path runs on Windows too.
fn install_self_copy(dir: &Path, name: &str) -> PathBuf {
    let file = if cfg!(windows) {
        format!("another-{name}.exe")
    } else {
        format!("another-{name}")
    };
    let dest = dir.join(file);
    std::fs::copy(BIN, &dest).unwrap();
    dest
}

#[test]
fn an_external_command_runs_with_its_arguments_and_exit_status() {
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let missing_home = home.path().join("never-created");
    install_self_copy(bin.path(), "selfcopy");

    // `another selfcopy --version` runs `another-selfcopy --version`.
    let out = another(&[bin.path()], &missing_home, &["selfcopy", "--version"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("another "),
        "{}",
        text(&out.stdout)
    );

    // The child's own failure comes back as ours: clap in the child exits 2 for an unknown
    // flag. (A child that is itself `another` looking for `another-<x>` finds nothing.)
    let out = another(
        &[bin.path()],
        &missing_home,
        &["selfcopy", "--no-such-flag"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));

    // Dispatch opens nothing: the home directory is not created.
    assert!(!missing_home.exists());
}

#[test]
fn an_unknown_command_with_no_executable_is_still_clap_s_error() {
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let out = another(&[bin.path()], home.path(), &["stauts"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("unrecognized subcommand"), "{err}");
    assert!(
        err.contains("status"),
        "the spelling suggestion is kept: {err}"
    );
}

#[test]
fn a_built_in_command_is_never_replaced() {
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // `another-board` and `another-help` are on PATH, but the built-ins run.
    install_self_copy(bin.path(), "board");
    install_self_copy(bin.path(), "help");
    let out = another(&[bin.path()], &home.path().join("h"), &["help"]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("Usage: another"),
        "{}",
        text(&out.stdout)
    );
    // Run as a dispatched child, the self-copy would get `--help` alone and print the
    // top-level usage. The built-in prints the board's own help. Clap names the binary
    // `another.exe` on Windows, so match the subcommand part only.
    let out = another(&[bin.path()], &home.path().join("h"), &["board", "--help"]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(" board [OPTIONS]") && stdout.contains("Read-only work board"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn the_name_must_be_the_first_argument() {
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    install_self_copy(bin.path(), "selfcopy");
    let out = another(
        &[bin.path()],
        home.path(),
        &["--json", "selfcopy", "--version"],
    );
    // Not dispatched: under --json the unknown command is a JSON usage error on stdout
    // (exit 2), and the external command never ran (it would print its version).
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    let err: serde_json::Value = serde_json::from_slice(&out.stdout).expect("a JSON error");
    assert_eq!(err["error"]["code"], "usage", "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unrecognized subcommand"),
        "{err}"
    );
    assert!(!text(&out.stdout).contains("another "), "the plugin ran");
    assert!(
        std::fs::read_dir(home.path()).unwrap().next().is_none(),
        "a home was created"
    );

    // Without --json, clap's own text on stderr, as before.
    let out = another(&[bin.path()], home.path(), &["--home", "x", "selfcopy"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    assert!(text(&out.stderr).contains("unrecognized subcommand"));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;

    fn script(dir: &Path, file: &str, body: &str, mode: u32) -> PathBuf {
        let p = dir.join(file);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn arguments_pass_unchanged_with_no_shell_and_the_environment_is_not_extended() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(
            bin.path(),
            "another-echo",
            r#"for a in "$@"; do printf '[%s]\n' "$a"; done; env | grep '^ANOTHER' | sort; exit 7"#,
            0o755,
        );
        let args = [
            "echo",
            "two words",
            "--json",
            "$(touch pwned)",
            "; rm -rf x",
            "*",
            "",
        ];
        let out = Command::new(BIN)
            .args(args)
            .current_dir(home.path())
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
            .env("ANOTHER_HOME", home.path().join("h"))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7), "{}", text(&out.stderr));
        let stdout = text(&out.stdout);
        let expected = "[two words]\n[--json]\n[$(touch pwned)]\n[; rm -rf x]\n[*]\n[]\n";
        assert!(stdout.starts_with(expected), "{stdout}");
        // Only what the caller set: dispatch adds no ANOTHER_* variable.
        let env_lines: Vec<&str> = stdout[expected.len()..].lines().collect();
        assert_eq!(env_lines.len(), 1, "{env_lines:?}");
        assert!(env_lines[0].starts_with("ANOTHER_HOME="));
        assert!(!home.path().join("pwned").exists());
        assert!(!home.path().join("h").exists());
    }

    #[test]
    fn stdin_is_inherited() {
        use std::io::Write;
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(bin.path(), "another-cat", "exec cat", 0o755);
        let mut child = Command::new(BIN)
            .arg("cat")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
            .env("ANOTHER_HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(text(&out.stdout), "hello\n");
    }

    #[test]
    fn a_signal_is_reported_as_the_child_s() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(bin.path(), "another-die", "kill -TERM $$", 0o755);
        let out = Command::new(BIN)
            .arg("die")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
            .env("ANOTHER_HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(out.status.signal(), Some(15), "{:?}", out.status);
    }

    #[test]
    fn non_executable_files_and_relative_path_entries_are_skipped() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(first.path(), "another-pick", "echo first", 0o644);
        script(second.path(), "another-pick", "echo second", 0o755);
        // `.` and an empty entry both mean the current directory to a shell.
        script(cwd.path(), "another-pick", "echo cwd", 0o755);
        let path = format!(".::{}:{}", first.path().display(), second.path().display());
        let out = Command::new(BIN)
            .arg("pick")
            .current_dir(cwd.path())
            .env("PATH", path)
            .env("ANOTHER_HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(text(&out.stdout), "second\n", "{}", text(&out.stderr));
    }

    #[test]
    fn names_that_are_not_plain_never_run() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for file in [
            "another-Upper",
            "another-.hidden",
            "another-a_b",
            "another--x",
        ] {
            script(bin.path(), file, "echo RAN", 0o755);
        }
        for name in ["Upper", ".hidden", "a_b", "-x"] {
            let out = another(&[bin.path()], home.path(), &[name]);
            assert!(!text(&out.stdout).contains("RAN"), "{name} ran");
            assert_eq!(out.status.code(), Some(2), "{name}");
        }
    }

    #[test]
    fn an_executable_that_cannot_start_is_a_clear_error() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let p = bin.path().join("another-broken");
        std::fs::write(&p, "#!/no/such/interpreter\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = another(&[bin.path()], home.path(), &["broken"]);
        assert_eq!(out.status.code(), Some(126));
        assert!(
            text(&out.stderr).contains("could not run"),
            "{}",
            text(&out.stderr)
        );
    }
}

/// Adversarial F1 (review of #153): a file this user cannot execute (an execute bit for
/// group or others only) earlier on PATH must not shadow a runnable one later on PATH, the
/// way `execvp` and shells skip it.
#[cfg(unix)]
#[test]
fn adv153_an_entry_this_user_cannot_run_does_not_shadow_a_later_one() {
    use std::os::unix::fs::PermissionsExt;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let blocked = install_self_copy(first.path(), "shadow");
    // Executable for the group only: not for this user (the file's owner).
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o010)).unwrap();
    install_self_copy(second.path(), "shadow");
    let out = another(
        &[first.path(), second.path()],
        &home.path().join("none"),
        &["shadow", "--version"],
    );
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).starts_with("another "),
        "{}",
        text(&out.stdout)
    );
}
