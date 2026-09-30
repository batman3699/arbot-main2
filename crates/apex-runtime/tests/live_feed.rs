//! Task 8.5 R3 — feed → book → events (`apex_runtime::live::feed`).
//!
//! The handler is pure over the book: notifications in, writes and effects out.
//! So every row of the module's table — a swap applied, its confirmed copy
//! refused, a ladder exit, a mint, a reorg, a gap — is driven here exactly.

use alloy_primitives::{address, keccak256, Address, B256, U256};
use apex_chain::rpc::ws::{Head, Notification, RawLog};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::feed::{self, Effect, FeedHandler, BURN, MINT, SWAP};
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_state::feed::event::EventKind;
use apex_types::ids::ChainId;
use apex_types::state::ReconstructionStatus;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EU256;

const BASE: ChainId = ChainId(8453);
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const NOW: UnixNanos = UnixNanos(1_700_000_000_000_000_000);
const TICK: i32 = -197_350;

fn book() -> PoolBook {
    let snap = PoolSnapshot {
        spec: PoolSpec {
            pool: POOL,
            venue: Venue::UniswapV3,
            token0: WETH,
            token1: USDC,
            fee_ppm: 500,
            depth_usd: 5_800_000.0,
        },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(TICK).unwrap(),
            liquidity: 1_400_000_000_000_000_000,
            tick: TICK,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: Some(EU256::exp10(21)),
            balance1: Some(EU256::from(4_600_000_000_000u64)),
        },
        ladder: TickLadder::new(vec![(-199_000, 1), (-196_000, -1)], -200_000, -195_000),
        decimals: (18, 6),
        factory: Venue::UniswapV3.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        seq: 0,
    };
    PoolBook::from_snapshots([snap], ReconstructionStatus::Verified)
}

fn word_i(v: i128) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

/// A swap log with the pool's post-swap state: `amount0` WETH in (positive: the
/// pool received it), `amount1` USDC out.
fn swap(tick: i32, block: u64, index: u64, pending: bool, weth_in: i128, usdc_out: i128) -> RawLog {
    let mut data = Vec::new();
    data.extend(word_i(weth_in));
    data.extend(word_i(-usdc_out));
    let sqrt = get_sqrt_ratio_at_tick(tick).unwrap();
    let mut s = [0u8; 32];
    sqrt.to_big_endian(&mut s);
    data.extend(s);
    data.extend(word_i(1_399_000_000_000_000_000));
    data.extend(word_i(i128::from(tick)));
    RawLog {
        pending,
        removed: false,
        address: POOL,
        topics: vec![SWAP, B256::repeat_byte(1), B256::repeat_byte(2)],
        data,
        block_number: Some(block),
        transaction_hash: Some(keccak256(block.to_be_bytes())),
        log_index: Some(index),
    }
}

fn event_of(effects: &[Effect]) -> &apex_state::feed::event::StateEvent {
    match effects {
        [Effect::Event(e)] => e,
        other => panic!("expected one event, got {other:?}"),
    }
}

#[test]
fn a_swap_decodes_its_post_swap_state() {
    let s = feed::decode_swap(&swap(-197_360, 101, 7, true, 5 * 10i128.pow(18), 13_450_000_000)).unwrap();
    assert_eq!(s.tick, -197_360);
    assert_eq!(s.amount0, 5 * 10i128.pow(18));
    assert_eq!(s.amount1, -13_450_000_000);
    assert_eq!(s.liquidity, 1_399_000_000_000_000_000);
    assert_eq!((s.block, s.log_index), (101, 7));
    assert!(s.pending);

    // A log that cannot be ordered cannot be applied.
    let mut unordered = swap(-197_360, 101, 7, true, 1, 1);
    unordered.log_index = None;
    assert!(feed::decode_swap(&unordered).is_none());
}

/// **The capture path's event.** A preconfirmed swap is applied to the book and
/// becomes one `PendingSwap` on the pool it moved, sized in USD from the USDC
/// side, at the swap's own ordinal.
#[test]
fn a_new_swap_moves_the_book_and_becomes_an_event() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let effects = h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 5 * 10i128.pow(18), 13_450_000_000)), NOW);
    let e = event_of(&effects);

    assert_eq!(book.get(POOL).unwrap().state.tick, -197_360, "the book moved");
    assert_eq!((e.at.block, e.at.log_index), (101, 7));
    assert_eq!(e.fingerprint.preconf_sequence, Some(101), "marked preconfirmed");
    match &e.kind {
        EventKind::PendingSwap { pools, notional_usd, .. } => {
            assert_eq!(pools[0].address, POOL);
            let n = notional_usd.expect("the USDC side prices it");
            assert!((13_000.0..14_000.0).contains(&n), "notional {n}");
        }
        other => panic!("expected a PendingSwap, got {other:?}"),
    }
}

/// **One swap, two copies.** The confirmed copy of a swap already applied
/// preconfirmed is refused by the book and produces no second event — and if it
/// did, it would carry the same ordinal for the plane to catch.
#[test]
fn the_confirmed_copy_of_an_applied_swap_is_not_a_second_event() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let pending = h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 10i128.pow(18), 2_690_000_000)), NOW);
    let first = event_of(&pending).at;
    let confirmed = h.handle(&book, Notification::Log(swap(-197_360, 101, 7, false, 10i128.pow(18), 2_690_000_000)), NOW);
    assert!(confirmed.is_empty(), "{confirmed:?}");

    // An older swap arriving late is refused the same way: it would roll the
    // pool back.
    assert!(h.handle(&book, Notification::Log(swap(-197_000, 101, 3, false, 1, 1)), NOW).is_empty());
    assert_eq!(book.get(POOL).unwrap().state.tick, -197_360);
    assert_eq!(first.block, 101);
}

/// A swap that carries the price off the ladder asks for a reload and publishes
/// nothing: the pool cannot be priced until it is re-read.
#[test]
fn a_swap_off_the_ladder_reloads_and_publishes_nothing() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let effects = h.handle(&book, Notification::Log(swap(-150_000, 102, 0, true, 1, 1)), NOW);
    assert_eq!(effects, vec![Effect::Reload(vec![POOL])]);
}

/// A mint or burn changes a range's liquidity, which the ladder carries: reload.
#[test]
fn liquidity_events_reload_the_pool() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    for topic in [MINT, BURN] {
        let log = RawLog { topics: vec![topic], ..swap(TICK, 103, 0, false, 1, 1) };
        assert_eq!(h.handle(&book, Notification::Log(log), NOW), vec![Effect::Reload(vec![POOL])]);
    }
}

/// A reorg retracts a log. What was applied may not have happened: reload.
#[test]
fn a_removed_log_reloads_the_pool() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let removed = RawLog { removed: true, ..swap(-197_360, 104, 0, false, 1, 1) };
    assert_eq!(h.handle(&book, Notification::Log(removed), NOW), vec![Effect::Reload(vec![POOL])]);
    assert_eq!(book.get(POOL).unwrap().state.tick, TICK, "the retracted swap was not applied");
}

/// A gap makes the whole book `Rebuilding` until it is re-read (§5.6, INV-08).
#[test]
fn a_gap_rebuilds_the_book() {
    for n in [
        Notification::Reconnected { outage: std::time::Duration::from_secs(3), attempt: 1 },
        Notification::Gap { dropped: 12 },
    ] {
        let book = book();
        let mut h = FeedHandler::new(BASE);
        assert_eq!(h.handle(&book, n, NOW), vec![Effect::FullReload]);
        assert_eq!(book.status(), ReconstructionStatus::Rebuilding);
    }
}

/// Heads are remembered for the next fingerprint and publish nothing.
#[test]
fn a_head_is_remembered_not_published() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let head = Head {
        number: 105,
        hash: B256::repeat_byte(9),
        timestamp: 1,
        base_fee_per_gas: Some(6_000_000),
        gas_used: 1,
        gas_limit: 2,
    };
    assert!(h.handle(&book, Notification::Head(head.clone()), NOW).is_empty());
    let effects = h.handle(&book, Notification::Log(swap(-197_360, 106, 0, true, 1, 1)), NOW);
    let e = event_of(&effects);
    assert_eq!(e.fingerprint.parent_block_hash, head.hash);
    assert_eq!(e.fingerprint.confirmed_block_number, 105);
}

/// WETH is priced through the book's own WETH/USDC pool: tick −197,350 is about
/// $2,690.
#[test]
fn prices_come_from_the_book() {
    let prices = feed::usd_prices(&book());
    assert_eq!(prices[&USDC], 1.0);
    let weth = prices[&WETH];
    assert!((2_600.0..2_800.0).contains(&weth), "WETH at {weth}");
}

/// Two different swaps never share a fingerprint, however their other fields
/// line up — the delta hash identifies the change itself.
#[test]
fn different_swaps_have_different_fingerprints() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let a = h.handle(&book, Notification::Log(swap(-197_360, 110, 0, true, 1, 1)), NOW);
    let b = h.handle(&book, Notification::Log(swap(-197_370, 110, 1, true, 1, 1)), NOW);
    assert_ne!(event_of(&a).fingerprint.hash(), event_of(&b).fingerprint.hash());
    let _ = U256::ZERO;
}

/// A `Swap` is exactly five words. A log under its topic with any other shape
/// is not one, and is not decoded as far as it happens to go.
#[test]
fn a_log_of_the_wrong_shape_is_not_a_swap() {
    let mut long = swap(-197_360, 101, 7, true, 1, 1);
    long.data.extend([0u8; 32]);
    assert!(feed::decode_swap(&long).is_none());
    let mut short = swap(-197_360, 101, 7, true, 1, 1);
    short.data.truncate(128);
    assert!(feed::decode_swap(&short).is_none());
}

/// A swap first seen confirmed — the preconfirmed feed missed it — is new, and
/// its event is not marked preconfirmed.
#[test]
fn a_swap_first_seen_confirmed_is_not_marked_preconfirmed() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let effects = h.handle(&book, Notification::Log(swap(-197_360, 101, 7, false, 1, 1)), NOW);
    assert_eq!(event_of(&effects).fingerprint.preconf_sequence, None);
}

/// A token quoted only against WETH is priced through WETH, even when its pool
/// is the deeper one and so is met before WETH has a price.
#[test]
fn a_token_two_pools_from_a_stable_is_priced_in_any_order() {
    const X: Address = address!("1000000000000000000000000000000000000001");
    let weth_usdc = (*book().get(POOL).unwrap()).clone();
    let mut x_weth = weth_usdc.clone();
    x_weth.spec.pool = address!("2000000000000000000000000000000000000002");
    (x_weth.spec.token0, x_weth.spec.token1) = (X, WETH);
    x_weth.spec.depth_usd = 10_000_000.0; // deeper than WETH/USDC
    x_weth.decimals = (18, 18);
    // Half a WETH per X: 1.0001^-6932 ≈ 0.5.
    x_weth.state.sqrt_price_x96 = get_sqrt_ratio_at_tick(-6_932).unwrap();
    let book = PoolBook::from_snapshots([weth_usdc, x_weth], ReconstructionStatus::Verified);

    let prices = feed::usd_prices(&book);
    let x = prices.get(&X).copied().expect("X is two pools from USDC");
    assert!((1_300.0..1_400.0).contains(&x), "X at {x}, WETH at {}", prices[&WETH]);
}
