//! Per-pool trust gate for LOG-DERIVED STATE.
//!
//! Distinct from `cl_parity_gate`, which validates the multi-tick MATH against
//! the pool's own quoter given fresh state. This validates the STATE itself:
//! log-derived snapshot versus a fresh RPC read. Different causes, different
//! fixes, different TTLs — so a single verdict could not tell you which tripped.
//!
//! Fails CLOSED: a pool with no verdict is not trusted.

use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Verdict {
    /// Retained for diagnosis: when a pool is rejected the operator needs the
    /// magnitude, not just the verdict.
    #[allow(dead_code)]
    err_bps: i64,
    trusted: bool,
    checked_at: Instant,
}

/// How the tracked pool population currently splits.
///
/// Kept as three named counts rather than trusted/untrusted, because "not
/// trusted right now" and "measured and wrong" are different facts and only the
/// second is evidence about state quality.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateStats {
    /// Passing verdict, still inside its TTL.
    pub trusted: usize,
    /// Measured and diverged.
    pub failed: usize,
    /// Had a verdict, but it aged out. Says nothing about correctness.
    pub expired: usize,
    pub total: usize,
}

pub struct StateGate {
    verdicts: Mutex<HashMap<Address, Verdict>>,
    ttl: Duration,
    max_err_bps: i64,
    checks_per_scan: usize,
}

/// Process-wide gate, keyed by pool address (chain-unique).
#[allow(dead_code)]
pub fn gate() -> &'static StateGate {
    static GATE: OnceLock<StateGate> = OnceLock::new();
    GATE.get_or_init(StateGate::from_env)
}

#[allow(dead_code)]
impl StateGate {
    pub fn from_env() -> Self {
        Self {
            ttl: Duration::from_secs(
                crate::util::env_parse_opt::<u64>("ARBOT_STATE_GATE_TTL_SECS")
                    .unwrap_or(300)
                    .max(30),
            ),
            // Matches ARBOT_CL_PARITY_MAX_ERR_BPS so the two gates cannot
            // disagree about what "passing" means.
            max_err_bps: i64::from(
                crate::util::env_parse_opt::<u32>("ARBOT_STATE_GATE_MAX_ERR_BPS").unwrap_or(5),
            ),
            // 32/pass at the 15s default is ~128 pools/min per venue. With
            // ~700 tracked pools and a 300s verdict TTL, that is roughly full
            // coverage per TTL; at the previous default of 8 it was a quarter
            // of that, and most of those slots were wasted on pools too stale
            // to validate (see state_validation::validatable).
            checks_per_scan: crate::util::env_parse_opt::<usize>(
                "ARBOT_STATE_GATE_CHECKS_PER_SCAN",
            )
            .unwrap_or(32)
            .clamp(1, 256),
            verdicts: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn for_test(max_err_bps: i64, ttl_secs: u64, checks_per_scan: usize) -> Self {
        Self {
            verdicts: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_err_bps,
            checks_per_scan,
        }
    }

    /// May this pool's log-derived state be trusted? Unknown or expired
    /// verdicts are NOT trusted.
    pub fn trusted(&self, pool: Address) -> bool {
        let Ok(guard) = self.verdicts.lock() else {
            return false;
        };
        match guard.get(&pool) {
            Some(v) => v.trusted && v.checked_at.elapsed() < self.ttl,
            None => false,
        }
    }

    fn needs_check(&self, pool: Address) -> bool {
        let Ok(guard) = self.verdicts.lock() else {
            return false;
        };
        match guard.get(&pool) {
            Some(v) => v.checked_at.elapsed() >= self.ttl,
            None => true,
        }
    }

    /// `None` means the read failed. That is NOT evidence of correctness, so
    /// nothing is stored and the pool stays untrusted until a real measurement.
    pub fn record(&self, pool: Address, err_bps: Option<i64>) {
        let Some(err_bps) = err_bps else {
            return;
        };
        let trusted = err_bps.abs() <= self.max_err_bps;
        if let Ok(mut guard) = self.verdicts.lock() {
            guard.insert(
                pool,
                Verdict { trusted, checked_at: Instant::now(), err_bps },
            );
        }
        if !trusted {
            tracing::warn!(
                pool = %format!("{pool:#x}"),
                err_bps,
                max_err_bps = self.max_err_bps,
                "log-derived state disagrees with a fresh RPC read"
            );
        }
    }

    /// Pools due for a check, capped at `checks_per_scan`.
    ///
    /// Validation competes for the same RPC budget as quoting, so it drips
    /// rather than stampedes.
    pub fn due_for_check(&self, pools: impl IntoIterator<Item = Address>) -> Vec<Address> {
        let mut out = Vec::new();
        for pool in pools {
            if out.len() >= self.checks_per_scan {
                break;
            }
            if self.needs_check(pool) {
                out.push(pool);
            }
        }
        out
    }

    /// Partition of the tracked population.
    ///
    /// `failed` and `expired` are DISTINCT and must stay so. Deriving untrusted
    /// as `total - trusted` conflated them, so a passing verdict that merely
    /// aged out was reported as untrusted — which makes the §9 gate score
    /// re-check latency as untrustworthiness. `trusted + failed + expired`
    /// partitions `total` exactly.
    pub fn stats(&self) -> GateStats {
        let Ok(guard) = self.verdicts.lock() else {
            return GateStats::default();
        };
        let mut s = GateStats {
            total: guard.len(),
            ..GateStats::default()
        };
        for v in guard.values() {
            if v.checked_at.elapsed() >= self.ttl {
                s.expired += 1;
            } else if v.trusted {
                s.trusted += 1;
            } else {
                s.failed += 1;
            }
        }
        s
    }
}

/// Divergence of `local` from `on_chain` in bps, signed.
///
/// `None` when the ratio is not representable — precisely the catastrophic
/// case (a units error orders of magnitude out), so it must never read as
/// agreement. Callers record `None` as "no measurement".
#[allow(dead_code)]
pub fn divergence_bps(local: U256, on_chain: U256) -> Option<i64> {
    if on_chain.is_zero() {
        return None;
    }
    let (diff, sign) = if local >= on_chain {
        (local - on_chain, 1i64)
    } else {
        (on_chain - local, -1i64)
    };
    let scaled = diff.checked_mul(U256::from(10_000u64))?;
    let bps = scaled / on_chain;
    if bps > U256::from(u64::MAX) {
        return None;
    }
    i64::try_from(bps.as_u128()).ok().map(|v| v * sign)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn gate(max_err_bps: i64, ttl_secs: u64) -> StateGate {
        StateGate::for_test(max_err_bps, ttl_secs, 8)
    }

    #[test]
    fn an_unmeasured_pool_is_not_trusted() {
        let g = gate(5, 300);
        assert!(
            !g.trusted(addr(1)),
            "trusting the unmeasured is the failure this exists to prevent"
        );
    }

    #[test]
    fn a_passing_measurement_grants_trust() {
        let g = gate(5, 300);
        g.record(addr(1), Some(3));
        assert!(g.trusted(addr(1)));
    }

    #[test]
    fn a_failing_measurement_withholds_trust() {
        let g = gate(5, 300);
        g.record(addr(1), Some(4_000));
        assert!(!g.trusted(addr(1)));
    }

    #[test]
    fn under_reporting_fails_too() {
        let g = gate(5, 300);
        g.record(addr(1), Some(-4_000));
        assert!(!g.trusted(addr(1)));
    }

    /// A failed RPC is NOT evidence of correctness. It must write no verdict,
    /// leaving the TTL to age the pool out on its own — failure downgrades
    /// trust, it never stalls the searcher.
    #[test]
    fn an_unreachable_rpc_is_not_evidence() {
        let g = gate(5, 300);
        g.record(addr(1), None);
        assert!(!g.trusted(addr(1)));
        assert!(g.due_for_check([addr(1)]).contains(&addr(1)));
    }

    #[test]
    fn trust_expires_with_the_ttl() {
        let g = gate(5, 0);
        g.record(addr(1), Some(0));
        assert!(!g.trusted(addr(1)), "a verdict is evidence with a shelf life");
    }

    #[test]
    fn due_for_check_is_bounded_and_skips_fresh_pools() {
        let g = gate(5, 300);
        g.record(addr(1), Some(0));
        let due = g.due_for_check((1..=20).map(addr));
        assert_eq!(due.len(), 8, "must respect checks_per_scan");
        assert!(!due.contains(&addr(1)));
    }

    /// `untrusted` must mean MEASURED AND FAILED. An earlier version derived it
    /// as `total - trusted`, so a PASSING verdict that merely aged out counted
    /// as untrusted — observed live as trusted falling 212 -> 166 over 22
    /// minutes purely from expiry, while divergence was ~1.4%. Read that way,
    /// the §9 gate scores re-check latency as untrustworthiness.
    #[test]
    fn an_expired_pass_is_expired_not_failed() {
        let g = gate(5, 0); // ttl 0 => every verdict is immediately stale
        g.record(addr(1), Some(0)); // a PASS
        let s = g.stats();
        assert_eq!(s.trusted, 0, "an expired verdict is not currently trusted");
        assert_eq!(s.failed, 0, "but it did not fail — it aged out");
        assert_eq!(s.expired, 1);
        assert_eq!(s.total, 1);
    }

    #[test]
    fn a_failing_verdict_counts_as_failed_not_expired() {
        let g = gate(5, 300);
        g.record(addr(1), Some(4_000));
        let s = g.stats();
        assert_eq!(s.failed, 1);
        assert_eq!(s.expired, 0);
        assert_eq!(s.trusted, 0);
    }

    #[test]
    fn stats_partition_the_tracked_population() {
        let g = gate(5, 300);
        g.record(addr(1), Some(0));
        g.record(addr(2), Some(9_999));
        let s = g.stats();
        assert_eq!(s.trusted, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.expired, 0);
        assert_eq!(
            s.trusted + s.failed + s.expired,
            s.total,
            "the three buckets must partition the population exactly"
        );
    }

    #[test]
    fn divergence_is_signed_and_symmetric() {
        assert_eq!(
            divergence_bps(U256::from(101u64), U256::from(100u64)),
            Some(100)
        );
        assert_eq!(
            divergence_bps(U256::from(99u64), U256::from(100u64)),
            Some(-100)
        );
        assert_eq!(
            divergence_bps(U256::from(100u64), U256::from(100u64)),
            Some(0)
        );
    }

    /// A zero reference is exactly the catastrophic case, so it must decline
    /// rather than be silently treated as agreement.
    #[test]
    fn divergence_declines_on_a_zero_reference() {
        assert_eq!(divergence_bps(U256::from(1u64), U256::zero()), None);
    }
}
