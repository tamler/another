//! Locate this process's `another` binary when writing hook wrappers and daemon units.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Canonicalize `current_exe` when possible so installed hook and daemon
/// definitions don't point at a rustup or cargo proxy that later disappears.
pub fn resolve_current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the another binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_current_exe_returns_an_existing_path() {
        let path = resolve_current_exe().unwrap();
        assert!(path.is_absolute(), "{path:?}");
        assert!(path.exists(), "{path:?}");
    }
}
