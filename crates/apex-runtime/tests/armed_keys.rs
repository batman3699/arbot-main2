//! Task 8.5 R36 — the armed senders' keys: made on the host, written 0400,
//! never overwritten, loaded only from a file no one else can read, and never
//! shown in an error.

use apex_runtime::armed::keys;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn secret_file(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("armed.keys");
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    path
}

#[test]
fn generated_keys_load_back_to_the_same_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("armed.keys");
    let made = keys::generate(3, &path).unwrap();
    assert_eq!(made.iter().collect::<BTreeSet<_>>().len(), 3, "three distinct keys");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o400);
    let loaded = keys::load(&path).unwrap();
    assert_eq!(loaded.iter().map(|k| k.address()).collect::<Vec<_>>(), made);
}

#[test]
fn a_file_others_can_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("armed.keys");
    keys::generate(1, &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440)).unwrap();
    let err = keys::load(&path).unwrap_err();
    assert!(err.contains("readable"), "{err}");
}

#[test]
fn an_existing_file_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("armed.keys");
    keys::generate(1, &path).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(keys::generate(2, &path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "the keys already made are kept");
}

#[test]
fn no_error_shows_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = secret_file(dir.path(), "0xnot-a-key-but-supersecret\n");
    let err = keys::load(&path).unwrap_err();
    assert!(!err.contains("supersecret"), "{err}");
    assert!(err.contains("line 1"), "{err}");
}

#[test]
fn an_empty_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert!(keys::load(&secret_file(dir.path(), "\n\n")).is_err());
}
