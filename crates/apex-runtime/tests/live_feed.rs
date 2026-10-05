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
use apex_runtime::live::feed::{self, Effect, FeedHandler, BURN, MINT, PANCAKE_SWAP, SWAP};
use apex_runtime::live::sim::BlockContext;
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_state::feed::event::EventKind;
use apex_types::ids::ChainId;
use apex_types::state::ReconstructionStatus;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EU256;
use std::time::Duration;

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
        dynamic_fee: None,
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

/// One notification, and then the quiet that ends its burst.
fn settle(h: &mut FeedHandler, book: &PoolBook, n: Notification) -> Vec<Effect> {
    let mut effects = h.handle(book, n, NOW);
    effects.extend(h.flush(book));
    effects
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

/// **The capture path's event.** A preconfirmed swap is held until its burst
/// is over, then applied to the book and published as one `PendingSwap` on the
/// pool it moved, sized in USD from the USDC side, at the swap's own ordinal.
#[test]
fn a_new_swap_moves_the_book_and_becomes_an_event() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let held = h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 5 * 10i128.pow(18), 13_450_000_000)), NOW);
    assert!(held.is_empty(), "{held:?}");
    assert_eq!(book.get(POOL).unwrap().state.tick, TICK, "held, not applied");
    let effects = h.flush(&book);
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
    let pending = settle(&mut h, &book, Notification::Log(swap(-197_360, 101, 7, true, 10i128.pow(18), 2_690_000_000)));
    let first = event_of(&pending).at;
    let confirmed = settle(&mut h, &book, Notification::Log(swap(-197_360, 101, 7, false, 10i128.pow(18), 2_690_000_000)));
    assert!(confirmed.is_empty(), "{confirmed:?}");

    // An older swap arriving late is refused the same way: it would roll the
    // pool back.
    assert!(settle(&mut h, &book, Notification::Log(swap(-197_000, 101, 3, false, 1, 1))).is_empty());
    assert_eq!(book.get(POOL).unwrap().state.tick, -197_360);
    assert_eq!(first.block, 101);
}

/// A swap that carries the price off the ladder asks for a reload and publishes
/// nothing: the pool cannot be priced until it is re-read.
#[test]
fn a_swap_off_the_ladder_reloads_and_publishes_nothing() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let effects = settle(&mut h, &book, Notification::Log(swap(-150_000, 102, 0, true, 1, 1)));
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
    let effects = settle(&mut h, &book, Notification::Log(swap(-197_360, 106, 0, true, 1, 1)));
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

/// Two different bursts never share a fingerprint, however their other fields
/// line up — the delta hash identifies the change itself.
#[test]
fn different_swaps_have_different_fingerprints() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let a = settle(&mut h, &book, Notification::Log(swap(-197_360, 110, 0, true, 1, 1)));
    let b = settle(&mut h, &book, Notification::Log(swap(-197_370, 110, 1, true, 1, 1)));
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
    let effects = settle(&mut h, &book, Notification::Log(swap(-197_360, 101, 7, false, 1, 1)));
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

// ------------------------------------------------------------------ PancakeSwap

const CAKE_POOL: Address = address!("72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38");

/// A `Swap` PancakeSwap's 1 bp WETH/USDC pool emitted on Base: block 52,109,364,
/// log index 393. Seven words — Uniswap's five, then the protocol fees.
fn cake_log() -> RawLog {
    let party = B256::left_padding_from(address!("8f10b468b06c6fd214b65f87778827f7d113f996").as_slice());
    RawLog {
        pending: false,
        removed: false,
        address: CAKE_POOL,
        topics: vec![PANCAKE_SWAP, party, party],
        data: alloy_primitives::hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000068584968155a2f",
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffb50e363",
            "0000000000000000000000000000000000000000000363df4215b46b048caa6e",
            "000000000000000000000000000000000000000000000000136684f6d608b4b3",
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffcfce8",
            "000000000000000000000000000000000000000000000000000000e1aa578cf8",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap(),
        block_number: Some(52_109_364),
        transaction_hash: Some(alloy_primitives::b256!(
            "87676ded616a1ff7066ea0c4450c12bc2262a85990f30dc3d1f5fe940d7d0e22"
        )),
        log_index: Some(393),
    }
}

#[test]
fn pancakeswaps_swap_topic_is_its_signatures_hash() {
    let sig = "Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)";
    assert_eq!(PANCAKE_SWAP, keccak256(sig.as_bytes()));
    assert_eq!(feed::swap_topic(Venue::PancakeV3), PANCAKE_SWAP);
    assert_eq!(feed::swap_topic(Venue::UniswapV3), SWAP);
    assert_eq!(feed::swap_topic(Venue::Slipstream), SWAP);
}

/// **As Base recorded it**, against `cast abi-decode` of the same data: the
/// post-swap state is the first five words, as for Uniswap.
#[test]
fn a_pancakeswap_swap_decodes_as_base_recorded_it() {
    let s = feed::decode_swap(&cake_log()).unwrap();
    assert_eq!(s.pool, CAKE_POOL);
    assert_eq!(s.amount0, 29_370_469_879_994_927);
    assert_eq!(s.amount1, -78_584_989);
    assert_eq!(s.sqrt_price_x96, U256::from(4_098_410_126_486_972_375_280_238u128));
    assert_eq!(s.liquidity, 1_397_950_930_032_833_715);
    assert_eq!(s.tick, -197_400);
    assert_eq!((s.block, s.log_index), (52_109_364, 393));
}

/// Each topic is held to its own length: PancakeSwap's at five or eight words
/// is not a swap, and neither is Uniswap's at seven.
#[test]
fn each_swap_topic_is_held_to_its_own_shape() {
    let mut five = cake_log();
    five.data.truncate(160);
    assert!(feed::decode_swap(&five).is_none());
    let mut eight = cake_log();
    eight.data.extend([0u8; 32]);
    assert!(feed::decode_swap(&eight).is_none());
    let mut uniswap_seven = cake_log();
    uniswap_seven.topics[0] = SWAP;
    assert!(feed::decode_swap(&uniswap_seven).is_none());
}

/// The handler applies a PancakeSwap swap like any other and publishes it.
#[test]
fn a_pancakeswap_swap_moves_the_book_and_becomes_an_event() {
    let mut snap = (*book().get(POOL).unwrap()).clone();
    snap.spec.pool = CAKE_POOL;
    snap.spec.venue = Venue::PancakeV3;
    snap.factory = Venue::PancakeV3.factory();
    let book = PoolBook::from_snapshots([snap], ReconstructionStatus::Verified);
    let effects = settle(&mut FeedHandler::new(BASE), &book, Notification::Log(cake_log()));
    let e = event_of(&effects);
    assert_eq!(book.get(CAKE_POOL).unwrap().state.tick, -197_400, "the book moved");
    assert_eq!((e.at.block, e.at.log_index), (52_109_364, 393));
}

// ------------------------------------------------- R17: a flashblock is one state

const POOL2: Address = address!("b4cB800910B228ED3d0834cF79D697127BBB00e5");

/// `POOL` and a second WETH/USDC pool, both at `TICK`.
fn two_pools() -> PoolBook {
    let one = (*book().get(POOL).unwrap()).clone();
    let mut two = one.clone();
    two.spec.pool = POOL2;
    PoolBook::from_snapshots([one, two], ReconstructionStatus::Verified)
}

/// `l` as emitted by `pool`, in its own transaction.
fn by(l: RawLog, pool: Address, tx: u8) -> RawLog {
    RawLog { address: pool, transaction_hash: Some(B256::repeat_byte(tx)), ..l }
}

fn pools_of(e: &apex_state::feed::event::StateEvent) -> Vec<Address> {
    match &e.kind {
        EventKind::PendingSwap { pools, .. } => pools.iter().map(|p| p.address).collect(),
        other => panic!("expected a PendingSwap, got {other:?}"),
    }
}

fn notional_of(e: &apex_state::feed::event::StateEvent) -> Option<f64> {
    match &e.kind {
        EventKind::PendingSwap { notional_usd, .. } => *notional_usd,
        other => panic!("expected a PendingSwap, got {other:?}"),
    }
}

/// **A flashblock is one state.** Two swaps of one flashblock — the shape of
/// block 52,184,616's, where a sale into one pool was priced before the same
/// flashblock's sale into the other and a route paid between them that paid
/// nowhere else — are held, so a pricer reading the book between them sees
/// neither. The burst is then applied in one write and published as one event
/// naming both pools, at the last swap's ordinal, as large as its largest swap.
#[test]
fn a_flashblock_is_one_state() {
    let book = two_pools();
    let mut h = FeedHandler::new(BASE);
    let first = by(swap(-197_360, 101, 7, true, 5 * 10i128.pow(18), 13_450_000_000), POOL, 1);
    let second = by(swap(-197_355, 101, 9, true, 10i128.pow(18), 2_690_000_000), POOL2, 2);

    assert!(h.handle(&book, Notification::Log(first), NOW).is_empty());
    assert_eq!(book.get(POOL).unwrap().state.tick, TICK, "a pricer between the two sees neither");
    assert!(h.handle(&book, Notification::Log(second), NOW).is_empty());
    assert_eq!(book.get(POOL2).unwrap().state.tick, TICK);
    assert!(h.holding());

    let effects = h.flush(&book);
    let e = event_of(&effects);
    let (a, b) = (book.get(POOL).unwrap(), book.get(POOL2).unwrap());
    assert_eq!((a.state.tick, b.state.tick), (-197_360, -197_355));
    assert_eq!(a.seq, b.seq, "applied in one write");
    assert_eq!((e.at.block, e.at.log_index), (101, 9), "the state after the last swap");
    assert_eq!(pools_of(e), vec![POOL, POOL2]);
    match &e.kind {
        EventKind::PendingSwap { target, .. } => assert_eq!(*target, B256::repeat_byte(2)),
        other => panic!("expected a PendingSwap, got {other:?}"),
    }
    let n = notional_of(e).expect("both sides priced");
    assert!((13_000.0..14_000.0).contains(&n), "the largest swap's notional, {n}");

    assert!(!h.holding());
    assert!(h.flush(&book).is_empty(), "a burst is published once");
}

/// A swap from a later block than a held preconfirmed one ends the burst at
/// once: that block began after the burst's flashblock was published — and a
/// confirmed one too, when the preconfirmed feed missed its copy.
#[test]
fn a_swap_from_a_later_block_ends_the_burst() {
    for pending in [true, false] {
        let book = two_pools();
        let mut h = FeedHandler::new(BASE);
        assert!(h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 1, 1)), NOW).is_empty());
        let effects = h.handle(&book, Notification::Log(by(swap(-197_355, 102, 0, pending, 1, 1), POOL2, 3)), NOW);
        let e = event_of(&effects);
        assert_eq!((e.at.block, e.at.log_index), (101, 7), "block 101's burst, published");
        assert_eq!(book.get(POOL2).unwrap().state.tick, TICK, "block 102's swap held");
        assert!(h.holding());
    }
}

/// A confirmed copy never ends a burst: it trails by 0.5–2 s and lands in the
/// middle of later bursts. Held with one, it is refused as not newer and is no
/// part of the burst's event — not its size, and not its ordinal, which would
/// be the copy's own earlier event's and be dropped as a redelivery.
#[test]
fn a_confirmed_log_does_not_end_a_burst() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    assert!(h.handle(&book, Notification::Log(swap(-197_360, 102, 3, true, 1, 1)), NOW).is_empty());
    let late_copy = swap(-197_000, 101, 7, false, 5 * 10i128.pow(18), 13_450_000_000);
    assert!(h.handle(&book, Notification::Log(late_copy), NOW).is_empty());
    let effects = h.flush(&book);
    let e = event_of(&effects);
    assert_eq!((e.at.block, e.at.log_index), (102, 3));
    assert!(notional_of(e).expect("priced") < 1.0, "the copy is not part of the burst");
    assert_eq!(book.get(POOL).unwrap().state.tick, -197_360, "the older copy did not roll it back");

    // Nor does it date the burst: the next swap of the same flashblock joins.
    assert!(h.handle(&book, Notification::Log(swap(-197_361, 103, 1, true, 1, 1)), NOW).is_empty());
    assert!(h.handle(&book, Notification::Log(swap(-197_360, 102, 3, false, 1, 1)), NOW).is_empty());
    assert!(h.handle(&book, Notification::Log(swap(-197_362, 103, 2, true, 1, 1)), NOW).is_empty());
    let effects = h.flush(&book);
    assert_eq!((event_of(&effects).at.block, event_of(&effects).at.log_index), (103, 2));
    assert_eq!(book.get(POOL).unwrap().state.tick, -197_362);
}

/// Neither a head nor a liquidity event ends a burst: neither moves a price
/// the burst's event carries.
#[test]
fn heads_and_liquidity_events_do_not_end_a_burst() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    assert!(h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 1, 1)), NOW).is_empty());
    let head = Head { number: 100, hash: B256::repeat_byte(9), timestamp: 1, base_fee_per_gas: None, gas_used: 1, gas_limit: 2 };
    assert!(h.handle(&book, Notification::Head(head), NOW).is_empty());
    let mint = RawLog { topics: vec![MINT], ..swap(TICK, 101, 8, true, 1, 1) };
    assert_eq!(h.handle(&book, Notification::Log(mint), NOW), vec![Effect::Reload(vec![POOL])]);
    assert!(h.holding());
    assert_eq!(event_of(&h.flush(&book)).at.log_index, 7);
}

/// A gap ends a burst without publishing it: what was held happened, so the
/// book takes it, and nothing is priced until the book is read again.
#[test]
fn a_gap_applies_what_was_held_and_publishes_nothing() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    assert!(h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 1, 1)), NOW).is_empty());
    assert_eq!(h.handle(&book, Notification::Gap { dropped: 3 }, NOW), vec![Effect::FullReload]);
    assert_eq!(book.get(POOL).unwrap().state.tick, -197_360);
    assert_eq!(book.status(), ReconstructionStatus::Rebuilding);
    assert!(!h.holding());
    assert!(h.flush(&book).is_empty());
}

/// The burst is over `BURST_QUIET` after its latest swap — not its first, and
/// not moved by anything that is not a swap.
#[test]
fn the_quiet_deadline_follows_the_latest_swap() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let ms = |n: u64| UnixNanos(NOW.0 + n * 1_000_000);
    assert_eq!(feed::BURST_QUIET, Duration::from_millis(20));
    assert_eq!(h.quiet_deadline(), None);
    h.handle(&book, Notification::Log(swap(-197_360, 101, 7, true, 1, 1)), ms(0));
    assert_eq!(h.quiet_deadline(), Some(ms(20)));
    let head = Head { number: 100, hash: B256::repeat_byte(9), timestamp: 1, base_fee_per_gas: None, gas_used: 1, gas_limit: 2 };
    h.handle(&book, Notification::Head(head), ms(5));
    assert_eq!(h.quiet_deadline(), Some(ms(20)));
    h.handle(&book, Notification::Log(swap(-197_361, 101, 9, true, 1, 1)), ms(8));
    assert_eq!(h.quiet_deadline(), Some(ms(28)));
    let effects = h.flush(&book);
    assert_eq!(event_of(&effects).observed_at, ms(8), "observed when its last swap arrived");
    assert_eq!(h.quiet_deadline(), None);
}

/// A pool whose last swap in the burst left its ladder is reloaded and left out
/// of the event; one a later swap brought back is in it, every swap applied to
/// it covered.
#[test]
fn a_pool_off_its_ladder_is_reloaded_not_published() {
    let book = two_pools();
    let mut h = FeedHandler::new(BASE);
    h.handle(&book, Notification::Log(swap(-150_000, 101, 1, true, 1, 1)), NOW);
    h.handle(&book, Notification::Log(by(swap(-197_355, 101, 2, true, 1, 1), POOL2, 2)), NOW);
    let effects = h.flush(&book);
    assert_eq!(effects[0], Effect::Reload(vec![POOL]));
    assert_eq!(pools_of(event_of(&effects[1..])), vec![POOL2]);

    // Off, then back on: priceable again, and still re-read.
    h.handle(&book, Notification::Log(swap(-150_000, 102, 1, true, 10i128.pow(18), 2_690_000_000)), NOW);
    h.handle(&book, Notification::Log(swap(-197_365, 102, 2, true, 1, 1)), NOW);
    let effects = h.flush(&book);
    assert_eq!(effects[0], Effect::Reload(vec![POOL]));
    let e = event_of(&effects[1..]);
    assert_eq!(pools_of(e), vec![POOL]);
    assert_eq!((e.at.block, e.at.log_index), (102, 2));
    let n = notional_of(e).expect("priced");
    assert!(n > 2_000.0, "the swap that left the ladder is part of the burst: {n}");

    // On, then off: its last word is off the ladder, so nothing to price.
    h.handle(&book, Notification::Log(swap(-197_366, 103, 0, true, 1, 1)), NOW);
    h.handle(&book, Notification::Log(swap(-150_000, 103, 1, true, 1, 1)), NOW);
    assert_eq!(h.flush(&book), vec![Effect::Reload(vec![POOL])]);
}

/// One swap that cannot be sized makes its burst unmeasured — which Engine D
/// admits rather than skips.
#[test]
fn a_swap_that_cannot_be_sized_makes_the_burst_unmeasured() {
    const X: Address = address!("1000000000000000000000000000000000000001");
    const Y: Address = address!("1000000000000000000000000000000000000002");
    const XY: Address = address!("3000000000000000000000000000000000000003");
    let weth_usdc = (*book().get(POOL).unwrap()).clone();
    let mut xy = weth_usdc.clone();
    xy.spec.pool = XY;
    (xy.spec.token0, xy.spec.token1) = (X, Y);
    let book = PoolBook::from_snapshots([weth_usdc, xy], ReconstructionStatus::Verified);

    let mut h = FeedHandler::new(BASE);
    h.handle(&book, Notification::Log(swap(-197_360, 101, 1, true, 10i128.pow(18), 2_690_000_000)), NOW);
    h.handle(&book, Notification::Log(by(swap(-197_355, 101, 2, true, 1, 1), XY, 2)), NOW);
    let effects = h.flush(&book);
    assert_eq!(pools_of(event_of(&effects)), vec![POOL, XY]);
    assert_eq!(notional_of(event_of(&effects)), None);
}

// ------------------------------------------- R18: the block being built, from the feed

fn head_at(number: u64, timestamp: u64) -> Head {
    Head { number, hash: B256::repeat_byte(9), timestamp, base_fee_per_gas: Some(5_000_000), gas_used: 1, gas_limit: 2 }
}

/// **The block being built comes from the feed.** Tier 2 sets the context of
/// the block a trade would land in, and read it from BlockPI before every
/// simulation — a round trip as long as the simulation's own. The feed already
/// has it: the newest sealed head, one block on, two seconds later — or the
/// newest block a preconfirmed log came from, when the head lags behind the
/// flashblocks, as BlockPI's did one time in five (measured 2026-10-06).
#[test]
fn the_block_being_built_follows_the_feed() {
    let book = book();
    let mut h = FeedHandler::new(BASE);
    let at = |number, timestamp| Some(BlockContext { number, timestamp, base_fee: 5_000_000 });
    assert_eq!(h.block_being_built(), None, "no head, no block");

    h.handle(&book, Notification::Head(head_at(100, 1_000)), NOW);
    assert_eq!(h.block_being_built(), at(101, 1_002));

    // A preconfirmed log from the block after: the head lags.
    h.handle(&book, Notification::Log(swap(-197_360, 102, 0, true, 1, 1)), NOW);
    assert_eq!(h.block_being_built(), at(102, 1_004));

    // A confirmed log never dates it, and an older preconfirmed one never
    // moves it back.
    h.handle(&book, Notification::Log(swap(-197_360, 105, 0, false, 1, 1)), NOW);
    h.handle(&book, Notification::Log(swap(-197_360, 101, 3, true, 1, 1)), NOW);
    assert_eq!(h.block_being_built(), at(102, 1_004));

    // Any preconfirmed log dates it, a mint's as well as a swap's.
    let mint = RawLog { topics: vec![MINT], ..swap(TICK, 103, 0, true, 1, 1) };
    h.handle(&book, Notification::Log(mint), NOW);
    assert_eq!(h.block_being_built(), at(103, 1_006));

    // A newer head.
    h.handle(&book, Notification::Head(head_at(103, 1_006)), NOW);
    assert_eq!(h.block_being_built(), at(104, 1_008));

    // A head without a base fee is no context to simulate in.
    h.handle(&book, Notification::Head(Head { base_fee_per_gas: None, ..head_at(104, 1_008) }), NOW);
    assert_eq!(h.block_being_built(), None);
}
