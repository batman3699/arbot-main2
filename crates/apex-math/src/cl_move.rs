//! Where a concentrated-liquidity pool's state lands when its price moves to a
//! target (Task 8.5 R26).
//!
//! The state at a price does not depend on the swaps that took it there, only
//! on the liquidity it crossed: walking the ladder to the target and crossing
//! each initialized tick gives exactly what the pool will hold. An armed
//! backrun is priced against that state before any trigger exists.
//!
//! Crossing follows v3-core: reaching an initialized tick's price crosses it,
//! and a downward crossing leaves the tick one below it. Balances move by what
//! flows between the prices, fee-less: the side leaving the pool rounds up and
//! the side entering it rounds down, so the pool is never shown richer.

use crate::cl_math::{
    get_amount0_delta, get_amount1_delta, get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio,
    min_sqrt_ratio,
};
use crate::cl_state::ClPoolState;
use crate::cl_swap::{LadderStep, TickLadder};
use ethers_core::types::U256;

/// Initialized ticks a move may cross: the ladder's own scale, with room.
const MAX_CROSSINGS: usize = 10_000;

fn apply_net(liquidity: u128, net: i128) -> Option<u128> {
    if net >= 0 {
        liquidity.checked_add(net.unsigned_abs())
    } else {
        liquidity.checked_sub(net.unsigned_abs())
    }
}

/// `state` with its price at `target`: tick, liquidity and balances as the pool
/// would hold them. `None` when the move would leave the ladder's proven range,
/// drain liquidity or a balance, or the target is outside the protocol's prices.
pub fn move_to_price(state: &ClPoolState, ladder: &TickLadder, target: U256) -> Option<ClPoolState> {
    if state.liquidity == 0 || target < min_sqrt_ratio() || target >= max_sqrt_ratio() || !ladder.covers(state.tick) {
        return None;
    }
    let mut s = state.clone();
    if target == s.sqrt_price_x96 {
        return Some(s);
    }
    let down = target < s.sqrt_price_x96;
    let (mut into, mut out) = (U256::zero(), U256::zero());
    for _ in 0..MAX_CROSSINGS {
        let next = match ladder.next_initialized(s.tick, down) {
            LadderStep::Initialized { tick, liquidity_net } => Some((tick, liquidity_net, get_sqrt_ratio_at_tick(tick)?)),
            LadderStep::Exhausted => None,
        };
        let crossing = next.filter(|(_, _, at)| if down { target <= *at } else { target >= *at });
        let stop = crossing.map_or(target, |(_, _, at)| at);
        // With no initialized tick before the target, the ladder must prove the
        // target itself, or the liquidity on the way is unknown.
        if crossing.is_none() && !ladder.covers(get_tick_at_sqrt_ratio(target)?) {
            return None;
        }
        let (from, to) = (s.sqrt_price_x96, stop);
        if down {
            into = into.checked_add(get_amount0_delta(from, to, s.liquidity, false)?)?;
            out = out.checked_add(get_amount1_delta(from, to, s.liquidity, true)?)?;
        } else {
            into = into.checked_add(get_amount1_delta(from, to, s.liquidity, false)?)?;
            out = out.checked_add(get_amount0_delta(from, to, s.liquidity, true)?)?;
        }
        s.sqrt_price_x96 = stop;
        let Some((tick, net, _)) = crossing else {
            s.tick = get_tick_at_sqrt_ratio(stop)?;
            return settle(s, down, into, out);
        };
        let signed = if down { net.checked_neg()? } else { net };
        s.liquidity = apply_net(s.liquidity, signed).filter(|l| *l > 0)?;
        s.tick = if down { tick - 1 } else { tick };
        if stop == target {
            return settle(s, down, into, out);
        }
    }
    None
}

fn settle(mut s: ClPoolState, down: bool, into: U256, out: U256) -> Option<ClPoolState> {
    let (entering, leaving) = if down { (&mut s.balance0, &mut s.balance1) } else { (&mut s.balance1, &mut s.balance0) };
    if let Some(b) = entering.as_mut() {
        *b = b.checked_add(into)?;
    }
    if let Some(b) = leaving.as_mut() {
        *b = b.checked_sub(out)?;
    }
    Some(s)
}
