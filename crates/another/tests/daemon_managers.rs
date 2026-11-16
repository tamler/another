//! `another daemon uninstall` must tell confirmed removal apart from a manager it could not
//! reach (review round 1, P2-1). The systemd and Task Scheduler paths run here against fake
//! `systemctl` and `schtasks` programs put first on PATH, on any Unix host; nothing real is
//! loaded, stopped or deleted.
//!
//! This binary holds a single test, because it changes PATH for its own process and
//! integration-test binaries run as separate processes.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use another::daemon::{self, DaemonEnv, Manager};

const LABEL: &str = "one.toanother.another.fake-managers";

/// The fake managers read their behaviour from `mode` and log their arguments to `calls`.
const SYSTEMCTL: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
mode="$(cat "$dir/mode")"
printf '%s\n' "$*" >> "$dir/calls"
case "$mode:$2" in
  denied:*) echo 'Access denied: manager unavailable' >&2; exit 1 ;;
  absent:disable) echo 'Failed to disable unit: Unit file x.service does not exist.' >&2; exit 1 ;;
  absent:show) printf 'LoadState=not-found\nActiveState=inactive\nUnitFileState=\nMainPID=0\n' ;;
  stuck:disable) echo 'Access denied' >&2; exit 1 ;;
  stuck:show) printf 'LoadState=loaded\nActiveState=active\nUnitFileState=enabled\nMainPID=99\n' ;;
  stopped:show) printf 'LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\nMainPID=0\n' ;;
  reloadfail:daemon-reload) echo 'Access denied' >&2; exit 1 ;;
esac
exit 0
"#;

const SCHTASKS: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
mode="$(cat "$dir/mode")"
printf '%s\n' "$*" >> "$dir/calls"
label="$(cat "$dir/label")"
case "$mode:$1" in
  denied:*) echo 'ERROR: Access is denied.' >&2; exit 1 ;;
  absent:/End|absent:/Delete) echo 'ERROR: The system cannot find the file specified.' >&2; exit 1 ;;
  absent:/Query) printf '"\\Microsoft\\Other","N/A","Ready"\n' ;;
  stuck:/Delete) echo 'ERROR: Access is denied.' >&2; exit 1 ;;
  stuck:/Query) printf '"\\Microsoft\\Other","N/A","Ready"\n"\\%s","N/A","Running"\n' "$label" ;;
  ok:/Query) printf '"\\Microsoft\\Other","N/A","Ready"\n' ;;
esac
exit 0
"#;

struct Fakes {
    dir: PathBuf,
}

impl Fakes {
    fn new(root: &Path) -> Fakes {
        let dir = root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, script) in [("systemctl", SYSTEMCTL), ("schtasks", SCHTASKS)] {
            let path = dir.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("label"), LABEL).unwrap();
        Fakes { dir }
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.dir.join("mode"), mode).unwrap();
        let _ = std::fs::remove_file(self.dir.join("calls"));
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn env_for(root: &Path, manager: Manager) -> DaemonEnv {
    DaemonEnv {
        manager,
        home: root.join("home"),
        config_home: Some(root.join("config")),
        another_home: root.join("data"),
        exe: root.join("bin-real").join("another"),
        path: Some("/usr/bin:/bin".into()),
        user: Some("HOST\\tester".into()),
        label: LABEL.into(),
        bind: "127.0.0.1:7499".into(),
    }
}

/// Write the definition without loading it, then uninstall in `mode`.
fn uninstall_in(
    fakes: &Fakes,
    env: &DaemonEnv,
    mode: &str,
) -> (anyhow::Result<daemon::DaemonReport>, bool, Vec<String>) {
    fakes.mode("ok");
    daemon::install(env, false, false).unwrap();
    assert!(env.file_path().exists());
    fakes.mode(mode);
    let result = daemon::uninstall(env, false);
    (result, env.file_path().exists(), fakes.calls())
}

#[test]
fn uninstall_removes_the_definition_only_when_removal_is_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let fakes = Fakes::new(dir.path());
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![fakes.dir.clone()];
    dirs.extend(std::env::split_paths(&old_path));
    // Only this test runs in this process (see the module comment).
    std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());

    for manager in [Manager::Systemd, Manager::TaskScheduler] {
        let env = env_for(dir.path(), manager);

        // The manager refuses everything: nothing is proved, so the definition stays and
        // the error says what to do.
        let (result, kept, _) = uninstall_in(&fakes, &env, "denied");
        let err = format!("{:#}", result.expect_err("denied must be an error"));
        assert!(
            kept,
            "{manager:?}: the definition was deleted on a manager failure"
        );
        assert!(err.contains("Access"), "{manager:?}: {err}");
        assert!(
            err.contains(&env.file_path().display().to_string()),
            "{manager:?}: the error names the kept definition: {err}"
        );

        // The service is still there and could not be stopped or deleted.
        let (result, kept, _) = uninstall_in(&fakes, &env, "stuck");
        assert!(
            result.is_err(),
            "{manager:?}: a service that is still present"
        );
        assert!(kept, "{manager:?}");

        // The manager answers and confirms the service is not there: removing the
        // definition is right, and so is repeating the uninstall.
        let (result, kept, _) = uninstall_in(&fakes, &env, "absent");
        let report = result.unwrap_or_else(|e| panic!("{manager:?}: absent: {e:#}"));
        assert_eq!(report.change, "delete");
        assert!(!kept, "{manager:?}");
        fakes.mode("absent");
        let again = daemon::uninstall(&env, false).unwrap();
        assert_eq!(
            again.change, "unchanged",
            "{manager:?}: uninstall is idempotent"
        );

        // The normal case: the manager stops and removes it.
        let (result, kept, calls) = uninstall_in(&fakes, &env, "ok");
        assert_eq!(result.unwrap().change, "delete", "{manager:?}");
        assert!(!kept, "{manager:?}");
        match manager {
            Manager::Systemd => assert_eq!(
                calls,
                [
                    format!("--user disable --now {LABEL}.service"),
                    "--user daemon-reload".to_string(),
                ]
            ),
            _ => assert!(
                calls.contains(&format!("/Delete /TN {LABEL} /F")),
                "{calls:?}"
            ),
        }
    }

    // systemd: disabling fails, but the manager reports the unit stopped and not enabled.
    let env = env_for(dir.path(), Manager::Systemd);
    let (result, kept, _) = uninstall_in(&fakes, &env, "stopped");
    assert!(result.is_ok(), "{result:?}");
    assert!(!kept);

    // systemd: stopped and removed, but the manager could not reload: report it.
    let (result, kept, _) = uninstall_in(&fakes, &env, "reloadfail");
    let err = format!("{:#}", result.expect_err("a failed reload is reported"));
    assert!(!kept, "the unit was stopped, so the file is gone");
    assert!(err.contains("daemon-reload"), "{err}");

    // Task Scheduler status: a manager that cannot be queried is not "not loaded" by proof,
    // but status still answers, with loaded false.
    let env = env_for(dir.path(), Manager::TaskScheduler);
    fakes.mode("stuck");
    assert!(daemon::status(&env).unwrap().loaded);
    fakes.mode("absent");
    assert!(!daemon::status(&env).unwrap().loaded);

    std::env::set_var("PATH", old_path);
}
