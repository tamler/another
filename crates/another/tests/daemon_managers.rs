//! `another daemon uninstall` must tell a confirmed stop and removal apart from a manager
//! it could not reach or that refused (review rounds 1 and 2). The systemd and Task
//! Scheduler paths run here against fake `systemctl` and `schtasks` programs put first on
//! PATH, on any Unix host; nothing real is loaded, stopped or deleted.
//!
//! This binary holds a single test, because it changes PATH for its own process and
//! integration-test binaries run as separate processes.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use another::daemon::{self, DaemonEnv, Manager};

const LABEL: &str = "one.toanother.another.fake-managers";

/// The fake systemd reads its behaviour from `mode` and logs its arguments to `calls`.
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
  stopped:disable) echo 'Access denied' >&2; exit 1 ;;
  stopped:show) printf 'LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\nMainPID=0\n' ;;
  reloadfail:daemon-reload) echo 'Access denied' >&2; exit 1 ;;
esac
exit 0
"#;

/// The fake Task Scheduler keeps state in marker files: `registered` and `running`. As
/// documented for the real one, `/Delete` removes the registration but leaves a running
/// program running. `mode` can deny everything, or only `/End`, `/Delete` or `/Query`.
/// `/Query /TN <label> /V /FO CSV /NH` reports the last result 267009 (task running)
/// while `running` exists; `/Query /FO CSV /NH` lists the registered tasks.
const SCHTASKS: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
mode="$(cat "$dir/mode")"
printf '%s\n' "$*" >> "$dir/calls"
label="$(cat "$dir/label")"
deny() { echo 'ERROR: Access is denied.' >&2; exit 1; }
missing() { echo 'ERROR: The system cannot find the file specified.' >&2; exit 1; }
[ "$mode" = denied ] && deny
case "$1" in
  /End)
    [ "$mode" = end_denied ] && deny
    [ -f "$dir/registered" ] || missing
    if [ -f "$dir/running" ]; then rm -f "$dir/running"; echo 'SUCCESS: terminated.'; exit 0; fi
    echo 'ERROR: The task is not running.' >&2; exit 1 ;;
  /Delete)
    [ "$mode" = delete_denied ] && deny
    [ -f "$dir/registered" ] || missing
    rm -f "$dir/registered"; echo 'SUCCESS: deleted.'; exit 0 ;;
  /Query)
    [ "$mode" = query_denied ] && deny
    if [ "$2" = /TN ]; then
      [ -f "$dir/registered" ] || missing
      result=0
      [ -f "$dir/running" ] && result=267009
      printf '"HOST","\\%s","N/A","Ready","Interactive only","1/1/2026 9:00:00 AM","%s","tester"\n' "$label" "$result"
      exit 0
    fi
    printf '"\\Microsoft\\Other","N/A","Ready"\n'
    [ -f "$dir/registered" ] && printf '"\\%s","N/A","Ready"\n' "$label"
    exit 0 ;;
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

    /// Set the fake Task Scheduler's `registered` and `running` markers.
    fn task(&self, registered: bool, running: bool) {
        for (name, on) in [("registered", registered), ("running", running)] {
            let path = self.dir.join(name);
            if on {
                std::fs::write(path, "").unwrap();
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn is(&self, marker: &str) -> bool {
        self.dir.join(marker).exists()
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

/// A Task Scheduler uninstall from the given task state, in `mode`.
fn task_uninstall(
    fakes: &Fakes,
    env: &DaemonEnv,
    registered: bool,
    running: bool,
    mode: &str,
) -> (anyhow::Result<daemon::DaemonReport>, bool, Vec<String>) {
    fakes.task(registered, running);
    uninstall_in(fakes, env, mode)
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

    systemd_cases(&fakes, &env_for(dir.path(), Manager::Systemd));
    task_scheduler_cases(&fakes, &env_for(dir.path(), Manager::TaskScheduler));

    std::env::set_var("PATH", old_path);
}

fn systemd_cases(fakes: &Fakes, env: &DaemonEnv) {
    // The manager refuses everything: nothing is proved, so the definition stays and the
    // error says what to do.
    let (result, kept, _) = uninstall_in(fakes, env, "denied");
    let err = format!("{:#}", result.expect_err("denied must be an error"));
    assert!(kept, "the definition was deleted on a manager failure");
    assert!(err.contains("Access"), "{err}");
    assert!(
        err.contains(&env.file_path().display().to_string()),
        "the error names the kept definition: {err}"
    );

    // Still running and enabled, and disabling failed.
    let (result, kept, _) = uninstall_in(fakes, env, "stuck");
    assert!(result.is_err());
    assert!(kept);

    // Confirmed absent: removal is right, and repeating it is too.
    let (result, kept, _) = uninstall_in(fakes, env, "absent");
    assert_eq!(result.unwrap().change, "delete");
    assert!(!kept);
    fakes.mode("absent");
    assert_eq!(daemon::uninstall(env, false).unwrap().change, "unchanged");

    // Disabling fails, but the manager reports the unit stopped and not enabled.
    let (result, kept, calls) = uninstall_in(fakes, env, "stopped");
    assert!(result.is_ok(), "{result:?}");
    assert!(!kept);
    assert!(
        calls.iter().any(|c| c.starts_with("--user show")),
        "the failed disable was followed by a check: {calls:?}"
    );

    // Stopped and removed, but the manager could not reload: report it.
    let (result, kept, _) = uninstall_in(fakes, env, "reloadfail");
    let err = format!("{:#}", result.expect_err("a failed reload is reported"));
    assert!(!kept, "the unit was stopped, so the file is gone");
    assert!(err.contains("daemon-reload"), "{err}");

    // The normal case.
    let (result, kept, calls) = uninstall_in(fakes, env, "ok");
    assert_eq!(result.unwrap().change, "delete");
    assert!(!kept);
    assert_eq!(
        calls,
        [
            format!("--user disable --now {LABEL}.service"),
            "--user daemon-reload".to_string(),
        ]
    );
}

fn task_scheduler_cases(fakes: &Fakes, env: &DaemonEnv) {
    let deleted = |calls: &[String]| calls.iter().any(|c| c.starts_with("/Delete"));

    // Review round 2: /End is refused while the task runs, and /Delete would succeed.
    // Deleting would leave the server running with no task, so nothing is deleted.
    let (result, kept, calls) = task_uninstall(fakes, env, true, true, "end_denied");
    let err = format!("{:#}", result.expect_err("an unstopped task is an error"));
    assert!(kept, "the definition stays for a retry");
    assert!(!deleted(&calls), "/Delete must not run: {calls:?}");
    assert!(fakes.is("registered") && fakes.is("running"));
    assert!(err.contains("still running"), "{err}");
    assert!(err.contains("Access is denied"), "{err}");

    // The manager refuses everything.
    let (result, kept, calls) = task_uninstall(fakes, env, true, true, "denied");
    let err = format!("{:#}", result.expect_err("denied must be an error"));
    assert!(kept);
    assert!(!deleted(&calls), "{calls:?}");
    assert!(
        err.contains(&env.file_path().display().to_string()),
        "{err}"
    );

    // The task's state cannot be read, though /End and /Delete would work.
    let (result, kept, calls) = task_uninstall(fakes, env, true, true, "query_denied");
    assert!(result.is_err());
    assert!(kept);
    assert!(!deleted(&calls), "{calls:?}");
    assert!(
        fakes.is("running"),
        "nothing was stopped on an unknown state"
    );

    // Stopped, but the delete is refused and the task is still listed.
    let (result, kept, _) = task_uninstall(fakes, env, true, true, "delete_denied");
    assert!(result.is_err());
    assert!(kept);
    assert!(fakes.is("registered"));
    assert!(
        !fakes.is("running"),
        "it was stopped before the delete was tried"
    );

    // Already stopped: /End fails ("not running"), the state says stopped, so it is
    // deleted.
    let (result, kept, calls) = task_uninstall(fakes, env, true, false, "ok");
    assert_eq!(result.unwrap().change, "delete");
    assert!(!kept);
    assert!(deleted(&calls), "{calls:?}");
    assert!(!fakes.is("registered"));

    // The normal case: running, stopped by /End, then deleted.
    let (result, kept, calls) = task_uninstall(fakes, env, true, true, "ok");
    assert_eq!(result.unwrap().change, "delete");
    assert!(!kept);
    assert!(!fakes.is("registered") && !fakes.is("running"));
    let end = calls.iter().position(|c| c.starts_with("/End"));
    let delete = calls.iter().position(|c| c.starts_with("/Delete"));
    assert!(end.is_some() && end < delete, "{calls:?}");

    // Never registered: confirmed absent, nothing to stop or delete; repeating is fine.
    let (result, kept, calls) = task_uninstall(fakes, env, false, false, "ok");
    assert_eq!(result.unwrap().change, "delete");
    assert!(!kept);
    assert!(!deleted(&calls), "{calls:?}");
    fakes.mode("ok");
    assert_eq!(daemon::uninstall(env, false).unwrap().change, "unchanged");

    // Status reads the task list.
    fakes.task(true, false);
    fakes.mode("ok");
    assert!(daemon::status(env).unwrap().loaded);
    fakes.task(false, false);
    assert!(!daemon::status(env).unwrap().loaded);

    // The task shows stopped, but something still answers /health on the daemon's
    // address: on a hosted runner `/End` returned while the old server kept serving. Not
    // proved stopped, so nothing is deleted.
    let (serving_env, _server) = with_health_server(env);
    let (result, kept, calls) = task_uninstall(fakes, &serving_env, true, true, "ok");
    let err = format!(
        "{:#}",
        result.expect_err("a server still answering is an error")
    );
    assert!(kept);
    assert!(!deleted(&calls), "{calls:?}");
    assert!(err.contains("still answers /health"), "{err}");

    // Restart waits for the old server to go before `/Run`, and refuses while it answers.
    fakes.task(true, true);
    fakes.mode("ok");
    let err = format!(
        "{:#}",
        daemon::restart(&serving_env).expect_err("old server still answering")
    );
    assert!(err.contains("did not stop"), "{err}");
    assert!(
        !fakes.calls().iter().any(|c| c.starts_with("/Run")),
        "{:?}",
        fakes.calls()
    );
    // Restart reads the address from the installed definition: put back the plain one.
    fakes.mode("ok");
    daemon::install(env, false, false).unwrap();
    fakes.task(true, true);
    fakes.mode("ok");
    daemon::restart(env).unwrap();
    let calls = fakes.calls();
    let end = calls.iter().position(|c| c.starts_with("/End"));
    let run = calls.iter().position(|c| c.starts_with("/Run"));
    assert!(end.is_some() && end < run, "{calls:?}");
}

/// A copy of `env` whose address has a tiny server answering `GET /health` with 200,
/// standing in for a daemon that outlived its task. The server lives as long as the
/// returned listener's thread, i.e. the rest of the test process.
fn with_health_server(env: &DaemonEnv) -> (DaemonEnv, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    let mut env = env.clone();
    env.bind = format!("127.0.0.1:{port}");
    (env, server)
}
