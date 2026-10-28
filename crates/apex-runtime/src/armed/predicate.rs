//! The predicates an armed transaction carries (R26), as Base's
//! `base_sendRawTransactionValidity` takes them: raw storage words, masked
//! and compared, and a block bound. Layouts verified on chain, 2026-10-12.

use crate::live::book::PoolSnapshot;
use crate::live::inventory::Venue;
use alloy_primitives::Address;
use ethers_core::types::U256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Whose storage the words are: the pool's, or PoolManager's for a v4
    /// pool (R37).
    pub address: Address,
    /// The word whose low 160 bits are sqrtPriceX96.
    pub price_slot: Option<U256>,
    /// The word whose low 128 bits are the in-range liquidity.
    pub liquidity_slot: Option<U256>,
    /// reserve0's and reserve1's words.
    pub reserve_slots: Option<(U256, U256)>,
}

/// The words of the pool at `address` on venue `v`. A v4 pool's words are
/// PoolManager's, found by its pool ID: none here, see [`layout_of`].
pub fn layout(v: Venue, address: Address) -> Layout {
    let cl = |price: u64, liquidity: u64| Layout {
        address,
        price_slot: Some(U256::from(price)),
        liquidity_slot: Some(U256::from(liquidity)),
        reserve_slots: None,
    };
    match v {
        Venue::UniswapV3 => cl(0, 4),
        Venue::PancakeV3 => cl(0, 5),
        Venue::Slipstream | Venue::SlipstreamV3 => cl(6, 16),
        Venue::AerodromeV2 => {
            Layout { address, price_slot: None, liquidity_slot: None, reserve_slots: Some((U256::from(20), U256::from(21))) }
        }
        Venue::UniswapV4 => Layout { address, price_slot: None, liquidity_slot: None, reserve_slots: None },
    }
}

/// The words `p`'s predicates watch (R37): a v4 pool's slot0 and liquidity
/// in PoolManager's storage, at its state slot and three past it; without
/// its key, none, and it is left unarmed.
pub fn layout_of(p: &PoolSnapshot) -> Layout {
    match (p.spec.venue, p.spec.v4) {
        (Venue::UniswapV4, Some(k)) => {
            let word = |b: alloy_primitives::B256| U256::from_big_endian(b.as_slice());
            Layout {
                address: crate::live::v4::POOL_MANAGER,
                price_slot: Some(word(crate::live::v4::state_slot(k.pool_id))),
                liquidity_slot: Some(word(crate::live::v4::liquidity_slot(k.pool_id))),
                reserve_slots: None,
            }
        }
        (v, _) => layout(v, p.spec.pool),
    }
}

pub fn price_mask() -> U256 {
    (U256::one() << 160) - U256::one()
}

pub fn liquidity_mask() -> U256 {
    (U256::one() << 128) - U256::one()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = ">=")]
    AtLeast,
    #[serde(rename = "<=")]
    AtMost,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "params", rename_all = "snake_case")]
pub enum Predicate {
    Storage {
        address: String,
        slot: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<String>,
        op: Op,
        value: String,
    },
    BlockNumber {
        op: Op,
        value: String,
    },
}

fn hex(v: U256) -> String {
    format!("{v:#x}")
}

fn storage(address: Address, slot: U256, mask: Option<U256>, op: Op, value: U256) -> Predicate {
    Predicate::Storage { address: format!("{address:#x}"), slot: hex(slot), mask: mask.map(hex), op, value: hex(value) }
}

/// The trigger past its threshold: `at` is the trigger pool at the threshold.
/// A constant-product pool needs both reserves bounded, so a mint (both rise)
/// cannot fire it.
pub fn trigger(p: &PoolSnapshot, at: &PoolSnapshot, up: bool) -> Option<Vec<Predicate>> {
    let l = layout_of(p);
    let pool = l.address;
    match (&at.reserves, l.reserve_slots, l.price_slot) {
        // token0's price in token1 up: reserve0 falls, reserve1 rises.
        (Some(r), Some((s0, s1)), _) if up => Some(vec![
            storage(pool, s0, None, Op::AtMost, r.reserve0),
            storage(pool, s1, None, Op::AtLeast, r.reserve1),
        ]),
        (Some(r), Some((s0, s1)), _) => Some(vec![
            storage(pool, s0, None, Op::AtLeast, r.reserve0),
            storage(pool, s1, None, Op::AtMost, r.reserve1),
        ]),
        (None, _, Some(slot)) => Some(vec![storage(
            pool,
            slot,
            Some(price_mask()),
            if up { Op::AtLeast } else { Op::AtMost },
            at.state.sqrt_price_x96,
        )]),
        _ => None,
    }
}

/// The other pool between `edges` (`[lower, upper]` in price), and its
/// liquidity at least `floor_bps` of what it holds.
pub fn band(p: &PoolSnapshot, edges: &[PoolSnapshot; 2], floor_bps: u32) -> Option<Vec<Predicate>> {
    let l = layout_of(p);
    let pool = l.address;
    let [lo, hi] = edges;
    match (&p.reserves, l.reserve_slots, l.price_slot, l.liquidity_slot) {
        (Some(_), Some((s0, s1)), _, _) => {
            let (a, b) = (lo.reserves.as_ref()?, hi.reserves.as_ref()?);
            Some(vec![
                storage(pool, s0, None, Op::AtLeast, a.reserve0.min(b.reserve0)),
                storage(pool, s0, None, Op::AtMost, a.reserve0.max(b.reserve0)),
                storage(pool, s1, None, Op::AtLeast, a.reserve1.min(b.reserve1)),
                storage(pool, s1, None, Op::AtMost, a.reserve1.max(b.reserve1)),
            ])
        }
        (None, _, Some(price), Some(liquidity)) => {
            let floor = U256::from(p.state.liquidity) * U256::from(floor_bps) / U256::from(10_000u32);
            Some(vec![
                storage(pool, price, Some(price_mask()), Op::AtLeast, lo.state.sqrt_price_x96),
                storage(pool, price, Some(price_mask()), Op::AtMost, hi.state.sqrt_price_x96),
                storage(pool, liquidity, Some(liquidity_mask()), Op::AtLeast, floor),
            ])
        }
        _ => None,
    }
}

/// No later than `to_block`.
pub fn expiry(to_block: u64) -> Predicate {
    Predicate::BlockNumber { op: Op::AtMost, value: format!("{to_block:#x}") }
}
