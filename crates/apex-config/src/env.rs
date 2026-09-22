//! Controlled environment access.
//!
//! Blueprint §2.4 forbids "late configuration lookup" on the dispatch path, and
//! this repository has 84 `ARBOT_*` variables read at call sites throughout the
//! hot path. The fix is not discipline, it is a chokepoint: config is resolved
//! once at boot through this type, which records every key it consulted so
//! "reads no environment" is a testable claim rather than an assertion.

use std::collections::BTreeSet;
use std::sync::Mutex;

/// Prefix for the only variables config is allowed to read after this migration.
/// Everything else becomes a typed field in `ops/inputs.yaml`.
pub const SECRET_PREFIX: &str = "APEX_SECRET_";

pub struct Env {
    secrets_only: bool,
    consulted: Mutex<BTreeSet<String>>,
}

impl Env {
    /// The production constructor: only `APEX_SECRET_*` may be read.
    pub fn secrets_only() -> Self {
        Self { secrets_only: true, consulted: Mutex::new(BTreeSet::new()) }
    }

    /// Escape hatch for the migration window, while legacy `${VAR}` placeholders
    /// in `ops/inputs.yaml` still reference non-secret names. Phase 17 removes it.
    pub fn permissive() -> Self {
        Self { secrets_only: false, consulted: Mutex::new(BTreeSet::new()) }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.consulted.lock().expect("env mutex").insert(key.to_string());
        if self.secrets_only && !key.starts_with(SECRET_PREFIX) {
            return None;
        }
        std::env::var(key).ok()
    }

    pub fn consulted(&self) -> Vec<String> {
        self.consulted.lock().expect("env mutex").iter().cloned().collect()
    }
}
