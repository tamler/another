//! `scripts/install.sh` against a fake release served from `file://`: it installs a verified
//! archive, and refuses a tampered one or a missing checksum line. No network.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

fn script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

/// A release directory with one archive (a stub `another`) and SHA256SUMS.
fn release(dir: &Path, version: &str, target: &str, tamper: bool) {
    let name = format!("another-{version}-{target}");
    let pkg = dir.join("pkg").join(&name);
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("another"),
        "#!/bin/sh\necho \"another 0.0.0-test\"\n",
    )
    .unwrap();
    let rel = dir.join("rel");
    std::fs::create_dir_all(&rel).unwrap();
    let archive = format!("{name}.tar.gz");
    let ok = Command::new("tar")
        .args(["-czf"])
        .arg(rel.join(&archive))
        .arg("-C")
        .arg(dir.join("pkg"))
        .arg(&name)
        .status()
        .unwrap();
    assert!(ok.success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd '{}' && (sha256sum '{archive}' 2>/dev/null || shasum -a 256 '{archive}')",
            rel.display()
        ))
        .output()
        .unwrap();
    let mut line = String::from_utf8_lossy(&sum.stdout).to_string();
    if tamper {
        line = format!("{}{}", "0".repeat(64), &line[64..]);
    }
    std::fs::write(rel.join("SHA256SUMS"), line).unwrap();
}

fn install(dir: &Path, version: &str) -> std::process::Output {
    Command::new("sh")
        .arg(script())
        .env("ANOTHER_VERSION", version)
        .env(
            "ANOTHER_DOWNLOAD_BASE",
            format!("file://{}", dir.join("rel").display()),
        )
        .env("ANOTHER_INSTALL_DIR", dir.join("bin"))
        .env("HOME", dir)
        .output()
        .unwrap()
}

#[test]
fn installs_verified_archives_only() {
    let Some(target) = target() else { return };
    if !have("curl") || !(have("sha256sum") || have("shasum")) {
        eprintln!("skipping: needs curl and sha256sum/shasum");
        return;
    }
    let good = tempfile::tempdir().unwrap();
    release(good.path(), "0.0.0-test", target, false);
    let out = install(good.path(), "v0.0.0-test");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let bin = good.path().join("bin/another");
    let v = Command::new(&bin).output().unwrap();
    assert!(String::from_utf8_lossy(&v.stdout).contains("0.0.0-test"));
    assert!(err.contains("another init && another doctor"), "{err}");

    // A checksum that doesn't match: refused, nothing installed.
    let bad = tempfile::tempdir().unwrap();
    release(bad.path(), "0.0.0-test", target, true);
    let out = install(bad.path(), "0.0.0-test");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert!(!bad.path().join("bin/another").exists());

    // An archive that isn't listed in SHA256SUMS: refused too.
    std::fs::write(good.path().join("rel/SHA256SUMS"), "").unwrap();
    std::fs::remove_file(&bin).unwrap();
    let out = install(good.path(), "0.0.0-test");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not listed"));
    assert!(!bin.exists());
}

#[test]
fn the_install_script_parses() {
    let ok = Command::new("sh").arg("-n").arg(script()).status().unwrap();
    assert!(ok.success());
}

/// `curl | sh` runs whatever arrived: a download cut off at any line must run nothing (no
/// output, nothing written), because the script is one function called on its last line.
#[test]
fn a_truncated_download_runs_nothing() {
    use std::io::Write;
    let full = std::fs::read_to_string(script()).unwrap();
    let lines: Vec<&str> = full.lines().collect();
    let tmp = tempfile::tempdir().unwrap();
    for cut in 1..lines.len() {
        let partial = lines[..cut].join("\n") + "\n";
        let dir = tmp.path().join(format!("bin{cut}"));
        let mut child = Command::new("sh")
            .arg("-s")
            .env("ANOTHER_VERSION", "0.1.0")
            .env("ANOTHER_INSTALL_DIR", &dir)
            .env(
                "ANOTHER_DOWNLOAD_BASE",
                format!("file://{}", tmp.path().display()),
            )
            .env("HOME", tmp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(partial.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("another-install:"),
            "cut after line {cut} ran part of the script: {stderr}"
        );
        assert!(
            !dir.exists(),
            "cut after line {cut} created {}",
            dir.display()
        );
    }
}
