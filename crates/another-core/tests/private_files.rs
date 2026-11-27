#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn files_are_private_under_umask_022() {
    let output = Command::new("sh")
        .args(["-c", "umask 022; exec \"$@\"", "sh"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "private_files_child", "--nocapture"])
        .env("ANOTHER_PRIVATE_FILES_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn private_files_child() {
    if std::env::var_os("ANOTHER_PRIVATE_FILES_CHILD").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = another_core::Store::open(&home).unwrap();
    for name in [
        "another.db",
        "another.db-wal",
        "another.db-shm",
        "build.json",
    ] {
        let mode = std::fs::metadata(home.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    drop(store);
    let _reopened = another_core::Store::open(&home).unwrap();
    for name in [
        "another.db",
        "another.db-wal",
        "another.db-shm",
        "build.json",
    ] {
        let mode = std::fs::metadata(home.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "reopened {name}");
    }
}

#[test]
fn normal_open_tightens_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let _store = another_core::Store::open(&home).unwrap();
    let names = [
        "another.db",
        "another.db-wal",
        "another.db-shm",
        "build.json",
    ];
    for name in names {
        std::fs::set_permissions(home.join(name), std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let _second = another_core::Store::open(&home).unwrap();
    for name in names {
        let mode = std::fs::metadata(home.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "existing {name}");
    }
}
