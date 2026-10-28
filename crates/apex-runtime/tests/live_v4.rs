//! Task 8.5 R37: Uniswap v4's storage, as PoolManager lays it out, and the
//! rules a v4 pool is admitted by. The slots and the slot0 word are the
//! chain's (read 2026-10-28, StateView agreeing to the wei).

use alloy_primitives::{address, b256, keccak256, Address, B256, U256};
use apex_runtime::live::v4::{
    bitmap_slot, decode_slot0, hook_flags, key_address, liquidity_slot, refusal, state_slot, tick_slot, V4Key,
    POOL_MANAGER,
};
use std::collections::BTreeSet;

/// AXIS/USDC's leading v4 pool: fee 100, spacing 1, hooked by V4AlphaHook.
const AXIS_POOL: B256 = b256!("c19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa");
const ALPHA_HOOK: Address = address!("b0b24b89db0dafbe43c5b40226b63a179f592880");
const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");

fn word(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

#[test]
fn pool_manager_is_base_s() {
    assert_eq!(POOL_MANAGER, address!("498581ff718922c3f8e6a244956af099b2652b2b"));
}

#[test]
fn the_axis_pool_s_slots_are_the_chain_s() {
    // `cast index bytes32 <id> 6`.
    let state = state_slot(AXIS_POOL);
    assert_eq!(state, b256!("a446a125276052ec378b11eacacc4812f36f5e00ab66ee87cd802ad56ce21aa5"));
    assert_eq!(U256::from_be_bytes(liquidity_slot(AXIS_POOL).0), U256::from_be_bytes(state.0) + U256::from(3));
}

#[test]
fn a_pool_s_key_address_is_its_id_s_first_twenty_bytes() {
    assert_eq!(key_address(AXIS_POOL), address!("c19d91d99afc0332e5013a569c9721cd9ecb51a1"));
}

#[test]
fn slot0_decodes_as_state_view_reads_it() {
    let w = U256::from_be_bytes(b256!("000000000064000000fb936200000000000000000000087d3eff65fbcecdc0a1").0);
    let s = decode_slot0(w);
    assert_eq!(s.sqrt_price_x96, U256::from(40_089_314_331_252_449_198_241u128));
    assert_eq!(s.tick, -289_950);
    assert_eq!(s.protocol_fee, 0);
    assert_eq!(s.lp_fee, 100);
}

/// `abi.encode(int16)` and `abi.encode(int24)` sign-extend to 32 bytes: a slot
/// computed from the unsigned value would read an empty word.
#[test]
fn v4_slots_sign_extend_negative_keys() {
    let state = U256::from_be_bytes(state_slot(AXIS_POOL).0);
    let mut buf = [0xffu8; 32].to_vec();
    buf.extend_from_slice(&word(state + U256::from(5)));
    assert_eq!(bitmap_slot(AXIS_POOL, -1), keccak256(&buf), "word -1");

    let mut buf = Vec::new();
    buf.extend_from_slice(&word(U256::MAX - U256::from(289_950 - 1)));
    buf.extend_from_slice(&word(state + U256::from(4)));
    assert_eq!(tick_slot(AXIS_POOL, -289_950), keccak256(&buf), "tick -289,950");

    let mut buf = Vec::new();
    buf.extend_from_slice(&word(U256::from(7)));
    buf.extend_from_slice(&word(state + U256::from(5)));
    assert_eq!(bitmap_slot(AXIS_POOL, 7), keccak256(&buf), "a positive word");
}

#[test]
fn the_alpha_hook_s_flags_are_before_initialize_add_liquidity_and_swap() {
    assert_eq!(hook_flags(ALPHA_HOOK), 0x2880);
}

fn key(fee: u32, hooks: Address) -> V4Key {
    V4Key { pool_id: AXIS_POOL, fee, tick_spacing: 1, hooks }
}

#[test]
fn refusals() {
    let allowed: BTreeSet<Address> = [ALPHA_HOOK].into_iter().collect();
    let none = BTreeSet::new();
    assert_eq!(refusal(&key(100, ALPHA_HOOK), AXIS, USDC, &allowed), None, "an allowlisted inert hook");
    assert_eq!(refusal(&key(3_000, Address::ZERO), AXIS, USDC, &none), None, "no hook");
    assert_eq!(refusal(&key(3_000, Address::ZERO), Address::ZERO, USDC, &none), Some("native currency"));
    assert_eq!(refusal(&key(0x80_0000, Address::ZERO), AXIS, USDC, &none), Some("dynamic fee"));
    // Low bits 0x2884: AFTER_SWAP_RETURNS_DELTA set on top of the Alpha hook's.
    let returns = address!("b0b24b89db0dafbe43c5b40226b63a179f592884");
    assert_eq!(refusal(&key(100, returns), AXIS, USDC, &[returns].into_iter().collect()), Some("hook returns a delta"));
    assert_eq!(refusal(&key(100, ALPHA_HOOK), AXIS, USDC, &none), Some("hook not allowlisted"));
}
