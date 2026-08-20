//! Per-pool trust gate for the local multi-tick CL model.
//!
//! The multi-tick simulator is exact on almost every pool measured — 0 bps
//! against the on-chain quoter across both deep pools (>$60M) and thin ones
//! (~$10k), at 1..7 tick crossings. But on at least one Base pool
//! (`0xc211e1f853a898bd1302385ccde55f33a8c4b3f3`, WETH/cbBTC fee 100, $3.2M) it
//! is wrong by 40,000+ bps: already 62 bps off at a dust 0.0001 WETH swap, and
//! catastrophically over at 0.01 WETH. That pool's own reported state is
//! internally inconsistent — `slot0` + `liquidity()` imply 0.80 WETH absorbable
//! within one tick, while its own quoter saturates at 0.0012 WETH, a 667x
//! contradiction between two facts the pool asserts about itself.
//!
//! That defeats a purely local model: the inputs are self-consistent-looking
//! and wrong, so no amount of correct math detects it. The only authority is
//! the pool's own quoter.
//!
//! So: trust the local model **per pool**, and earn that trust by measuring.
//! A pool whose model disagrees with its quoter beyond `max_err_bps` loses its
//! ladder and falls back to on-chain quoting; every other pool keeps the fast
//! local path. Verdicts are cached with a TTL so the cost is one quoter call
//! per pool per TTL, not per scan — the whole point of the local path is to
//! avoid per-scan RPC, and this must not reintroduce it.
//!
//! Fails CLOSED: a pool with no verdict is not trusted. On a cold start every
//! pool quotes on-chain (correct, slower) and the fast path engages as verdicts
//! land. Trusting an unmeasured pool is exactly the failure this gate exists to
//! prevent.

use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Outcome of one parity measurement.
#[derive(Clone, Copy, Debug)]
struct Verdict {
    /// Retained for diagnosis: when a pool is rejected the operator needs the
    /// magnitude, not just the verdict.
    #[allow(dead_code)]
    err_bps: i64,
    trusted: bool,
    checked_at: Instant,
}

pub struct ParityGate {
    verdicts: Mutex<HashMap<Address, Verdict>>,
    ttl: Duration,
    max_err_bps: i64,
    checks_per_scan: usize,
}

/// Process-wide gate. Keyed by pool address, which is chain-unique, so one
/// instance is safe across chains in the same process.
pub fn gate() -> &'static ParityGate {
    static GATE: OnceLock<ParityGate> = OnceLock::new();
    GATE.get_or_init(ParityGate::from_env)
}

impl ParityGate {
    pub fn from_env() -> Self {
        Self {
            verdicts: Mutex::new(HashMap::new()),
            // Liquidity structure changes as positions are minted/burned, so a
            // verdict is evidence with a shelf life, not a permanent property.
            ttl: Duration::from_secs(
                crate::util::env_parse_opt::<u64>("ARBOT_CL_PARITY_TTL_SECS")
                    .unwrap_or(300)
                    .max(30),
            ),
            // Matches the cl_parity harness default so the offline sweep and
            // the runtime gate cannot disagree about what "passing" means.
            max_err_bps: i64::from(
                crate::util::env_parse_opt::<u32>("ARBOT_CL_PARITY_MAX_ERR_BPS").unwrap_or(5),
            ),
            // Bounded per-scan work: validation competes for the same RPC
            // budget as quoting, so it drips rather than stampedes.
            checks_per_scan: crate::util::env_parse_opt::<usize>(
                "ARBOT_CL_PARITY_CHECKS_PER_SCAN",
            )
            .unwrap_or(8)
            .clamp(1, 64),
        }
    }

    /// May this pool's ladder be used? Unknown or expired verdicts are NOT
    /// trusted.
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

    /// Record a measurement. `None` means the quoter could not be reached — that
    /// is NOT evidence of correctness, so nothing is stored and the pool stays
    /// untrusted until a real measurement lands.
    pub fn record(&self, pool: Address, err_bps: Option<i64>) {
        let Some(err_bps) = err_bps else {
            return;
        };
        let trusted = err_bps.abs() <= self.max_err_bps;
        if let Ok(mut guard) = self.verdicts.lock() {
            guard.insert(
                pool,
                Verdict {
                    trusted,
                    checked_at: Instant::now(),
                    err_bps,
                },
            );
        }
        if !trusted {
            tracing::warn!(
                pool = %format!("{pool:#x}"),
                err_bps,
                max_err_bps = self.max_err_bps,
                "CL model disagrees with the pool's own quoter; ladder withheld, \
                 this pool will quote on-chain"
            );
        }
    }

    /// Pools due for a check, capped at `checks_per_scan`.
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

    /// Summary for logging: (trusted, rejected, total tracked).
    pub fn stats(&self) -> (usize, usize, usize) {
        let Ok(guard) = self.verdicts.lock() else {
            return (0, 0, 0);
        };
        let total = guard.len();
        let trusted = guard
            .values()
            .filter(|v| v.trusted && v.checked_at.elapsed() < self.ttl)
            .count();
        (trusted, total.saturating_sub(trusted), total)
    }
}

/// Divergence of `model` from `on_chain`, in bps, signed.
///
/// `None` when the ratio is not representable — which is precisely the
/// catastrophic case (a units or decimals error producing an output orders of
/// magnitude out), so it must never be silently treated as agreement. Callers
/// record `None` as "no measurement", leaving the pool untrusted.
pub fn divergence_bps(model: U256, on_chain: U256) -> Option<i64> {
    if on_chain.is_zero() {
        return None;
    }
    let (diff, sign) = if model >= on_chain {
        (model - on_chain, 1i64)
    } else {
        (on_chain - model, -1i64)
    };
    let scaled = diff.checked_mul(U256::from(10_000u64))?;
    let bps = scaled / on_chain;
    if bps > U256::from(u64::MAX) {
        return None;
    }
    i64::try_from(bps.as_u128()).ok().map(|v| v * sign)
}

/// The local model's answer for one pool, or `None` when it declines to answer.
///
/// An exhausted quote is a lower bound on a partial fill, not a claim about the
/// pool. Comparing it against a complete on-chain fill would manufacture a
/// failure, so decline rather than measure.
///
/// Size matters and callers must pass the notional edges are actually quoted
/// at: the known-bad pool is only 62 bps out at 0.0001 WETH but 40,423 bps out
/// at 0.01 WETH, so measuring at a token amount would pass it.
pub fn model_quote(
    state: &crate::cl_sim::ClPoolState,
    ladder: &crate::cl_swap::TickLadder,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if amount_in.is_zero() {
        return None;
    }
    let q = crate::cl_swap::quote_exact_input_multi_tick(
        state,
        ladder,
        amount_in,
        zero_for_one,
        crate::cl_sim::cl_max_ticks_crossed(),
    )?;
    (!q.exhausted && !q.amount_out.is_zero()).then_some(q.amount_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn gate_with(max_err_bps: i64, ttl_secs: u64) -> ParityGate {
        ParityGate {
            verdicts: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_err_bps,
            checks_per_scan: 8,
        }
    }

    #[test]
    fn unmeasured_pools_are_not_trusted() {
        let g = gate_with(5, 300);
        assert!(
            !g.trusted(addr(1)),
            "an unmeasured pool must fail closed — trusting it is the exact \
             failure this gate exists to prevent"
        );
        assert!(g.needs_check(addr(1)));
    }

    #[test]
    fn a_passing_measurement_grants_trust() {
        let g = gate_with(5, 300);
        g.record(addr(1), Some(3));
        assert!(g.trusted(addr(1)));
        assert!(!g.needs_check(addr(1)), "a fresh verdict needs no recheck");
    }

    #[test]
    fn a_failing_measurement_withholds_the_ladder() {
        let g = gate_with(5, 300);
        // The real observed divergence on 0xc211e1f8.
        g.record(addr(1), Some(40_423));
        assert!(!g.trusted(addr(1)));
    }

    #[test]
    fn divergence_is_symmetric_and_signed() {
        let g = gate_with(5, 300);
        g.record(addr(1), Some(-40_423));
        assert!(!g.trusted(addr(1)), "under-quoting is a failure too");
    }

    #[test]
    fn an_unreachable_quoter_is_not_evidence_of_correctness() {
        let g = gate_with(5, 300);
        g.record(addr(1), None);
        assert!(
            !g.trusted(addr(1)),
            "a failed measurement must leave the pool untrusted, not trusted"
        );
        assert!(g.needs_check(addr(1)), "and must remain due for a check");
    }

    #[test]
    fn trust_expires_with_the_ttl() {
        let g = gate_with(5, 0); // ttl 0 => every verdict is immediately stale
        g.record(addr(1), Some(0));
        assert!(
            !g.trusted(addr(1)),
            "liquidity structure changes; a verdict is evidence with a shelf life"
        );
        assert!(g.needs_check(addr(1)));
    }

    #[test]
    fn due_for_check_is_bounded_and_skips_fresh_pools() {
        let g = gate_with(5, 300);
        g.record(addr(1), Some(0));
        let due = g.due_for_check((1..=20).map(addr));
        assert_eq!(due.len(), 8, "must respect checks_per_scan");
        assert!(!due.contains(&addr(1)), "a freshly-verified pool is not due");
    }

    #[test]
    fn divergence_bps_matches_the_harness() {
        // Model 5x the quoter is +40000 bps, the magnitude seen on 0xc211e1f8.
        assert_eq!(
            divergence_bps(U256::from(500u64), U256::from(100u64)),
            Some(40_000)
        );
        assert_eq!(divergence_bps(U256::from(100u64), U256::from(100u64)), Some(0));
        assert_eq!(divergence_bps(U256::from(101u64), U256::from(100u64)), Some(100));
        assert_eq!(divergence_bps(U256::from(99u64), U256::from(100u64)), Some(-100));
    }

    #[test]
    fn divergence_declines_to_answer_on_a_zero_reference() {
        assert_eq!(divergence_bps(U256::from(1u64), U256::zero()), None);
    }
}
