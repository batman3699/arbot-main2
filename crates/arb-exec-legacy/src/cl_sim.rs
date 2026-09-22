//! The environment gates that decide HOW concentrated liquidity is priced.
//!
//! What this module used to be is now split in two: the pure maths moved to
//! `apex_math::cl_state`, the node reads moved to `apex_venues::cl_load`. What
//! is left is the part that belongs to neither — five process-global switches,
//! read at fifteen call sites across `venues`, `plan`, `sizing` and
//! `cl_parity_gate`.
//!
//! They stay here on purpose. Turning them into injected configuration is a
//! refactor of the live pricing path, not a file move, and it is the actual
//! subject of Task 2.6 (PLAN.md §33 Phase 2) — which is also where the §1.1.1
//! pricing-provenance confound gets closed, since `ARBOT_CL_MULTI_TICK` is one
//! of these five and is precisely the flag that reads as ON while every
//! fast-path edge carries no ladder.
//!
//! Gated by `ARBOT_LOCAL_CL_QUOTES=1` (default on). When disabled or state is
//! incomplete, callers fall back to on-chain quoter RPC.

// Re-exported so every `crate::cl_sim::...` path in this crate keeps resolving
// across the split. The names moved; the call sites did not have to.
pub use apex_math::cl_state::{quote_exact_input_single_tick, ClPoolState};
// `log_cl_quote_parity` is deliberately not re-exported: it has no callers
// and never did. It lives at `apex_venues::cl_load::log_cl_quote_parity`,
// where Task 2.2's three-way differential is expected to pick it up.
pub use apex_venues::cl_load::{load_cl_pool_state, load_cl_pool_states_batched};

/// Serialises tests that mutate `ARBOT_LOCAL_CL_QUOTES`. The env is process
/// global, so a test flipping it races any concurrent test that reads it.
/// Lives here (not in main.rs's test module) because `cl_sim` compiles into both
/// the lib and bin targets, and the lib cannot see bin-only items.
#[cfg(test)]
pub(crate) static CL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that restores `ARBOT_CL_MULTI_TICK` to its pre-guard value when
/// dropped — including when the drop happens during panic unwinding.
///
/// `CL_ENV_LOCK` only serialises access to the var across tests; it does
/// nothing about a single test that panics on an assertion between
/// `set_var`/`remove_var` and its intended trailing cleanup. Without this
/// guard that leaves the flag set (or cleared) for whichever test the process
/// happens to run next, which is a spurious, cascading, hard-to-reproduce
/// failure — not a real bug in the code under test. `Drop::drop` runs on
/// unwind, so constructing this guard right after taking `CL_ENV_LOCK` makes
/// cleanup unconditional.
///
/// Lives here (not in main.rs's test module), following `CL_ENV_LOCK`, so
/// both `cl_sim`'s own tests and `plan`'s tests can share one implementation.
/// Callers must still take `CL_ENV_LOCK` themselves first — this guard
/// governs value restoration, not cross-test serialisation.
#[cfg(test)]
pub(crate) struct MultiTickEnvGuard {
    previous: Option<String>,
}

#[cfg(test)]
impl MultiTickEnvGuard {
    /// Snapshot the current value, then set `ARBOT_CL_MULTI_TICK = value`.
    pub(crate) fn set(value: &str) -> Self {
        let previous = std::env::var("ARBOT_CL_MULTI_TICK").ok();
        std::env::set_var("ARBOT_CL_MULTI_TICK", value);
        Self { previous }
    }

    /// Snapshot the current value, then remove `ARBOT_CL_MULTI_TICK`.
    pub(crate) fn cleared() -> Self {
        let previous = std::env::var("ARBOT_CL_MULTI_TICK").ok();
        std::env::remove_var("ARBOT_CL_MULTI_TICK");
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for MultiTickEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("ARBOT_CL_MULTI_TICK", value),
            None => std::env::remove_var("ARBOT_CL_MULTI_TICK"),
        }
    }
}

pub fn local_cl_quotes_enabled() -> bool {
    std::env::var("ARBOT_LOCAL_CL_QUOTES")
        .map(|raw| !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

#[allow(dead_code)]
pub fn cl_quote_parity_enabled() -> bool {
    std::env::var("ARBOT_CL_QUOTE_PARITY")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn multi_tick_enabled() -> bool {
    crate::util::env_flag("ARBOT_CL_MULTI_TICK", false)
}

/// Bitmap words fetched per side when building a ladder.
pub fn cl_ladder_words() -> usize {
    crate::util::env_parse_opt::<usize>("ARBOT_CL_LADDER_WORDS")
        .unwrap_or(2)
        .clamp(1, crate::cl_ticks::MAX_TICK_WORDS)
}

/// Ceiling on tick crossings per quote. A swap needing more is reported
/// exhausted rather than quoted, bounding worst-case loop cost.
pub fn cl_max_ticks_crossed() -> u32 {
    crate::util::env_parse_opt::<u32>("ARBOT_CL_MAX_TICKS")
        .unwrap_or(128)
        .clamp(1, 1_024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_cl_quotes_default_enabled() {
        // Mutating a process-global env var races any other test that reads it
        // (plan::tests exercises the CL curve path gated on this flag), so both
        // sides must take the same lock.
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
        assert!(local_cl_quotes_enabled());
        std::env::set_var("ARBOT_LOCAL_CL_QUOTES", "0");
        assert!(!local_cl_quotes_enabled());
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
    }

    #[test]
    fn multi_tick_defaults_off() {
        let _lock = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MultiTickEnvGuard::cleared();
        assert!(
            !multi_tick_enabled(),
            "multi-tick must stay off until the parity harness is green"
        );
    }

    #[test]
    fn multi_tick_honours_the_flag() {
        let _lock = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MultiTickEnvGuard::set("1");
        assert!(multi_tick_enabled());
    }

    #[test]
    fn ladder_words_is_clamped_to_the_word_ceiling() {
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_LADDER_WORDS", "999");
        assert!(cl_ladder_words() <= crate::cl_ticks::MAX_TICK_WORDS);
        std::env::remove_var("ARBOT_CL_LADDER_WORDS");
    }
}
