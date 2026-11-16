//! `another remote up`: the authenticated server behind a tunnel. A fake `cloudflared` on
//! PATH stands in for the real one, so these tests never open a tunnel. What they pin:
//! the quick-tunnel URL becomes the server's public origin, the tunnel stops when the
//! server does (a leftover `cloudflared` would keep the machine reachable), and each way of
//! failing to start says what to do.
//!
//! The tests signal and, on a timeout, stop the processes they start, so they run only on a
//! GitHub-hosted runner (`require_hosted_runner`, as for daemon, push and wake tests). Every
//! process they start is stopped by its exact pid when a test ends, failed or not.
#![cfg(unix)]

#[path = "common/hosted_ci.rs"]
mod hosted_ci;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_another");
const HOST: &str = "quiet-river-1234.trycloudflare.com";

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Fx {
        hosted_ci::require_hosted_runner();
        let fx = Fx {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fx.bin()).unwrap();
        let out = fx.another(&["init"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        fx
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// Where the fake cloudflared writes its pid and arguments.
    fn pid_file(&self) -> PathBuf {
        self.dir.path().join("cloudflared.pid")
    }

    fn args_file(&self) -> PathBuf {
        self.dir.path().join("cloudflared.args")
    }

    /// Install a fake `cloudflared`: it records its pid and arguments, runs `body`, and the
    /// last command replaces the shell (so the recorded pid is the long-running process).
    fn cloudflared(&self, body: &str) {
        let p = self.bin().join("cloudflared");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\necho $$ > '{}'\necho \"$@\" > '{}'\n{body}\n",
                self.pid_file().display(),
                self.args_file().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn another(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("ANOTHER_HOME", self.home())
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut child = self
            .another(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if started.elapsed() > Duration::from_secs(60) {
                stop(&mut child);
                panic!(
                    "`another {args:?}` did not exit: {:?}",
                    child.wait_with_output()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn tunnel_pid(&self) -> i32 {
        std::fs::read_to_string(self.pid_file())
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

/// A test that fails after the fake cloudflared started must not leave it running: stop
/// the recorded pid (and only that pid) if it is still alive.
impl Drop for Fx {
    fn drop(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.pid_file()) else {
            return;
        };
        if let Ok(pid) = text.trim().parse::<i32>() {
            if alive(pid) {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }
}

/// A server the test started. Dropped while still running (a failed assertion), it is
/// stopped like a timeout: SIGTERM first, so its own cleanup stops the tunnel.
struct Running(Child);

impl std::ops::Deref for Running {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Running {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            stop(&mut self.0);
        }
    }
}

/// Stop a child this test started: SIGTERM, so it can clean up; SIGKILL only if it is still
/// running 5 s later.
fn stop(child: &mut Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn signal(pid: u32, sig: &str) {
    let ok = Command::new("kill")
        .args([&format!("-{sig}"), &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -{sig} {pid}");
}

/// Wait until `child` exits, at most `limit`; returns its exit status.
fn wait_for(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > limit {
            stop(child);
            panic!("did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lines of the child's stderr, read on a thread.
fn stderr_lines(child: &mut Child) -> mpsc::Receiver<String> {
    let err = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Read stderr until a line contains `needle`; returns everything read.
fn until(rx: &mpsc::Receiver<String>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = String::new();
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        seen.push_str(&line);
        seen.push('\n');
        if line.contains(needle) {
            return seen;
        }
    }
    panic!("no line with {needle:?} in:\n{seen}");
}

/// A GET with the given Host header; returns status line and body.
fn get(port: u16, host: &str, path: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("connect: {e}"),
        }
    };
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let status = text.lines().next().unwrap_or_default().to_string();
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, body)
}

/// The quick-tunnel URL cloudflared prints becomes the public origin, and stopping the
/// server, by SIGINT (Ctrl-C) or SIGTERM (a service manager), stops cloudflared too.
#[test]
fn a_quick_tunnel_serves_its_url_and_stops_with_the_server() {
    for sig in ["INT", "TERM"] {
        let fx = Fx::new();
        // cloudflared logs its URL on stderr inside a box, then keeps running.
        fx.cloudflared(&format!(
            "echo 'INF Requesting new quick Tunnel on trycloudflare.com...' >&2\n\
             echo 'INF |  https://{HOST}  |' >&2\n\
             exec sleep 60"
        ));
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let mut child = Running(
            fx.another(&["remote", "up", "--bind", &bind])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let rx = stderr_lines(&mut child);
        let text = until(&rx, "quick tunnel");
        assert!(
            text.contains(&format!("point the tunnel at http://{bind}")),
            "{text}"
        );
        assert!(text.contains(&format!("https://{HOST}/mcp")), "{text}");

        // cloudflared was asked to forward to the bind address, and it is running.
        let args = std::fs::read_to_string(fx.args_file()).unwrap();
        assert_eq!(
            args.trim(),
            format!("tunnel --url http://{bind} --no-autoupdate")
        );
        let tunnel = fx.tunnel_pid();
        assert!(alive(tunnel));

        // The server answers for the tunnel's host, with the tunnel's URL as its resource.
        let (status, body) = get(port, HOST, "/.well-known/oauth-protected-resource/mcp");
        assert!(status.contains("200"), "{status} {body}");
        assert!(body.contains(&format!("https://{HOST}/mcp")), "{body}");

        signal(child.id(), sig);
        let status = wait_for(&mut child, Duration::from_secs(30));
        assert!(status.success(), "SIG{sig}: {status:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(tunnel) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !alive(tunnel),
            "SIG{sig}: cloudflared {tunnel} outlived the server"
        );
    }
}

/// With --public-url, no cloudflared is started (none is even installed here), and the
/// given origin is the server's.
#[test]
fn a_public_url_starts_no_tunnel() {
    let fx = Fx::new();
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = Running(
        fx.another(&[
            "remote",
            "up",
            "--bind",
            &bind,
            "--public-url",
            "https://another.example.com",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let rx = stderr_lines(&mut child);
    let text = until(&rx, "https://another.example.com/mcp");
    assert!(!text.contains("quick tunnel"), "{text}");
    let (status, body) = get(
        port,
        "another.example.com",
        "/.well-known/oauth-protected-resource/mcp",
    );
    assert!(status.contains("200"), "{status} {body}");
    assert!(body.contains("https://another.example.com/mcp"), "{body}");
    // Another host is refused: the server answers only for its public origin.
    let (status, _) = get(
        port,
        "evil.example.com",
        "/.well-known/oauth-protected-resource/mcp",
    );
    assert!(!status.contains("200"), "{status}");
    assert!(!fx.pid_file().exists());
    signal(child.id(), "INT");
    assert!(wait_for(&mut child, Duration::from_secs(30)).success());
}

/// Each way of failing to start exits 1 with a message that says what to do, and leaves no
/// cloudflared running.
#[test]
fn failures_to_start_say_what_to_do() {
    let fx = Fx::new();
    let text = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();

    let out = fx.run(&["remote", "up", "--bind", "127.0.0.1:0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("needs a fixed bind port"),
        "{}",
        text(&out)
    );

    let out = fx.run(&["remote", "up", "--bind", "http://127.0.0.1:7467"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("--bind is host:port, not a URL"),
        "{}",
        text(&out)
    );

    // No cloudflared on PATH: the named-tunnel instructions instead.
    let port = free_port().to_string();
    let bind = format!("127.0.0.1:{port}");
    let out = fx.run(&["remote", "up", "--bind", &bind]);
    assert_eq!(out.status.code(), Some(1));
    let t = text(&out);
    assert!(t.contains("cloudflared is not installed"), "{t}");
    assert!(t.contains("another remote up --public-url"), "{t}");

    // cloudflared that exits without a URL: its output is shown, and the named tunnel.
    fx.cloudflared("echo 'ERR failed to request quick Tunnel: 429 Too Many Requests' >&2\nexit 1");
    let out = fx.run(&["remote", "up", "--bind", &bind]);
    assert_eq!(out.status.code(), Some(1));
    let t = text(&out);
    assert!(
        t.contains("cloudflared exited before printing a URL"),
        "{t}"
    );
    assert!(t.contains("429 Too Many Requests"), "{t}");
    assert!(t.contains("Named Cloudflare Tunnel"), "{t}");
    let pid = fx.tunnel_pid();
    assert!(!alive(pid));

    // A URL that is not a quick tunnel's is not taken for one.
    fx.cloudflared(
        "echo 'INF see https://api.trycloudflare.com and https://example.com' >&2\nexit 1",
    );
    let out = fx.run(&["remote", "up", "--bind", &bind]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("exited before printing a URL"),
        "{}",
        text(&out)
    );
}

/// The server failing to start (the port is taken) still stops the tunnel it started.
#[test]
fn a_server_that_cannot_bind_stops_its_tunnel() {
    let fx = Fx::new();
    fx.cloudflared(&format!(
        "echo 'INF |  https://{HOST}  |' >&2\nexec sleep 60"
    ));
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = taken.local_addr().unwrap().to_string();
    let out = fx.run(&["remote", "up", "--bind", &bind]);
    assert!(!out.status.success(), "{out:?}");
    let tunnel = fx.tunnel_pid();
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(tunnel) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(tunnel),
        "cloudflared {tunnel} outlived a server that never started"
    );
    drop(taken);
}
