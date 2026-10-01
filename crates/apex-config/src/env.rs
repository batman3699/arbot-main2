//! Controlled environment access.
//!
//! Blueprint §2.4 forbids "late configuration lookup" on the dispatch path, and
//! this repository has 84 `ARBOT_*` variables read at call sites throughout the
//! hot path. The fix is not discipline, it is a chokepoint: config is resolved
//! once at boot through this type, which records every key it consulted so
//! "reads no environment" is a testable claim rather than an assertion.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Prefix for the only variables config is allowed to read after this migration.
/// Everything else becomes a typed field in `ops/inputs.yaml`.
pub const SECRET_PREFIX: &str = "APEX_SECRET_";

pub struct Env {
    secrets_only: bool,
    consulted: Mutex<BTreeSet<String>>,
    source: Source,
}

/// Where values come from. No `Debug`, here or on `Env`: a fixed set holds the
/// secrets themselves.
enum Source {
    Process,
    /// A test's own variables. Setting process variables from a test races
    /// every other test thread that reads the environment.
    Fixed(BTreeMap<String, String>),
}

impl Env {
    /// The production constructor: only `APEX_SECRET_*` may be read.
    pub fn secrets_only() -> Self {
        Self { secrets_only: true, consulted: Mutex::new(BTreeSet::new()), source: Source::Process }
    }

    /// `secrets_only`'s rule over a fixed set of variables instead of the
    /// process environment. For tests.
    pub fn fixed_secrets(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            secrets_only: true,
            consulted: Mutex::new(BTreeSet::new()),
            source: Source::Fixed(vars.into_iter().collect()),
        }
    }

    /// Escape hatch for the migration window, while legacy `${VAR}` placeholders
    /// in `ops/inputs.yaml` still reference non-secret names. Phase 17 removes it.
    pub fn permissive() -> Self {
        Self { secrets_only: false, consulted: Mutex::new(BTreeSet::new()), source: Source::Process }
    }

    /// Recover from a poisoned lock rather than propagating the panic.
    ///
    /// Poisoning means some other thread panicked while holding this mutex. The
    /// guarded value is a record of which keys were read -- losing exactness in
    /// that record is not worth turning one thread's panic into a second one.
    /// `main.rs` already carries a `lock_unpoison` helper for the same reason.
    fn consulted_guard(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.consulted.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.consulted_guard().insert(key.to_string());
        if self.secrets_only && !key.starts_with(SECRET_PREFIX) {
            return None;
        }
        match &self.source {
            Source::Process => std::env::var(key).ok(),
            Source::Fixed(vars) => vars.get(key).cloned(),
        }
    }

    pub fn consulted(&self) -> Vec<String> {
        self.consulted_guard().iter().cloned().collect()
    }
}
