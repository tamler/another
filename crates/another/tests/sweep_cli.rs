//! `another sweep` through the real binary: it expires lapsed holds and resource leases and
//! marks overdue requests stale, and both its text and JSON say how many of each.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .current_dir(cwd)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env("ANOTHER_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "another {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// One lapsed hold and one lapsed resource lease. The text names both counts, as the JSON
/// does; expired leases used to be left out of the text.
#[test]
fn sweep_reports_expired_holds_and_leases_in_text_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    another(&home, &shop, &["init"]);
    another(
        &home,
        &shop,
        &[
            "agent",
            "register",
            "a",
            "--program",
            "other",
            "--project",
            ".",
        ],
    );
    another(
        &home,
        &shop,
        &["hold", "acquire", "--agent", "a@shop", "src/x.rs"],
    );
    another(
        &home,
        &shop,
        &["resource", "register", "-a", "a@shop", "gpu", "box1"],
    );

    // Let both lapse without waiting out the minimum TTLs.
    let db = rusqlite::Connection::open(home.join(another_core::DB_FILE)).unwrap();
    db.execute("UPDATE holds SET expires_at = 1", []).unwrap();
    db.execute("UPDATE resource_leases SET expires_at = 1", [])
        .unwrap();
    drop(db);

    let text = String::from_utf8(another(&home, &shop, &["sweep"]).stdout).unwrap();
    assert!(text.contains("expired 1 hold(s)"), "{text}");
    assert!(text.contains("1 resource lease(s)"), "{text}");

    // A second sweep finds nothing left, and JSON carries all three counts.
    let json: Value =
        serde_json::from_slice(&another(&home, &shop, &["--json", "sweep"]).stdout).unwrap();
    assert_eq!(json["expired_holds"], 0, "{json}");
    assert_eq!(json["expired_leases"], 0, "{json}");
    assert_eq!(json["stale_requests"], 0, "{json}");
}
