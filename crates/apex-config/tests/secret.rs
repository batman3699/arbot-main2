//! §43: private keys and credentials never enter logs, metrics or telemetry.
//!
//! The repo already had to learn this narrowly -- `sim_rpc_url` carries a
//! provider credential and has a hand-written "keep this field out of logs"
//! comment. `Secret<T>` generalises that so it cannot be forgotten on the next
//! field.

use apex_config::Secret;

#[test]
fn debug_redacts() {
    let s = Secret::new("hunter2".to_string());
    let rendered = format!("{s:?}");
    assert!(!rendered.contains("hunter2"), "Debug leaked the secret: {rendered}");
    assert!(rendered.contains("REDACTED"));
}

#[test]
fn display_redacts() {
    let s = Secret::new("hunter2".to_string());
    assert!(!format!("{s}").contains("hunter2"));
}

#[test]
fn serialize_redacts() {
    // The dangerous path: a config struct derives Serialize for a diagnostic
    // dump and takes every secret with it.
    let s = Secret::new("hunter2".to_string());
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("hunter2"), "Serialize leaked the secret: {json}");
}

#[test]
fn the_value_is_still_reachable_deliberately() {
    // Redaction is worthless if it makes the type unusable; the point is that
    // reading it is an explicit call, not an accident.
    let s = Secret::new("hunter2".to_string());
    assert_eq!(s.expose(), "hunter2");
}

#[test]
fn a_url_with_an_embedded_key_redacts_whole() {
    // .env carries provider keys inline in URLs, e.g.
    // https://base.blockpi.network/v1/rpc/<key>. Redacting only a `key=` query
    // parameter would miss that shape entirely.
    let s = Secret::new("https://base.blockpi.network/v1/rpc/abc123def456".to_string());
    let rendered = format!("{s:?}");
    assert!(!rendered.contains("abc123def456"), "path-embedded key leaked: {rendered}");
}
