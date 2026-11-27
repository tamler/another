//! Exercise status with a fake systemd manager and real scratch loopback health servers.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;

use another::daemon::{self, DaemonEnv, Manager};

// One test owns this process's PATH, like daemon_managers.rs.
#[test]
fn status_attributes_health_to_the_current_database() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let manager = bin.join("systemctl");
    std::fs::write(&manager, "#!/bin/sh\nprintf 'LoadState=loaded\\nUnitFileState=enabled\\nActiveState=active\\nMainPID=4242\\n'\n").unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);
    let env = DaemonEnv {
        manager: Manager::Systemd,
        home: dir.path().join("user"),
        config_home: Some(dir.path().join("config")),
        another_home: dir.path().join("data"),
        exe: dir.path().join("another"),
        path: None,
        user: None,
        label: "one.toanother.health-fixture".into(),
        bind: "127.0.0.1:7466".into(),
    };
    let store = another_core::Store::open(&env.another_home).unwrap();
    let node = store.node_id().to_string();
    drop(store);
    let before = std::fs::read(env.another_home.join("another.db")).unwrap();
    std::fs::create_dir_all(env.file_path().parent().unwrap()).unwrap();
    for (body, expected) in [
        (format!(r#"{{"ok":true,"node_id":"{node}"}}"#), true),
        (r#"{"ok":true,"node_id":"other-node"}"#.into(), false),
        (r#"{"ok":true}"#.into(), false),
        ("not json".into(), false),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        std::fs::write(
            env.file_path(),
            format!("ExecStart=/work/another serve --bind {bind}\n"),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let status = daemon::status(&env).unwrap();
        server.join().unwrap();
        assert!(status.loaded);
        assert_eq!(status.healthy, expected);
        assert_eq!(status.detail.is_none(), expected);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["healthy"], expected);
        if !expected {
            assert!(status
                .detail
                .as_deref()
                .unwrap()
                .contains("did not identify this home's node"));
        }
        assert_eq!(
            std::fs::read(env.another_home.join("another.db")).unwrap(),
            before
        );
    }
    match old_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
}
