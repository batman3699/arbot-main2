//! Comparison of log-derived state against a fresh RPC read.
//!
//! Pure: no provider, no async. The RPC read happens in the validation task;
//! this decides what the numbers mean.

use crate::live_state::{ClSnapshot, TrustState, V2Snapshot};
use crate::state_gate::divergence_bps;
use ethers::types::U256;

/// One comparison of local state against the chain — the spec §9.1 record.
///
/// Field names match the spec so the emitted log and the document stay in step.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Reconciliation {
    pub local: String,
    pub on_chain: String,
    pub absolute_delta: String,
    /// Signed, worst field. `None` means the ratio is not representable, which
    /// is "no measurement", NOT agreement.
    pub relative_delta_bps: Option<i64>,
    /// CL only. Reported separately because a tick is a signed exponent and a
    /// bps ratio on it is meaningless.
    pub tick_delta: Option<i32>,
    pub local_state_version: u64,
    pub anchor_id: u64,
    pub continuity_epoch: u64,
    pub trust_state: TrustState,
    pub venue: &'static str,
}

fn abs_delta(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

/// Worse of two divergences, preserving sign.
///
/// `None` from either is contagious: an unmeasurable field means the comparison
/// as a whole is unmeasurable. Averaging would let a decoder that reads one
/// field correctly and another from the wrong offset look healthy.
fn worse(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.abs() >= y.abs() { x } else { y }),
        _ => None,
    }
}

#[allow(dead_code)]
pub fn compare_v2(
    local: &V2Snapshot,
    chain: &crate::quote_univ2::UniV2PairState,
) -> Reconciliation {
    let d0 = divergence_bps(local.state.reserve0, chain.reserve0);
    let d1 = divergence_bps(local.state.reserve1, chain.reserve1);
    Reconciliation {
        local: format!("r0={} r1={}", local.state.reserve0, local.state.reserve1),
        on_chain: format!("r0={} r1={}", chain.reserve0, chain.reserve1),
        absolute_delta: format!(
            "r0={} r1={}",
            abs_delta(local.state.reserve0, chain.reserve0),
            abs_delta(local.state.reserve1, chain.reserve1)
        ),
        relative_delta_bps: worse(d0, d1),
        tick_delta: None,
        local_state_version: local.prov.state_version,
        anchor_id: local.prov.anchor_id,
        continuity_epoch: local.prov.continuity_epoch,
        trust_state: local.prov.trust,
        venue: "v2",
    }
}

#[allow(dead_code)]
pub fn compare_cl(local: &ClSnapshot, chain: &crate::cl_sim::ClPoolState) -> Reconciliation {
    let dp = divergence_bps(local.sqrt_price_x96, chain.sqrt_price_x96);
    let dl = divergence_bps(U256::from(local.liquidity), U256::from(chain.liquidity));
    Reconciliation {
        local: format!(
            "sqrtP={} L={} tick={}",
            local.sqrt_price_x96, local.liquidity, local.tick
        ),
        on_chain: format!(
            "sqrtP={} L={} tick={}",
            chain.sqrt_price_x96, chain.liquidity, chain.tick
        ),
        absolute_delta: format!(
            "sqrtP={} L={}",
            abs_delta(local.sqrt_price_x96, chain.sqrt_price_x96),
            abs_delta(U256::from(local.liquidity), U256::from(chain.liquidity))
        ),
        relative_delta_bps: worse(dp, dl),
        tick_delta: Some(local.tick - chain.tick),
        local_state_version: local.prov.state_version,
        anchor_id: local.prov.anchor_id,
        continuity_epoch: local.prov.continuity_epoch,
        trust_state: local.prov.trust,
        venue: "cl",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_state::{ClSnapshot, Provenance, TrustState, V2Snapshot};
    use crate::quote_univ2::UniV2PairState;
    use ethers::types::{Address, U256};
    use std::time::Instant;

    fn prov(version: u64) -> Provenance {
        Provenance {
            state_version: version,
            anchor_id: 7,
            continuity_epoch: 3,
            ordinal: Some(crate::continuity::Ordinal {
                block: 500,
                tx_index: 1,
                log_index: 2,
            }),
            anchored_at: Instant::now(),
            trust: TrustState::Derived,
        }
    }

    fn v2_snap(r0: u64, r1: u64) -> V2Snapshot {
        V2Snapshot {
            state: UniV2PairState {
                token0: Address::from_low_u64_be(10),
                token1: Address::from_low_u64_be(11),
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
            },
            prov: prov(42),
        }
    }

    fn chain_v2(r0: u64, r1: u64) -> UniV2PairState {
        UniV2PairState {
            token0: Address::from_low_u64_be(10),
            token1: Address::from_low_u64_be(11),
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
        }
    }

    #[test]
    fn identical_v2_state_reports_zero_divergence() {
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(r.relative_delta_bps, Some(0));
        assert_eq!(r.local_state_version, 42);
        assert_eq!(r.anchor_id, 7);
        assert_eq!(r.continuity_epoch, 3);
    }

    /// Both reserves are compared and the WORSE one wins. A decoder that reads
    /// one field correctly and the other from the wrong offset must not be
    /// averaged into looking healthy.
    #[test]
    fn v2_takes_the_worse_of_the_two_reserves() {
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(1_000, 4_000));
        assert_eq!(
            r.relative_delta_bps,
            Some(-5_000),
            "must report the 50% miss, not average it away"
        );
    }

    #[test]
    fn v2_divergence_is_signed() {
        let over = compare_v2(&v2_snap(1_100, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(over.relative_delta_bps, Some(1_000));
        let under = compare_v2(&v2_snap(900, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(under.relative_delta_bps, Some(-1_000));
    }

    /// A zero on-chain reserve makes the ratio meaningless. It must report
    /// "no measurement" rather than agreement — the whole point of Option.
    #[test]
    fn v2_declines_on_a_zero_reference() {
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(0, 2_000));
        assert_eq!(r.relative_delta_bps, None);
    }

    fn cl_snap(sqrt: u64, liq: u128, tick: i32) -> ClSnapshot {
        ClSnapshot {
            sqrt_price_x96: U256::from(sqrt),
            liquidity: liq,
            tick,
            prov: prov(9),
        }
    }

    fn chain_cl(sqrt: u64, liq: u128, tick: i32) -> crate::cl_sim::ClPoolState {
        crate::cl_sim::ClPoolState {
            sqrt_price_x96: U256::from(sqrt),
            liquidity: liq,
            tick,
            tick_spacing: 60,
            fee_ppm: 3000,
            balance0: None,
            balance1: None,
        }
    }

    #[test]
    fn identical_cl_state_reports_zero_divergence() {
        let r = compare_cl(
            &cl_snap(1_000_000, 5_000, -100),
            &chain_cl(1_000_000, 5_000, -100),
        );
        assert_eq!(r.relative_delta_bps, Some(0));
        assert_eq!(r.tick_delta, Some(0));
    }

    /// sqrt_price and liquidity are both compared; the worse wins. A decoder
    /// reading sqrt_price from the right offset and liquidity from the wrong
    /// one is exactly the bug this must catch.
    #[test]
    fn cl_takes_the_worse_of_price_and_liquidity() {
        let r = compare_cl(
            &cl_snap(1_000_000, 5_000, 0),
            &chain_cl(1_000_000, 10_000, 0),
        );
        assert_eq!(
            r.relative_delta_bps,
            Some(-5_000),
            "liquidity halved must surface"
        );
    }

    /// Tick is reported separately and NOT folded into bps: it is a signed
    /// exponent, so a bps ratio on it is meaningless. An off-by-sign tick is
    /// catastrophic and must be visible on its own axis.
    #[test]
    fn cl_reports_tick_delta_separately() {
        let r = compare_cl(
            &cl_snap(1_000, 5_000, -198_238),
            &chain_cl(1_000, 5_000, 198_238),
        );
        assert_eq!(r.relative_delta_bps, Some(0), "price and liquidity agree");
        assert_eq!(
            r.tick_delta,
            Some(-396_476),
            "a sign-flipped tick must be visible even when price matches"
        );
    }
}
