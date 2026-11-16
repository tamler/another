//! Evidence for the low-level edit primitive, not an admission or client test.
use std::fs::{self, OpenOptions};

use another::handle_edit::{path_names_open_file, replace_open_file, EditError, MAX_EDIT_BYTES};

#[test]
fn an_edit_uses_the_open_file_after_the_path_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let moved = dir.path().join("moved");
    fs::write(&path, b"before").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(path_names_open_file(&file, &path).unwrap());
    fs::rename(&path, &moved).unwrap();
    fs::write(&path, b"other").unwrap();

    assert!(!path_names_open_file(&file, &path).unwrap());

    replace_open_file(&mut file, b"before", b"after").unwrap();

    assert_eq!(fs::read(&moved).unwrap(), b"after");
    assert_eq!(fs::read(&path).unwrap(), b"other");
}

#[test]
fn a_changed_preimage_is_refused_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    fs::write(&path, b"changed").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(replace_open_file(&mut file, b"expected", b"new").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"changed");
}

#[test]
fn oversized_replacement_is_refused_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    fs::write(&path, b"before").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let replacement = vec![b'x'; MAX_EDIT_BYTES + 1];
    assert!(replace_open_file(&mut file, b"before", &replacement).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"before");
}

#[test]
fn a_hard_link_created_after_open_is_refused_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let alias = dir.path().join("alias");
    fs::write(&path, b"before").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs::hard_link(&path, &alias).unwrap();
    assert!(!path_names_open_file(&file, &path).unwrap());
    assert!(replace_open_file(&mut file, b"before", b"after").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"before");
    assert_eq!(fs::read(&alias).unwrap(), b"before");
}

#[test]
fn write_failure_is_reported_as_unknown_not_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    fs::write(&path, b"before").unwrap();
    let mut read_only = fs::File::open(&path).unwrap();
    assert!(matches!(
        replace_open_file(&mut read_only, b"before", b"after"),
        Err(EditError::OutcomeUnknown(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), b"before");
}

#[test]
fn symlink_is_not_treated_as_the_open_file_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let link = dir.path().join("link");
    fs::write(&path, b"before").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&path, &link).unwrap();
    let file = fs::File::open(&path).unwrap();
    assert!(!path_names_open_file(&file, &link).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"before");
}
