//! Resource commands for people: `another resource list|borrow|return`, the lease lines in
//! the listing, and `borrow --wait` for an exclusive resource.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .current_dir(cwd)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run another")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("shop");
        std::fs::create_dir_all(&cwd).unwrap();
        let fx = Fx {
            _dir: dir,
            home,
            cwd,
        };
        fx.ok(&["init"]);
        for (name, program) in [("a", "claude"), ("b", "codex")] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                program,
                "--project",
                ".",
            ]);
        }
        fx.ok(&[
            "resource",
            "register",
            "model_server",
            "ollama@11434",
            "-a",
            "a",
            "--port",
            "11434",
            "--purpose",
            "llama3 for tests",
        ]);
        fx.ok(&[
            "resource",
            "register",
            "gpu",
            "metal:0",
            "-a",
            "a",
            "--exclusive",
        ]);
        fx
    }

    fn run(&self, args: &[&str]) -> Output {
        run(&self.home, &self.cwd, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }
}

#[test]
fn the_listing_shows_where_and_who_holds_each_lease() {
    let fx = Fx::new();
    fx.ok(&["resource", "borrow", "model_server:ollama@11434", "-a", "b"]);
    let list = fx.ok(&["resource", "list"]);
    let lines: Vec<&str> = list.lines().collect();
    let ollama = lines
        .iter()
        .position(|l| l.contains("ollama@11434"))
        .unwrap();
    assert!(lines[ollama].contains("port 11434"), "{list}");
    assert!(lines[ollama].contains("(llama3 for tests)"), "{list}");
    // Owner first, then borrowers, each with an expiry.
    assert!(
        lines[ollama + 1]
            .trim_start()
            .starts_with("owner a@shop until "),
        "{list}"
    );
    assert!(
        lines[ollama + 2]
            .trim_start()
            .starts_with("borrower b@shop until "),
        "{list}"
    );
    // `another resources` is the same command.
    assert_eq!(fx.ok(&["resources"]), list);
}

#[test]
fn return_gives_back_a_borrowed_lease() {
    let fx = Fx::new();
    fx.ok(&["borrow", "model_server:ollama@11434", "-a", "b"]);
    let out = fx.ok(&["resource", "return", "model_server:ollama@11434", "-a", "b"]);
    assert!(
        out.starts_with("released model_server ollama@11434"),
        "{out}"
    );
    assert!(!fx.ok(&["resources"]).contains("borrower b@shop"));
}

#[test]
fn borrow_wait_gives_up_after_its_time_on_an_exclusive_resource() {
    let fx = Fx::new();
    let started = Instant::now();
    let out = fx.run(&[
        "resource",
        "borrow",
        "gpu:metal:0",
        "-a",
        "b",
        "--wait",
        "2",
    ]);
    let took = started.elapsed();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("waiting up to 2s"), "{err}");
    assert!(
        err.contains("still held exclusively by another agent after 2s"),
        "{err}"
    );
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");

    // Without --wait it fails at once.
    let started = Instant::now();
    let out = fx.run(&["borrow", "gpu:metal:0", "-a", "b"]);
    assert!(!out.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn borrow_wait_takes_the_resource_once_it_is_free() {
    let fx = Fx::new();
    let (home, cwd) = (fx.home.clone(), fx.cwd.clone());
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let out = run(&home, &cwd, &["release", "gpu:metal:0", "-a", "a"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
    });
    let out = fx.run(&["borrow", "gpu:metal:0", "-a", "b", "--wait", "30"]);
    releaser.join().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("owner b@shop until"), "{stdout}");
}

/// Review R1 of #161 (P2): a retry blocked on the database lock past the deadline must not
/// succeed when the lock is let go. The test holds an immediate transaction for 2 s
/// against `--wait 1`, frees the resource inside it, and commits: the late attempt gives up.
#[test]
fn borrow_wait_does_not_acquire_after_its_deadline() {
    use std::io::{BufRead, BufReader};
    let fx = Fx::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["borrow", "gpu:metal:0", "-a", "b", "--wait", "1"])
        .current_dir(&fx.cwd)
        .env("ANOTHER_HOME", &fx.home)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut notice = String::new();
    err.read_line(&mut notice).unwrap();
    assert!(notice.contains("waiting up to 1s"), "{notice}");
    let db = rusqlite::Connection::open(fx.home.join("another.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    db.execute_batch("UPDATE resource_leases SET expires_at = 0; COMMIT")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut err, &mut rest).unwrap();
    assert!(
        !out.status.success(),
        "acquired after the wait: {}",
        text(&out.stdout)
    );
    // The lock holder expired every lease: the resource is free now, so the message must
    // not say it is held (review R2 P3), only that the wait ended.
    assert!(
        rest.contains("was not borrowed: the 1s wait ended"),
        "{rest}"
    );
    assert!(!rest.contains("still held"), "{rest}");
}

#[test]
fn borrow_wait_has_a_ceiling() {
    let fx = Fx::new();
    let out = fx.run(&["borrow", "gpu:metal:0", "-a", "b", "--wait", "3601"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("at most 3600 seconds"));
}
