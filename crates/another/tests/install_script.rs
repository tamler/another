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

/// A PATH holding only what the script needs, with `checker` as the one checksum tool, and a
/// `tar` that logs each call to `tar.log` before running the real one.
fn tools_path(dir: &Path, checker: &str) -> Option<std::path::PathBuf> {
    let bin = dir.join("tools");
    std::fs::create_dir_all(&bin).unwrap();
    let find = |tool: &str| {
        let out = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && path.starts_with('/')).then_some(path)
    };
    for tool in [
        "curl", "uname", "mktemp", "rm", "grep", "cut", "wc", "mkdir", "cp", "chmod", "mv", "gzip",
        checker,
    ] {
        std::os::unix::fs::symlink(find(tool)?, bin.join(tool)).unwrap();
    }
    let tar = find("tar")?;
    let shim = bin.join("tar");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{tar}' \"$@\"\n",
            dir.join("tar.log").display()
        ),
    )
    .unwrap();
    let ok = Command::new("chmod")
        .arg("0755")
        .arg(&shim)
        .status()
        .unwrap();
    assert!(ok.success());
    Some(bin)
}

fn install_with(dir: &Path, version: &str, path: &Path) -> std::process::Output {
    // Absolute, since the child's PATH has no sh.
    Command::new("/bin/sh")
        .arg(script())
        .env("PATH", path)
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

/// Some `sha256sum -c` builds skip a line they can't parse (a leading space, say) and still
/// exit 0, so a SHA256SUMS line that names the archive but is malformed must be refused, with
/// either checker, before anything is unpacked.
#[test]
fn refuses_a_malformed_checksum_line_with_either_checker() {
    let Some(target) = target() else { return };
    let mut ran = 0;
    for checker in ["sha256sum", "shasum"] {
        let dir = tempfile::tempdir().unwrap();
        let Some(path) = have(checker)
            .then(|| tools_path(dir.path(), checker))
            .flatten()
        else {
            eprintln!("skipping {checker}: it or another needed tool is missing");
            continue;
        };
        ran += 1;
        let archive = format!("another-0.0.0-test-{target}.tar.gz");

        // The restricted PATH is enough to install a good release with this checker.
        release(dir.path(), "0.0.0-test", target, false);
        let out = install_with(dir.path(), "0.0.0-test", &path);
        assert!(
            out.status.success(),
            "{checker}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(dir.path().join("bin/another")).unwrap();
        std::fs::remove_file(dir.path().join("tar.log")).unwrap();

        for (what, sums) in [
            (
                "leading space, wrong digest",
                format!(" {}  {archive}\n", "0".repeat(64)),
            ),
            ("short digest", format!("{}  {archive}\n", "0".repeat(63))),
            ("no separator", format!("{}{archive}\n", "0".repeat(64))),
        ] {
            std::fs::write(dir.path().join("rel/SHA256SUMS"), sums).unwrap();
            let out = install_with(dir.path(), "0.0.0-test", &path);
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{checker}, {what}: installed: {err}");
            assert!(err.contains("not listed"), "{checker}, {what}: {err}");
            assert!(
                !dir.path().join("tar.log").exists(),
                "{checker}, {what}: unpacked before refusing"
            );
            assert!(
                !dir.path().join("bin/another").exists(),
                "{checker}, {what}"
            );
        }
    }
    if ran == 0 {
        eprintln!("skipping: needs sha256sum or shasum");
    }
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
