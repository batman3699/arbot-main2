//! Advance pool state from a decoded pending swap for post-victim backrun search.

use std::collections::HashSet;

use ethers::types::{Address, U256};
use tracing::debug;

use crate::cl_sim::{quote_exact_input_single_tick, ClPoolState};
use crate::graph::{Graph, IndexedCycle};

#[derive(Clone, Debug)]
pub struct BackrunHint {
    pub from: Address,
    pub to: Address,
    pub amount_in: U256,
    pub price_impact_bps: u32,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct PostSwapPoolState {
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

pub fn backrun_post_state_enabled() -> bool {
    std::env::var("BACKRUN_POST_STATE")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Apply a simplified reserve update for V2-style pools after a victim swap.
pub fn advance_v2_reserves(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    amount_out: U256,
) -> Option<(U256, U256)> {
    if amount_in.is_zero() || amount_out.is_zero() {
        return None;
    }
    Some((
        reserve_in.saturating_add(amount_in),
        reserve_out.saturating_sub(amount_out),
    ))
}

/// Advance CL pool spot state after a victim exact-input swap (single-tick band).
pub fn advance_cl_state(
    state: &ClPoolState,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<ClPoolState> {
    let amount_out = quote_exact_input_single_tick(state, amount_in, zero_for_one, state.fee_ppm)
        .ok()??;
    if amount_out.is_zero() {
        return None;
    }
    let q96 = U256::from(1u128) << 96;
    let liq = U256::from(state.liquidity);
    let sqrt_p = state.sqrt_price_x96;
    let sqrt_p_next = if zero_for_one {
        let num = liq * sqrt_p;
        let denom = liq + amount_in * sqrt_p / q96;
        if denom.is_zero() {
            return None;
        }
        num / denom
    } else {
        sqrt_p + amount_in * q96 / liq
    };
    Some(ClPoolState {
        sqrt_price_x96: sqrt_p_next,
        liquidity: state.liquidity,
        tick: state.tick,
        tick_spacing: state.tick_spacing,
        fee_ppm: state.fee_ppm,
    })
}

pub fn post_state_from_hint(hint: &BackrunHint) -> PostSwapPoolState {
    PostSwapPoolState {
        pool: Address::zero(),
        token_in: hint.from,
        token_out: hint.to,
        amount_in: hint.amount_in,
        amount_out: U256::zero(),
    }
}

pub fn touched_pools_from_hints(hints: &[BackrunHint]) -> HashSet<Address> {
    let mut touched = HashSet::new();
    for hint in hints {
        touched.insert(hint.from);
        touched.insert(hint.to);
    }
    touched
}

pub fn log_backrun_opportunity(
    from: Address,
    to: Address,
    amount_in: U256,
    impact_bps: u32,
    source: &str,
    cycles: &[IndexedCycle],
) {
    debug!(
        target: "backrun",
        from = ?from,
        to = ?to,
        amount_in = %amount_in,
        impact_bps,
        source,
        cycles_found = cycles.len(),
        "backrun_opportunity_detected"
    );
}

pub fn targeted_bf_limits() -> crate::graph::BellmanFordLimits {
    crate::graph::BellmanFordLimits {
        min_hops: 2,
        max_hops: 4,
        max_relaxations: 32,
        max_cycles: 10,
        timeout: std::time::Duration::from_millis(50),
    }
}

/// Mark pools affected by mempool backrun hints for incremental populate (WP-6).
pub fn apply_post_state_hints(
    graph: &mut Graph,
    hints: &[BackrunHint],
    touched: &mut HashSet<Address>,
) {
    for hint in hints {
        touched.insert(hint.from);
        touched.insert(hint.to);
        for edge in graph.edges.iter_mut() {
            if edge.from == hint.from && edge.to == hint.to {
                edge.weight = edge.weight.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_v2_reserves_updates_balances() {
        let (rin, rout) = advance_v2_reserves(
            U256::from(1_000u64),
            U256::from(2_000u64),
            U256::from(100u64),
            U256::from(180u64),
        )
        .expect("advance");
        assert_eq!(rin, U256::from(1_100u64));
        assert_eq!(rout, U256::from(1_820u64));
    }
}
