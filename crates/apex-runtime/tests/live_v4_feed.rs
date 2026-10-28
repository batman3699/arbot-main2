//! Task 8.5 R37: v4 events in the feed. PoolManager logs every v4 pool's
//! Swap and ModifyLiquidity, the pool's ID in topic 1: decoded to the pool's
//! key address, a Swap's amounts turned to the pool's side (v4 logs the
//! swapper's), and a subscription only ever with a pool-ID filter.

use alloy_primitives::{address, keccak256, Address, B256, U256};
use apex_chain::rpc::ws::{Notification, RawLog};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::feed::{decode_modify_liquidity, decode_v4_swap, v4_filter, Effect, FeedHandler, MODIFY_LIQUIDITY, V4_SWAP};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::v4::{key_address, V4Key, POOL_MANAGER};
use apex_types::ids::ChainId;
use apex_types::state::ReconstructionStatus;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EU256;

const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
const ID: B256 = B256::repeat_byte(0xc1);
const TICK: i32 = -289_960;
const L: u128 = 4_475_487_890_402_941_809;
const NOW: UnixNanos = UnixNanos(1_800_000_000_000_000_000);

fn word_i(v: i128) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_u(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

fn book() -> PoolBook {
    let key = V4Key { pool_id: ID, fee: 100, tick_spacing: 1, hooks: Address::ZERO };
    let snap = PoolSnapshot {
        spec: PoolSpec { pool: key_address(ID), venue: Venue::UniswapV4, token0: AXIS, token1: USDC, fee_ppm: 100, depth_usd: 1e6, v4: Some(key) },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(TICK).unwrap(),
            liquidity: L,
            tick: TICK,
            tick_spacing: 1,
            fee_ppm: 100,
            balance0: Some(EU256::exp10(30)),
            balance1: Some(EU256::exp10(15)),
        },
        ladder: TickLadder::new(vec![(-290_500, L as i128), (-289_500, -(L as i128))], -291_000, -289_000),
        decimals: (18, 6),
        factory: POOL_MANAGER,
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    };
    PoolBook::from_snapshots([snap], ReconstructionStatus::Verified)
}

/// PoolManager's Swap: the swapper's deltas (`amount0` negative: it paid),
/// then the pool's post-swap price, liquidity, tick and fee.
fn v4_swap(amount0: i128, amount1: i128, tick: i32, block: u64) -> RawLog {
    let mut data = Vec::new();
    data.extend(word_i(amount0));
    data.extend(word_i(amount1));
    data.extend(word_u(apex_types::compat::u256_to_alloy(get_sqrt_ratio_at_tick(tick).unwrap())));
    data.extend(word_i(L as i128 - 7));
    data.extend(word_i(i128::from(tick)));
    data.extend(word_i(100));
    RawLog {
        pending: false,
        removed: false,
        address: POOL_MANAGER,
        topics: vec![V4_SWAP, ID, B256::repeat_byte(9)],
        data,
        block_number: Some(block),
        transaction_hash: Some(keccak256(block.to_be_bytes())),
        log_index: Some(3),
        transaction_index: Some(1),
    }
}

/// PoolManager's ModifyLiquidity: tickLower, tickUpper, an `int256` delta,
/// the salt.
fn modify(lower: i32, upper: i32, delta: [u8; 32], block: u64) -> RawLog {
    let mut data = Vec::new();
    data.extend(word_i(i128::from(lower)));
    data.extend(word_i(i128::from(upper)));
    data.extend(delta);
    data.extend([0u8; 32]);
    RawLog {
        pending: false,
        removed: false,
        address: POOL_MANAGER,
        topics: vec![MODIFY_LIQUIDITY, ID, B256::repeat_byte(9)],
        data,
        block_number: Some(block),
        transaction_hash: Some(keccak256(block.to_be_bytes())),
        log_index: Some(5),
        transaction_index: Some(2),
    }
}

#[test]
fn a_v4_swap_s_amounts_are_the_pool_s_side() {
    let s = decode_v4_swap(&v4_swap(-1_000, 250, TICK - 3, 101)).expect("decodes");
    assert_eq!(s.pool, key_address(ID), "keyed by the pool, not PoolManager");
    assert_eq!((s.amount0, s.amount1), (1_000, -250), "the swapper paid 1,000: the pool received it");
    assert_eq!((s.tick, s.liquidity), (TICK - 3, L - 7));
    assert_eq!(decode_v4_swap(&RawLog { topics: vec![V4_SWAP], ..v4_swap(-1, 1, TICK, 101) }), None, "no pool ID");
}

#[test]
fn a_v4_swap_moves_its_pool_in_the_book() {
    let b = book();
    let mut h = FeedHandler::new(ChainId(8453));
    let mut effects = h.handle(&b, Notification::Log(v4_swap(-1_000, 250, TICK - 3, 101)), NOW);
    effects.extend(h.flush(&b));
    assert!(effects.iter().any(|e| matches!(e, Effect::Event(_))), "{effects:?}");
    let p = b.get(key_address(ID)).unwrap();
    assert_eq!((p.state.tick, p.state.liquidity), (TICK - 3, L - 7));
}

#[test]
fn a_modify_liquidity_becomes_a_position_write() {
    let p = decode_modify_liquidity(&modify(-290_100, -289_800, word_i(5_000), 101)).expect("decodes");
    assert_eq!((p.pool, p.tick_lower, p.tick_upper, p.delta), (key_address(ID), -290_100, -289_800, 5_000));
    let b = book();
    let mut h = FeedHandler::new(ChainId(8453));
    h.handle(&b, Notification::Log(modify(-290_100, -289_800, word_i(5_000), 101)), NOW);
    h.flush(&b);
    assert_eq!(b.get(key_address(ID)).unwrap().state.liquidity, L + 5_000, "in range: the pool's liquidity moves");
}

#[test]
fn a_modify_liquidity_beyond_i128_reloads_the_pool() {
    let huge = word_u(U256::from(1) << 127);
    assert_eq!(decode_modify_liquidity(&modify(-290_100, -289_800, huge, 101)), None);
    let b = book();
    let mut h = FeedHandler::new(ChainId(8453));
    let effects = h.handle(&b, Notification::Log(modify(-290_100, -289_800, huge, 101)), NOW);
    assert_eq!(effects, vec![Effect::Reload(vec![key_address(ID)])]);
}

#[test]
fn a_removed_v4_log_reloads_its_pool() {
    let b = book();
    let mut h = FeedHandler::new(ChainId(8453));
    let gone = RawLog { removed: true, ..v4_swap(-1, 1, TICK, 101) };
    assert_eq!(h.handle(&b, Notification::Log(gone), NOW), vec![Effect::Reload(vec![key_address(ID)])]);
}

#[test]
fn no_pool_ids_no_v4_subscription() {
    assert_eq!(v4_filter(&[]), None);
    let f = v4_filter(&[ID]).unwrap();
    assert_eq!((f.addresses, f.topics0, f.topics1), (vec![POOL_MANAGER], vec![V4_SWAP, MODIFY_LIQUIDITY], vec![ID]));
}
