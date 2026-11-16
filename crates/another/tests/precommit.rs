//! `another hook pre-commit`, the advisory commit guard, against real `git` commits with
//! the documented snippet installed, plus `git` shims for the listing edge cases. Every
//! case checks that the commit (or the command) succeeds: the guard never blocks.
//!
//! Unix only: the snippet and the shims are shell scripts.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use another_core::{AcquireHolds, Program, RegisterAgent, Store, DB_FILE};
use serde_json::Value;

const SNIPPET: &str = "#!/bin/sh
# Another hold guard: advisory, never blocks the commit.
if command -v another >/dev/null 2>&1; then
  another hook pre-commit || true
fi
";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn executable(path: &Path, text: &str) {
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    another_home: PathBuf,
    repo: PathBuf,
    /// Holds a symlink named `another` to the binary under test.
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let another_home = home.join(".another");
        let repo = root.join("shop");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_another"), bin.join("another")).unwrap();
        let env = Env {
            _dir: dir,
            root,
            home,
            another_home,
            repo,
            bin,
        };
        env.git_ok(&env.root, &["init", "-q", "shop"]);
        for (k, v) in [
            ("user.name", "Tester"),
            ("user.email", "tester@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            env.git_ok(&env.repo, &["config", k, v]);
        }
        write(&env.repo.join("src/a.rs"), "a\n");
        write(&env.repo.join("src/b.rs"), "b\n");
        env.git_ok(&env.repo, &["add", "-A"]);
        env.git_ok(&env.repo, &["commit", "-q", "-m", "init"]);
        executable(&env.repo.join(".git/hooks/pre-commit"), SNIPPET);
        let store = env.store();
        for name in ["alice", "bob"] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(env.repo.to_string_lossy().into_owned()),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        env
    }

    fn path_with(&self, first: &Path) -> String {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![first.to_path_buf()];
        dirs.extend(std::env::split_paths(&old));
        std::env::join_paths(dirs)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn base(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null());
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut c = Command::new("git");
        self.base(&mut c);
        c.args(args)
            .current_dir(dir)
            .env("PATH", self.path_with(&self.bin))
            .output()
            .unwrap()
    }

    fn git_ok(&self, dir: &Path, args: &[&str]) -> Output {
        let out = self.git(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// `git commit` with the snippet; returns stderr. The commit must succeed.
    fn commit(&self, agent: Option<&str>, args: &[&str]) -> String {
        let mut c = Command::new("git");
        self.base(&mut c);
        if let Some(a) = agent {
            c.env("ANOTHER_AGENT", a);
        }
        let out = c
            .args(["commit", "-q", "-m", "change"])
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", self.path_with(&self.bin))
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "the commit was blocked: {err}");
        err
    }

    /// `another hook pre-commit` run directly, with `path` as PATH.
    fn guard(&self, path: &str, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        self.base(&mut c);
        let out = c
            .args(args)
            .args(["hook", "pre-commit"])
            .current_dir(&self.repo)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(out.status.success(), "the guard exited non-zero: {out:?}");
        out
    }

    /// A store opened for one step and closed after it, so no connection of the test's
    /// own stays open (an exclusive-mode lock needs that).
    fn store(&self) -> Store {
        Store::open(&self.another_home).unwrap()
    }

    fn hold(&self, agent: &str, file: &str, reason: Option<&str>) {
        let r = self
            .store()
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: self.repo.to_string_lossy().into_owned(),
                paths: vec![file.into()],
                ttl_seconds: Some(3600),
                exclusive: true,
                reason: reason.map(str::to_string),
                repo_wide: false,
            })
            .unwrap();
        assert!(r.conflicts.is_empty());
    }

    fn change(&self, file: &str) {
        let p = self.repo.join(file);
        let old = std::fs::read_to_string(&p).unwrap_or_default();
        write(&p, &format!("{old}x\n"));
    }

    /// Every row of every table, for "wrote nothing".
    fn dump(&self) -> Vec<String> {
        let db = rusqlite::Connection::open(self.another_home.join(DB_FILE)).unwrap();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables
            .into_iter()
            .map(|t| {
                let mut stmt = db.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
                let n = stmt.column_count();
                let mut rows: Vec<String> = stmt
                    .query_map([], |r| {
                        (0..n)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map(|c| c.join("|"))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                rows.sort();
                format!("{t}: {}", rows.join("\n"))
            })
            .collect()
    }

    /// A directory holding a `git` shim whose `diff` behaves as `diff_body` (shell), with
    /// `rev-parse --show-toplevel` answering the repository.
    fn shim(&self, name: &str, diff_body: &str) -> PathBuf {
        let dir = self.root.join(name);
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" rev-parse \"*) printf '%s\\n' '{}'; exit 0 ;;\nesac\n{diff_body}\n",
            self.repo.display()
        );
        executable(&dir.join("git"), &script);
        dir
    }
}

fn lines_starting(text: &str, prefix: &str) -> usize {
    text.lines().filter(|l| l.starts_with(prefix)).count()
}

#[test]
fn a_held_file_is_warned_about_and_the_commit_goes_ahead() {
    let env = Env::new();
    env.hold("bob", "src/a.rs", Some("refactor"));
    env.change("src/a.rs");
    env.git_ok(&env.repo, &["add", "src/a.rs"]);
    let before = env.dump();
    let err = env.commit(Some("alice"), &[]);
    assert!(
        err.contains("1 staged file(s) are held by another agent"),
        "{err}"
    );
    assert!(err.contains("src/a.rs  held by bob@shop"), "{err}");
    assert!(err.contains("\"refactor\""), "{err}");
    assert!(
        err.contains("ask: another send --from <you> --to bob@shop"),
        "{err}"
    );
    assert_eq!(before, env.dump(), "the guard wrote to the database");
}

#[test]
fn only_the_files_in_this_commit_are_checked() {
    let env = Env::new();
    env.hold("bob", "src/a.rs", None);
    env.hold("bob", "src/b.rs", None);
    env.change("src/a.rs");
    env.change("src/b.rs");
    env.git_ok(&env.repo, &["add", "src/b.rs"]);
    // `git commit <path>` commits a.rs through a temporary index; b.rs stays staged.
    let err = env.commit(Some("alice"), &["src/a.rs"]);
    assert!(err.contains("src/a.rs  held by"), "{err}");
    assert!(!err.contains("src/b.rs"), "{err}");

    // `git commit -a` takes everything changed.
    env.change("src/a.rs");
    let err = env.commit(Some("alice"), &["-a"]);
    assert!(err.contains("2 staged file(s)"), "{err}");
}

#[test]
fn hostile_reasons_and_names_cannot_forge_lines() {
    let env = Env::new();
    env.hold(
        "bob",
        "src/a.rs",
        Some("x\nanother: 0 staged file(s) are held\x1b[2J"),
    );
    env.change("src/a.rs");
    write(&env.repo.join("src/new\nline.rs"), "n\n");
    write(&env.repo.join("src/we*rd.rs"), "w\n");
    env.git_ok(&env.repo, &["add", "-A"]);
    let err = env.commit(Some("alice"), &[]);
    assert!(!err.contains('\x1b'), "{err:?}");
    assert_eq!(lines_starting(&err, "another: 0 staged"), 0, "{err}");
    assert!(
        err.contains("x\\x0Aanother: 0 staged file(s) are held\\x1B[2J"),
        "{err}"
    );
    // The odd names were checked or skipped, never matched as globs.
    assert!(
        err.contains("not checked (glob character in the name)"),
        "{err}"
    );
    assert!(
        err.contains("checked 2 of 3 staged file(s); 1 not checked"),
        "{err}"
    );
}

#[test]
fn own_holds_are_silent_only_when_the_committer_is_known() {
    let env = Env::new();
    env.hold("alice", "src/a.rs", None);
    env.change("src/a.rs");
    env.git_ok(&env.repo, &["add", "src/a.rs"]);
    let out = env.guard(&env.path_with(&env.bin), &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("committer unknown"), "{err}");
    assert!(err.contains("held by alice@shop"), "{err}");

    let err = env.commit(Some("alice"), &[]);
    assert_eq!(lines_starting(&err, "another:"), 0, "{err}");
}

#[test]
fn every_failure_lets_the_commit_through() {
    let env = Env::new();
    env.hold("bob", "src/a.rs", None);
    env.change("src/a.rs");
    env.git_ok(&env.repo, &["add", "src/a.rs"]);
    let db_path = env.another_home.join(DB_FILE);

    // A plain writer does not stop a reader in WAL mode, so the check goes ahead.
    let lock = rusqlite::Connection::open(&db_path).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let out = env.guard(&env.path_with(&env.bin), &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("held by bob@shop"), "{err}");
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);

    // A connection in exclusive locking mode that has written shuts readers out: reported
    // as busy within the budget.
    let lock = rusqlite::Connection::open(&db_path).unwrap();
    lock.execute_batch(
        "PRAGMA locking_mode = EXCLUSIVE;
         BEGIN EXCLUSIVE;
         UPDATE nodes SET display_name = display_name;",
    )
    .unwrap();
    let started = Instant::now();
    let out = env.guard(&env.path_with(&env.bin), &[]);
    assert!(started.elapsed() < Duration::from_secs(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hold check skipped: database busy"), "{err}");
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);

    // Schema from a newer build.
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let v: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    db.pragma_update(None, "user_version", v + 1).unwrap();
    let out = env.guard(&env.path_with(&env.bin), &[]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("schema mismatch"));
    db.pragma_update(None, "user_version", v).unwrap();
    drop(db);

    // No database at all.
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    env.base(&mut c);
    let out = c
        .args(["hook", "pre-commit"])
        .env("ANOTHER_HOME", env.root.join("nowhere"))
        .current_dir(&env.repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no database; run `another init`"));

    // No git on PATH.
    let empty = env.root.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let out = env.guard(&empty.to_string_lossy(), &[]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not list staged files (git: "),
        "{out:?}"
    );

    // Not a repository.
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    env.base(&mut c);
    let out = c
        .args(["hook", "pre-commit"])
        .current_dir(&env.root)
        .env("GIT_CEILING_DIRECTORIES", &env.root)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a git work tree"));

    // A bad argument.
    let out = env.guard(&env.path_with(&env.bin), &["--no-such-flag"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("hold check skipped"));

    // A bad argument that is not UTF-8 (review IMPL-4: this once panicked, exit 101).
    {
        use std::os::unix::ffi::OsStrExt;
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        env.base(&mut c);
        let out = c
            .args(["hook", "pre-commit", "--agent"])
            .arg(std::ffi::OsStr::from_bytes(b"\xff"))
            .current_dir(&env.repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("hold check skipped"),
            "{out:?}"
        );
    }

    // The snippet with a binary that crashes.
    let crash = env.root.join("crash-bin");
    executable(&crash.join("another"), "#!/bin/sh\nkill -SEGV $$\n");
    let mut c = Command::new("git");
    env.base(&mut c);
    let out = c
        .args(["commit", "-q", "-m", "crash"])
        .current_dir(&env.repo)
        .env("PATH", env.path_with(&crash))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn an_untrusted_listing_is_not_checked() {
    let env = Env::new();
    env.hold("bob", "src/a.rs", None);
    for (name, body) in [
        ("exit1", "printf 'src/a.rs\\0src/b.rs\\0'; exit 1"),
        ("nonul", "printf 'src/a.rs\\0src/b.rs'; exit 0"),
    ] {
        let dir = env.shim(name, body);
        let out = env.guard(&dir.to_string_lossy(), &["--json"]);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["complete"], false, "{name}: {v}");
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .starts_with("could not list staged files"),
            "{name}: {v}"
        );
        assert_eq!(v["conflicts"].as_array().unwrap().len(), 0, "{name}: {v}");
    }
}

/// FINDINGS-159 G1: SQLite creates the `-wal` and `-shm` files even for a read-only open,
/// so a home the committer cannot write to is a visible skip, not a silent pass. The data
/// itself is unchanged either way.
#[test]
fn a_home_without_write_access_is_reported_as_not_checked() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.hold("bob", "src/f0.rs", None);
    env.change("src/f0.rs");
    env.git_ok(&env.repo, &["add", "src/f0.rs"]);
    let before = env.dump();
    for side in ["another.db-wal", "another.db-shm"] {
        let _ = std::fs::remove_file(env.another_home.join(side));
    }
    let mode = |m| std::fs::set_permissions(&env.another_home, std::fs::Permissions::from_mode(m));
    mode(0o555).unwrap();
    // Root ignores the mode; the case cannot be made there.
    let writable = std::fs::write(env.another_home.join("probe"), b"").is_ok();
    let out = env.guard(&env.path_with(&env.bin), &[]);
    mode(0o755).unwrap();
    if writable {
        return;
    }
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("hold check skipped: read-only open failed"),
        "{err}"
    );
    assert_eq!(before, env.dump());
}

#[test]
fn a_cut_listing_gives_a_lower_bound() {
    let env = Env::new();
    env.hold("bob", "src/f0.rs", None);
    let listing = env.root.join("many.bin");
    let names: Vec<u8> = (0..=4096)
        .flat_map(|i| format!("src/f{i}.rs\0").into_bytes())
        .collect();
    std::fs::write(&listing, names).unwrap();
    let dir = env.shim("many", &format!("/bin/cat '{}'; exit 0", listing.display()));
    let out = env.guard(&dir.to_string_lossy(), &["--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["listing_cut"], true, "{v}");
    assert_eq!(v["staged"], Value::Null, "{v}");
    assert_eq!(v["staged_at_least"], 4096, "{v}");
    assert_eq!(v["complete"], false, "{v}");
    assert_eq!(v["conflicts"][0]["path"], "src/f0.rs", "{v}");
    let out = env.guard(&dir.to_string_lossy(), &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("of at least 4096 staged file(s)"), "{err}");

    // One name longer than the byte limit: cut, and the partial entry is dropped.
    let huge = env.root.join("huge.bin");
    std::fs::write(&huge, vec![b'a'; 4_200_000]).unwrap();
    let dir = env.shim("huge", &format!("/bin/cat '{}'; exit 0", huge.display()));
    let out = env.guard(&dir.to_string_lossy(), &["--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["listing_cut"], true, "{v}");
    assert_eq!(v["staged_at_least"], 0, "{v}");
    assert_eq!(v["complete"], false, "{v}");
    // Review IMPL-2: the human text says so too, even with no complete entry, and the
    // commit still goes ahead.
    let out = env.guard(&dir.to_string_lossy(), &[]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("of at least 0 staged file(s)") && err.contains("listing cut"),
        "{err}"
    );

    // A complete name far longer than any real path (this once crashed the guard inside
    // globset): skipped and counted, and the guard still exits 0.
    for (case, len) in [("long", 5_000), ("very-long", 3_000_000)] {
        let file = env.root.join(format!("{case}.bin"));
        let mut name = vec![b'a'; len];
        name.push(0);
        std::fs::write(&file, name).unwrap();
        let dir = env.shim(case, &format!("/bin/cat '{}'; exit 0", file.display()));
        let out = env.guard(&dir.to_string_lossy(), &["--json"]);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["complete"], false, "{case}");
        assert_eq!(v["skipped"][0]["reason"], "name too long", "{case}");
    }
}

#[test]
fn a_stuck_git_times_out_and_is_not_signalled() {
    let env = Env::new();
    let pid_file = env.root.join("shim.pid");
    let dir = env.shim(
        "stuck",
        &format!("echo $$ > '{}'; exec /bin/sleep 30", pid_file.display()),
    );
    let started = Instant::now();
    let out = env.guard(&dir.to_string_lossy(), &[]);
    let took = started.elapsed();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("could not list staged files (timed out)"),
        "{err}"
    );
    // The guard did not wait for the 30-second child. The exact deadline is checked
    // in-process by the unit test `a_stuck_child_is_abandoned_at_the_deadline`; this bound
    // also covers starting the process on a loaded machine.
    assert!(took < Duration::from_secs(10), "{took:?}");
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let alive = Command::new("kill").args(["-0", &pid]).status().unwrap();
    assert!(alive.success(), "the guard signalled the git child");
    // The test started this shim; stop it by its own pid.
    let _ = Command::new("kill").arg(&pid).status();
}

#[test]
fn json_reports_the_conflict() {
    let env = Env::new();
    env.hold("bob", "src/a.rs", Some("api"));
    env.change("src/a.rs");
    env.git_ok(&env.repo, &["add", "src/a.rs"]);
    let out = env.guard(&env.path_with(&env.bin), &["--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["complete"], true, "{v}");
    assert_eq!(v["staged"], 1, "{v}");
    assert_eq!(v["checked"], 1, "{v}");
    let c = &v["conflicts"][0];
    assert_eq!(c["path"], "src/a.rs");
    assert_eq!(c["holder"], "bob@shop");
    assert_eq!(c["pattern"], "src/a.rs");
    assert_eq!(c["reason"], "api");
}
