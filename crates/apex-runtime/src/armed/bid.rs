//! An armed transaction's bid (R36): a priority fee per gas, against what the
//! winners on its route paid per gas.
//!
//! Base orders eligible transactions by priority fee per gas, so the bid that
//! competes is per gas, not a share of value (R35's winners bid a median 22%
//! of value, but on 190-740k gas: a share says nothing about the price per
//! unit we must beat on ours). A validity transaction that loses is never
//! included — its predicates turn false once the winner closes the gap — so
//! its tip is paid only on a win, unlike the reactive lane's (R31).

use ethers_core::types::U256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bid {
    /// Wei per gas over the base fee.
    pub per_gas: u128,
    /// The share of measured winners it outbids.
    pub p_win: f64,
}

/// The bid `b` maximising `F(b) × (net − b × gas)`, `F(b)` the share of
/// `sorted_bids` strictly below `b`: over bidding nothing and every measured
/// bid + 1 wei up to `cap_wei_per_gas`. Integer: `below × (net − b × gas)`
/// over the same count. A tie keeps the lower bid.
pub fn best_bid(sorted_bids: &[u128], net_wei: U256, gas: u64, cap_wei_per_gas: u128) -> Bid {
    let n = sorted_bids.len();
    if n == 0 || gas == 0 {
        return Bid { per_gas: 0, p_win: 0.0 };
    }
    let net = if net_wei > U256::from(u128::MAX) { u128::MAX } else { net_wei.as_u128() };
    let (mut best_b, mut best_below, mut best) = (0u128, 0usize, 0u128);
    let mut i = 0;
    while i < n {
        let v = sorted_bids[i];
        while i < n && sorted_bids[i] == v {
            i += 1;
        }
        let b = v.saturating_add(1);
        if b > cap_wei_per_gas {
            break;
        }
        let Some(kept) = b.checked_mul(u128::from(gas)).and_then(|tip| net.checked_sub(tip)) else { continue };
        let value = (i as u128).saturating_mul(kept);
        if value > best {
            (best_b, best_below, best) = (b, i, value);
        }
    }
    Bid { per_gas: best_b, p_win: best_below as f64 / n as f64 }
}

/// What an arm is worth to hold: the chance its trigger moves past the rung
/// within the window, times the chance its bid wins, times what the win keeps
/// after the tip. Negative when the tip outweighs the surplus.
pub fn rank_value(p_trigger: f64, bid: &Bid, surplus_wei: U256, gas: u64) -> f64 {
    let surplus = surplus_wei.as_u128() as f64;
    let tip = bid.per_gas as f64 * gas as f64;
    p_trigger * bid.p_win * (surplus - tip)
}
