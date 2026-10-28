//! Uniswap v4 (Task 8.5 R37): one PoolManager holds every pool, keyed by a
//! 32-byte pool ID, its state at hashed slots. This module is the layout and
//! the admission rules, pure.
//!
//! # The layout (v4-core's `StateLibrary`, verified on chain 2026-10-28)
//!
//! A pool's state begins at `keccak256(abi.encode(poolId, 6))`:
//! - slot0 there: sqrtPriceX96 (bits 0–159), tick (160–183, signed),
//!   protocolFee (184–207), lpFee (208–231);
//! - liquidity at state + 3;
//! - ticks, `mapping(int24 => TickInfo)`, at state + 4; a tick's first word
//!   is liquidityGross (low 128 bits) and liquidityNet (high 128);
//! - the tick bitmap, `mapping(int16 => uint256)`, at state + 5.
//!
//! The book keys a v4 pool by [`key_address`], the first 20 bytes of its ID,
//! so every address-keyed map holds it as it holds any other pool.

use alloy_primitives::{address, keccak256, Address, B256, U256};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Uniswap v4's PoolManager on Base.
pub const POOL_MANAGER: Address = address!("498581fF718922c3f8e6A244956aF099B2652b2b");

/// `StateLibrary.POOLS_SLOT`: the `pools` mapping's slot.
pub const POOLS_SLOT: u64 = 6;

/// A pool key's fee with this bit set is dynamic: its hook sets it.
pub const DYNAMIC_FEE_FLAG: u32 = 0x80_0000;

/// The four hook permissions that return a delta: with any of them, the hook,
/// not the pool, decides what a swap or a position moves.
pub const RETURNS_DELTA_FLAGS: u16 = 0b1111;

/// What a v4 pool key holds beyond its two currencies (the pool's tokens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V4Key {
    pub pool_id: B256,
    /// 24 bits, as the key's `uint24`.
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
}

/// A pool's slot0 word, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot0 {
    pub sqrt_price_x96: U256,
    pub tick: i32,
    /// Two 12-bit fees, zero-for-one low: the protocol's share of the LP fee.
    pub protocol_fee: u32,
    pub lp_fee: u32,
}

/// The book's 20-byte key for a v4 pool: its ID's first 20 bytes.
pub fn key_address(pool_id: B256) -> Address {
    Address::from_slice(&pool_id[..20])
}

fn slot_of(key: [u8; 32], slot: U256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&key);
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    keccak256(buf)
}

fn plus(slot: B256, n: u64) -> U256 {
    U256::from_be_bytes(slot.0) + U256::from(n)
}

/// A signed key, sign-extended to 32 bytes as `abi.encode` writes it.
fn signed_word(v: i64) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

/// The pool's state slot: `keccak256(abi.encode(poolId, 6))`, its slot0.
pub fn state_slot(pool_id: B256) -> B256 {
    slot_of(pool_id.0, U256::from(POOLS_SLOT))
}

pub fn liquidity_slot(pool_id: B256) -> B256 {
    B256::from(plus(state_slot(pool_id), 3).to_be_bytes::<32>())
}

/// `tickBitmap[word]`: `keccak256(abi.encode(int16 word, state + 5))`.
pub fn bitmap_slot(pool_id: B256, word: i16) -> B256 {
    slot_of(signed_word(i64::from(word)), plus(state_slot(pool_id), 5))
}

/// `ticks[tick]`'s first word: `keccak256(abi.encode(int24 tick, state + 4))`.
pub fn tick_slot(pool_id: B256, tick: i32) -> B256 {
    slot_of(signed_word(i64::from(tick)), plus(state_slot(pool_id), 4))
}

pub fn decode_slot0(word: U256) -> Slot0 {
    let field = |shift: usize, bits: usize| (word >> shift) & ((U256::from(1) << bits) - U256::from(1));
    let raw_tick = field(160, 24).to::<u32>();
    let tick = if raw_tick >= 1 << 23 { raw_tick as i32 - (1 << 24) } else { raw_tick as i32 };
    Slot0 {
        sqrt_price_x96: field(0, 160),
        tick,
        protocol_fee: field(184, 24).to::<u32>(),
        lp_fee: field(208, 24).to::<u32>(),
    }
}

/// A hook's permissions: the low 14 bits of its address.
pub fn hook_flags(hooks: Address) -> u16 {
    u16::from_be_bytes([hooks[18], hooks[19]]) & 0x3fff
}

/// Why a v4 pool is refused, or `None` when it is admissible: no native
/// currency, a static fee, no hook that returns a delta, and any hook on the
/// reviewed allowlist.
pub fn refusal(key: &V4Key, currency0: Address, currency1: Address, allowed_hooks: &BTreeSet<Address>) -> Option<&'static str> {
    if currency0 == Address::ZERO || currency1 == Address::ZERO {
        return Some("native currency");
    }
    if key.fee & DYNAMIC_FEE_FLAG != 0 {
        return Some("dynamic fee");
    }
    if hook_flags(key.hooks) & RETURNS_DELTA_FLAGS != 0 {
        return Some("hook returns a delta");
    }
    if key.hooks != Address::ZERO && !allowed_hooks.contains(&key.hooks) {
        return Some("hook not allowlisted");
    }
    None
}

/// The hooks a v4 pool may carry: each reviewed by hand, and its code hash at
/// review, so a hook whose code changes is refused until reviewed again
/// (`ops/v4-hooks.base.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookAllowlist {
    pub hooks: BTreeMap<Address, B256>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    hooks: Vec<HookEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookEntry {
    address: Address,
    code_hash: B256,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    reason: String,
    #[allow(dead_code)]
    reviewed: String,
}

impl HookAllowlist {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: HookFile = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { hooks: file.hooks.into_iter().map(|h| (h.address, h.code_hash)).collect() })
    }

    pub fn addresses(&self) -> BTreeSet<Address> {
        self.hooks.keys().copied().collect()
    }
}

