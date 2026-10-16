//! Task 8.5 R28.1: `Mint` and `Burn` applied to the book in place
//! (`PoolBook::apply_writes` with `StateWrite::Liquidity`), instead of a ladder
//! reload for every one: about 1,300 reloads an hour on the canary, most of its
//! BlockPI HTTP traffic.
//!
//! What the book keeps:
//! - **The ladder takes every change, in any order.** Net and gross only add,
//!   so a log that arrives late is as exact as one that arrives first.
//! - **Active liquidity is a swap's to state.** A swap's post-state liquidity
//!   already counts every change before it, so a change applies to active
//!   liquidity only when no later swap has been applied, and a swap applied
//!   after a later change (its preconfirmed copy was missed) adds that change
//!   back.
//! - **A swap alone moves `last_log` and `block`.** A liquidity log that moved
//!   them would make the confirmed copy of a swap from before it look stale,
//!   and the pool's price would never take that swap.
//! - **One log, applied once.** Its preconfirmed copy and confirmed copy share a
//!   position; one the pool's read already holds is stale.
//! - **What cannot be applied exactly is read again**, and the pool stays
//!   flagged until a read at or after the change.

use alloy_primitives::{address, Address, B256, U256};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{LiquidityWrite, PoolBook, PoolSnapshot, StateWrite, SwapApplied, SwapWrite};
use apex_runtime::live::cp::Reserves;
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::state::ReconstructionStatus;
use ethers_core::types::U256 as EU256;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const CP: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
const TICK: i32 = -197_350;
const L: u128 = 1_400_000_000_000_000_000;

/// One position of 7 over `[-199_000, -196_000)`, read with gross at block 100.
fn snapshot() -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: POOL, venue: Venue::UniswapV3, token0: WETH, token1: USDC, fee_ppm: 500, depth_usd: 5_800_000.0 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(TICK).unwrap(),
            liquidity: L,
            tick: TICK,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::with_gross(vec![(-199_000, 7, 7), (-196_000, 7, -7)], -200_000, -195_000),
        decimals: (18, 6),
        factory: Venue::UniswapV3.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

fn book_of(pool: PoolSnapshot) -> PoolBook {
    let cp = PoolSnapshot {
        spec: PoolSpec { pool: CP, venue: Venue::AerodromeV2, ..snapshot().spec },
        reserves: Some(Reserves { reserve0: EU256::exp10(21), reserve1: EU256::exp10(12) }),
        ..snapshot()
    };
    PoolBook::from_snapshots([pool, cp], ReconstructionStatus::Verified)
}

fn book() -> PoolBook {
    book_of(snapshot())
}

fn liq(pool: Address, lower: i32, upper: i32, delta: i128, at: (u64, u64)) -> StateWrite {
    StateWrite::Liquidity(LiquidityWrite { pool, tick_lower: lower, tick_upper: upper, delta, at })
}

fn swap(liquidity: u128, tick: i32, at: (u64, u64)) -> StateWrite {
    StateWrite::Swap(SwapWrite {
        pool: POOL,
        sqrt_price_x96: U256::from_limbs(get_sqrt_ratio_at_tick(tick).unwrap().0),
        liquidity,
        tick,
        at,
    })
}

#[test]
fn a_mint_in_range_raises_the_active_liquidity_and_the_ladder() {
    let book = book();
    let before = book.versions_for(&[POOL]);
    assert_eq!(book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 0))]), vec![SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L + 1_000);
    assert_eq!(p.ladder.ticks(), &[(-199_000, 7), (-198_000, 1_000), (-197_000, -1_000), (-196_000, -7)]);
    assert_eq!(p.ladder.gross(), Some(&[7u128, 1_000, 1_000, 7][..]));
    assert_ne!(book.versions_for(&[POOL]), before, "a quote over the pool changed: its version must move");
    assert_eq!((p.last_log, p.block), (None, 100), "a swap's to move, not a position change's");
    assert_eq!(book.position_counts(), (1, 0));
}

/// `tickLower <= tick < tickUpper` is v3-core's active range: a position whose
/// upper tick is the current tick is not active.
#[test]
fn a_mint_out_of_range_leaves_the_active_liquidity() {
    let book = book();
    book.apply_writes(&[liq(POOL, -196_500, -196_000, 50, (101, 0))]);
    book.apply_writes(&[liq(POOL, -198_000, TICK, 60, (101, 1))]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L, "neither position is active at the current tick");
    assert_eq!(p.ladder.ticks(), &[(-199_000, 7), (-198_000, 60), (TICK, -60), (-196_500, 50), (-196_000, -57)]);
}

/// A position whose lower tick is the current tick is active: the range's lower
/// end is inclusive.
#[test]
fn a_mint_from_the_current_tick_is_active() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, TICK, -197_000, 25, (101, 0))]), vec![SwapApplied::Updated]);
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L + 25);
}

/// A position active at the current tick changes active liquidity even where
/// its ticks lie beyond the ladder's proven range.
#[test]
fn a_wide_mint_beyond_the_ladder_still_changes_the_active_liquidity() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, -887_270, 887_270, 40, (101, 0))]), vec![SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L + 40);
    assert_eq!(p.ladder.ticks(), snapshot().ladder.ticks(), "neither tick is the ladder's");
}

#[test]
fn a_burn_in_range_lowers_the_active_liquidity() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, -199_000, -196_000, -3, (101, 0))]), vec![SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L - 3);
    assert_eq!(p.ladder.ticks(), &[(-199_000, 4), (-196_000, -4)]);
}

/// With gross, the burn that empties both ticks removes them: exact, no read.
#[test]
fn a_burn_that_empties_its_ticks_removes_them() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, -199_000, -196_000, -7, (101, 0))]), vec![SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L - 7);
    assert!(p.ladder.ticks().is_empty());
    assert!(book.positions_unconfirmed().is_empty());
}

/// Without gross, the same burn is applied with the ticks kept at a zero net,
/// and the pool is read again to settle them.
#[test]
fn without_gross_a_burn_to_a_zero_net_is_applied_and_read_again() {
    let book = book_of(PoolSnapshot {
        ladder: TickLadder::new(vec![(-199_000, 7), (-196_000, -7)], -200_000, -195_000),
        ..snapshot()
    });
    assert_eq!(book.apply_writes(&[liq(POOL, -199_000, -196_000, -7, (101, 0))]), vec![SwapApplied::NeedsReload]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L - 7);
    assert_eq!(p.ladder.ticks(), &[(-199_000, 0), (-196_000, 0)]);
    assert_eq!(book.positions_unconfirmed(), vec![POOL]);
    assert_eq!(book.position_counts(), (0, 1));
}

/// A position the ladder cannot hold is not applied at all, and the pool is
/// read again.
#[test]
fn a_position_the_ladder_cannot_hold_is_refused_and_read_again() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, -199_500, -196_000, -1, (101, 0))]), vec![SwapApplied::NeedsReload]);
    let p = book.get(POOL).unwrap();
    assert_eq!((p.state.liquidity, p.ladder.clone()), (L, snapshot().ladder), "unchanged");
    assert_eq!(book.positions_unconfirmed(), vec![POOL]);
}

/// Active liquidity that would go below zero is a book that is wrong: nothing
/// applied, read again.
#[test]
fn a_burn_below_zero_active_liquidity_is_refused_and_read_again() {
    let book = book_of(PoolSnapshot { state: ClPoolState { liquidity: 2, ..snapshot().state }, ..snapshot() });
    assert_eq!(book.apply_writes(&[liq(POOL, -199_000, -196_000, -3, (101, 0))]), vec![SwapApplied::NeedsReload]);
    let p = book.get(POOL).unwrap();
    assert_eq!((p.state.liquidity, p.ladder.clone()), (2, snapshot().ladder), "unchanged");
}

/// A preconfirmed log and its confirmed copy are one log: applied once.
#[test]
fn the_confirmed_copy_of_an_applied_liquidity_log_is_stale() {
    let book = book();
    book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 4))]);
    assert_eq!(book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 4))]), vec![SwapApplied::Stale]);
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L + 1_000, "added once");
}

/// The confirmed copy still matches after later writes moved the pool on.
#[test]
fn a_confirmed_copy_arriving_after_later_swaps_is_stale() {
    let book = book();
    book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 4))]);
    book.apply_writes(&[swap(L + 1_000, TICK - 5, (102, 0))]);
    assert_eq!(book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 4))]), vec![SwapApplied::Stale]);
    assert_eq!(book.get(POOL).unwrap().ladder.ticks()[1], (-198_000, 1_000), "added once");
}

/// A pool fresh from a read at block 100 already holds every log of block 100
/// and before: one of those is stale, not a reason to read again.
#[test]
fn a_liquidity_log_the_read_already_holds_is_stale() {
    let book = book();
    assert_eq!(book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (100, 9))]), vec![SwapApplied::Stale]);
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L);
}

/// A log older than the last swap, never applied (its preconfirmed copy was
/// missed): the ladder takes it, and active liquidity stays the swap's, which
/// already counts it.
#[test]
fn a_late_liquidity_log_changes_the_ladder_and_not_a_later_swaps_liquidity() {
    let book = book();
    book.apply_writes(&[swap(L + 1_000, TICK, (105, 0))]);
    assert_eq!(book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (104, 3))]), vec![SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L + 1_000, "the swap's liquidity, counted once");
    assert!(p.ladder.ticks().contains(&(-198_000, 1_000)));
    assert_eq!(p.last_log, Some((105, 0)));
}

/// A swap from before a change already applied (the swap's preconfirmed copy
/// was missed): its liquidity predates the change, so the change, active at
/// the swap's tick, is added back. And the swap is not stale: a position
/// change does not date the pool.
#[test]
fn a_swap_from_before_an_applied_mint_adds_the_mint_back() {
    let book = book();
    book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 5))]);
    assert_eq!(book.apply_writes(&[swap(L + 77, TICK - 3, (101, 2))]), vec![SwapApplied::Updated]);
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L + 77 + 1_000);

    // At a tick outside the mint's range, the swap's liquidity stands.
    let book = self::book();
    book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 5))]);
    book.apply_writes(&[swap(L + 77, -196_990, (101, 2))]);
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L + 77);
}

/// A swap from before an applied burn, whose liquidity cannot take the burn:
/// the book is not the chain's. The swap's own state stands, and the pool is
/// read again.
#[test]
fn a_swap_whose_later_changes_do_not_fit_its_liquidity_is_read_again() {
    let book = book();
    book.apply_writes(&[liq(POOL, -199_000, -196_000, -3, (101, 5))]);
    assert_eq!(book.apply_writes(&[swap(2, TICK, (101, 2))]), vec![SwapApplied::NeedsReload]);
    let p = book.get(POOL).unwrap();
    assert_eq!((p.state.liquidity, p.last_log), (2, Some((101, 2))));
    assert_eq!(book.positions_unconfirmed(), vec![POOL]);
}

/// Within one flashblock, a mint then a swap: the swap's liquidity, which
/// counts the mint, is the pool's.
#[test]
fn a_mint_then_a_swap_in_one_write_ends_at_the_swaps_liquidity() {
    let book = book();
    let r = book.apply_writes(&[liq(POOL, -198_000, -197_000, 1_000, (101, 1)), swap(L + 999, TICK - 3, (101, 2))]);
    assert_eq!(r, vec![SwapApplied::Updated, SwapApplied::Updated]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.state.liquidity, L + 999);
    assert!(p.ladder.ticks().contains(&(-198_000, 1_000)));
}

/// The just-in-time pattern, whole: mint, swap, burn. The ladder ends as read.
#[test]
fn a_just_in_time_position_leaves_the_ladder_as_read() {
    let book = book();
    let r = book.apply_writes(&[
        liq(POOL, -197_400, -197_300, 5_000, (101, 1)),
        swap(L + 5_000, TICK + 20, (101, 2)),
        liq(POOL, -197_400, -197_300, -5_000, (101, 3)),
    ]);
    assert_eq!(r, vec![SwapApplied::Updated; 3]);
    let p = book.get(POOL).unwrap();
    assert_eq!(p.ladder, snapshot().ladder);
    assert_eq!(p.state.liquidity, L, "the burn, active at the swap's tick, after it");
    assert!(book.positions_unconfirmed().is_empty());
}

/// A constant-product pool has no ticks: refused.
#[test]
fn a_constant_product_pool_takes_no_position() {
    assert_eq!(book().apply_writes(&[liq(CP, -10, 10, 5, (101, 0))]), vec![SwapApplied::Unknown]);
}

/// **Against the chain** (ignored: network, through Base's public endpoints,
/// never the canary's key). WETH/USDC on Uniswap, PancakeSwap and Slipstream,
/// each read at a block an hour back, every `Swap`, `Mint` and `Burn` since
/// applied as the feed applies them, then held to a fresh read at the head:
/// price, tick, active liquidity, and every tick both ladders cover, net and
/// gross, to the unit.
///
/// ```text
/// cargo test -j 3 -p apex-runtime --test live_liquidity -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "network: reads Base through its public endpoints"]
async fn applied_in_place_the_book_matches_a_fresh_read() {
    use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
    use apex_chain::rpc::RpcTransport;
    use apex_runtime::live::reads::ChainReads;
    use std::sync::Arc;

    // Public endpoints rate-limit: passes with backoff, and a second endpoint.
    let settings = FailoverSettings {
        request_timeout: std::time::Duration::from_secs(30),
        extra_passes: 4,
        base_backoff: std::time::Duration::from_secs(1),
    };
    let urls = ["https://mainnet.base.org".to_string(), "https://base-rpc.publicnode.com".to_string()];
    let http = FailoverTransport::connect(&urls, 8453, settings).await.unwrap();
    let rpc: Arc<dyn RpcTransport> = Arc::new(http);
    let reads = ChainReads::new(Arc::clone(&rpc));
    // A few blocks back: a load-balanced public endpoint's nodes lag each other.
    let head = reads.head().await.unwrap() - 5;
    let from = head - 1_800;
    let weth_usdc = |pool, venue, fee_ppm| PoolSpec { pool, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 0.0 };
    let mut positions = 0;
    for spec in [
        weth_usdc(POOL, Venue::UniswapV3, 500),
        weth_usdc(address!("72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38"), Venue::PancakeV3, 100),
        weth_usdc(address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59"), Venue::Slipstream, 500),
    ] {
        positions += against_the_chain(&*rpc, &reads, spec, from, head).await;
    }
    assert!(positions > 0, "no mint or burn in the window: widen it");
}

async fn against_the_chain(
    rpc: &dyn apex_chain::rpc::RpcTransport,
    reads: &apex_runtime::live::reads::ChainReads,
    spec: PoolSpec,
    from: u64,
    head: u64,
) -> usize {
    use apex_runtime::live::feed;
    use serde_json::json;

    let pool = spec.pool;
    let (book, refused) = PoolBook::load(reads, std::slice::from_ref(&spec), from).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let topics = [feed::SWAP, feed::PANCAKE_SWAP, feed::MINT, feed::BURN];
    let logs = rpc
        .call(
            "eth_getLogs",
            json!([{ "address": pool, "fromBlock": format!("0x{:x}", from + 1), "toBlock": format!("0x{head:x}"), "topics": [topics] }]),
        )
        .await
        .unwrap();
    let (mut writes, mut swaps, mut positions) = (Vec::new(), 0, 0);
    for l in logs.as_array().unwrap() {
        let raw = apex_chain::rpc::ws::parse_log(l, false).unwrap();
        if let Some(s) = feed::decode_swap(&raw) {
            swaps += 1;
            writes.push(StateWrite::Swap(SwapWrite {
                pool: s.pool,
                sqrt_price_x96: s.sqrt_price_x96,
                liquidity: s.liquidity,
                tick: s.tick,
                at: (s.block, s.log_index),
            }));
        } else if let Some(p) = feed::decode_liquidity(&raw).filter(|p| p.delta != 0) {
            positions += 1;
            writes.push(StateWrite::Liquidity(LiquidityWrite {
                pool: p.pool,
                tick_lower: p.tick_lower,
                tick_upper: p.tick_upper,
                delta: p.delta,
                at: (p.block, p.log_index),
            }));
        }
    }
    let results = book.apply_writes(&writes);
    let refused: Vec<_> = writes
        .iter()
        .zip(&results)
        .filter(|(w, r)| matches!(w, StateWrite::Liquidity(_)) && **r != SwapApplied::Updated)
        .collect();
    assert!(refused.is_empty(), "{pool}: position changes not applied exactly: {refused:?}");

    let (fresh, _) = PoolBook::load(reads, &[spec], head).await.unwrap();
    let (ours, chain) = (book.get(pool).unwrap(), fresh.get(pool).unwrap());
    assert_eq!(
        (ours.state.sqrt_price_x96, ours.state.tick, ours.state.liquidity),
        (chain.state.sqrt_price_x96, chain.state.tick, chain.state.liquidity),
        "{pool}: the pool's state"
    );
    let covered = |l: &TickLadder, other: &TickLadder| -> Vec<(i32, i128, u128)> {
        l.ticks()
            .iter()
            .zip(l.gross().expect("read with gross"))
            .filter(|((t, _), _)| other.covers(*t))
            .map(|((t, n), g)| (*t, *n, *g))
            .collect()
    };
    let (a, b) = (covered(&ours.ladder, &chain.ladder), covered(&chain.ladder, &ours.ladder));
    assert_eq!(a, b, "{pool}: the ladder over the ticks both cover");
    println!(
        "{pool} blocks {from}..={head}: {swaps} swaps and {positions} position changes applied; \
         {} ticks agree, net and gross; active liquidity {}",
        a.len(),
        chain.state.liquidity
    );
    positions
}
