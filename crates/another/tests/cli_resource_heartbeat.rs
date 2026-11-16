//! `another resource heartbeat`: resources.rs runs it once but never looks at what it did.
//! It must push the caller's own leases forward and no one else's, and a target the caller
//! holds no lease on fails the call (exit 1) without renewing the rest.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: PathBuf,
}

impl Fx {
    /// Alice owns dev_server vite, bob owns port tcp:9, both with 60 s leases.
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shop = dir.path().join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let fx = Fx {
            _dir: dir,
            home,
            shop,
        };
        fx.ok(&["init"]);
        for name in ["alice", "bob"] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                "codex",
                "--project",
                ".",
            ]);
        }
        fx.ok(&[
            "resource",
            "register",
            "dev_server",
            "vite",
            "-a",
            "alice",
            "--ttl",
            "60",
        ]);
        fx.ok(&[
            "resource", "register", "port", "tcp:9", "-a", "bob", "--ttl", "60",
        ]);
        fx
    }

    /// Exit status, stdout and stderr.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .current_dir(&self.shop)
            .env("ANOTHER_HOME", &self.home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {err}");
        out
    }

    /// The expiry, in ms, of the lease `agent` holds on the resource called `name`.
    fn expiry(&self, name: &str, agent: &str) -> i64 {
        let all: Value = serde_json::from_str(&self.ok(&["--json", "resources"])).unwrap();
        let res = all
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no resource {name}: {all}"));
        let lease = res["leases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["agent"] == agent)
            .unwrap_or_else(|| panic!("no lease for {agent}: {res}"));
        another_core::parse_ts(lease["expires_at"].as_str().unwrap()).unwrap()
    }
}

fn pause() {
    std::thread::sleep(std::time::Duration::from_millis(1_100));
}

#[test]
fn a_heartbeat_renews_only_the_callers_leases() {
    let fx = Fx::new();
    let (alice0, bob0) = (
        fx.expiry("vite", "alice@shop"),
        fx.expiry("tcp:9", "bob@shop"),
    );
    pause();
    let beat: Value =
        serde_json::from_str(&fx.ok(&["--json", "resource", "heartbeat", "-a", "alice"])).unwrap();
    let names: Vec<&str> = beat
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["vite"], "{beat}");
    assert!(
        fx.expiry("vite", "alice@shop") >= alice0 + 1_000,
        "alice's lease was not renewed"
    );
    assert_eq!(fx.expiry("tcp:9", "bob@shop"), bob0, "bob's lease moved");

    // Naming a resource renews it, in text too.
    let alice1 = fx.expiry("vite", "alice@shop");
    pause();
    let text = fx.ok(&["resource", "heartbeat", "-a", "alice", "dev_server:vite"]);
    assert!(
        text.contains("vite") && text.contains("alice@shop"),
        "{text}"
    );
    assert!(fx.expiry("vite", "alice@shop") >= alice1 + 1_000);
}

#[test]
fn a_target_without_a_lease_fails_and_renews_nothing() {
    let fx = Fx::new();
    let alice0 = fx.expiry("vite", "alice@shop");
    pause();
    let (code, _, err) = fx.run(&[
        "resource",
        "heartbeat",
        "-a",
        "alice",
        "dev_server:vite",
        "port:tcp:9",
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("alice@shop has no live lease"), "{err}");
    assert_eq!(
        fx.expiry("vite", "alice@shop"),
        alice0,
        "a failed heartbeat renewed alice's own lease"
    );

    let (code, out, _) = fx.run(&[
        "--json",
        "resource",
        "heartbeat",
        "-a",
        "alice",
        "port:tcp:9",
    ]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["error"]["code"], "not_found", "{v}");

    let (code, _, err) = fx.run(&["resource", "heartbeat", "-a", "alice", "dev_server:nope"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("no registered resource \"dev_server:nope\""),
        "{err}"
    );
}
