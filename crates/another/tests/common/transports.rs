//! The example `transports.json` of docs/WAKE.md, so the tests run the config the owner is
//! told to copy, and a way to put a config into a test home.
#![allow(dead_code)]

use std::path::Path;

use another::transport::Config;
use another_core::WakeVia;

/// The first `json` block of the "Command transports" section of docs/WAKE.md: `tmux`,
/// `cmux` and `cmux-notify`.
pub fn example() -> String {
    let doc = include_str!("../../../../docs/WAKE.md");
    let section = doc
        .split("## Command transports")
        .nth(1)
        .expect("docs/WAKE.md has a Command transports section");
    let block = section
        .split("```json")
        .nth(1)
        .expect("the section has a json example");
    block.split("```").next().unwrap().to_string()
}

/// The example as a config, for in-process wakes on a store with no home.
pub fn example_config() -> Config {
    let config = Config::from_json(&example());
    assert_eq!(config.error, None);
    config
}

pub fn via(name: &str) -> WakeVia {
    name.parse().unwrap()
}

/// Write `text` as the home's `transports.json`, owned by this user and writable by
/// nobody else, as the load checks demand.
pub fn write(home: &Path, text: &str) {
    std::fs::create_dir_all(home).unwrap();
    let path = home.join("transports.json");
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

pub fn write_example(home: &Path) {
    write(home, &example());
}
