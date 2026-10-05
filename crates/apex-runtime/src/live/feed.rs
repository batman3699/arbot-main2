//! Feed → book → events (Task 8.5 R3).
//!
//! # What a notification does
//!
//! | Notification | Book | Plane |
//! |---|---|---|
//! | `Swap` | held until its burst is over (below) | nothing yet |
//! | a burst's swaps, each newer than its pool's last log | post-swap states applied exactly, **in one write** | one `PendingSwap` on the fast lane, naming every pool they moved |
//! | `Swap`, not newer | nothing — it is the confirmed copy of a preconfirmed swap already applied | nothing |
//! | `Swap` that leaves the ladder | applied, pool reloaded | that pool left out until reloaded: it cannot be priced |
//! | `Mint` / `Burn` | pool reloaded — a range's liquidity changed | nothing |
//! | a removed log (reorg) | pool reloaded — what was applied may not have happened | nothing |
//! | `Reconnected` / `Gap` | what is held applied, whole book `Rebuilding`, then reloaded | nothing — state is unknown until rebuilt |
//! | `Head` | the latest sealed block, for the next fingerprint | nothing |
//!
//! # A flashblock is one state (R17)
//!
//! Base publishes a flashblock whole: every transaction in it is sequenced
//! before anyone outside the builder sees any of it. BlockPI's `pendingLogs`
//! then sends its logs **one notification at a time**. Applying and publishing
//! each as it came let a pricer read the book between two transactions of one
//! flashblock — a state no trade can ever land in. The first two routes ever to
//! pay were exactly that (2026-10-05, block 52,184,616: a swap into one pool
//! priced before the same flashblock's swap into the other, +0.35 bps between
//! them, −3 bps either side), and about half of all preconfirmed swaps are
//! followed by another of the same flashblock within 12 ms.
//!
//! So swaps are held until the feed has been quiet for [`BURST_QUIET`], then
//! applied in one write and published as one event: a pricer sees the book
//! before the flashblock or after it. A swap from a later block than a held
//! preconfirmed one ends the burst at once — that block began after the
//! burst's flashblock was published. A confirmed copy, which trails by
//! 0.5–2 s and lands in the middle of later bursts, is held with them: it is
//! from an older block, almost always already applied, and never dates a
//! burst. Merging two flashblocks costs one real state priced; splitting one
//! prices a state that never existed, so every tie goes to holding.
//!
//! # A preconfirmed swap and its confirmed copy are one swap
//!
//! Measured against BlockPI 2026-09-30: a `pendingLogs` entry carries the same
//! block number, log index and transaction hash as the confirmed log it becomes,
//! and arrives 0.5–2.0 s earlier. So `(block, log index)` identifies a swap across
//! both feeds: the book refuses the second copy as not newer, and the event both
//! would produce has one `Ordinal`, which the plane's redelivery check catches.

use crate::live::book::{PoolBook, SwapApplied, SwapWrite};
use crate::live::abi;
use crate::live::inventory::Venue;
use alloy_primitives::{b256, keccak256, Address, B256, U256};
use apex_chain::rpc::ws::{Head, Notification, RawLog};
use apex_state::feed::event::{EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{ChainId, PoolId};
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;
use std::time::Duration;

/// How long the feed must be quiet before the swaps it is holding are one
/// state.
///
/// Measured against BlockPI 2026-10-05 (90 s, 306 preconfirmed swaps on 49 WETH
/// pools): consecutive logs of one flashblock arrived 0.08 ms apart at the
/// median and 11.1 ms apart at most; the next flashblock ~206 ms later. Twenty
/// milliseconds is about twice the widest gap inside a flashblock and a tenth
/// of the gap between two.
pub const BURST_QUIET: Duration = Duration::from_millis(20);

/// `Swap(address,address,int256,int256,uint160,uint128,int24)` — Uniswap v3 and
/// Slipstream share it.
pub const SWAP: B256 = b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
/// PancakeSwap v3's `Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)`:
/// Uniswap's five words, then the protocol fee the swap paid in each token. A
/// topic of its own, so a feed listening for Uniswap's alone would never see a
/// PancakeSwap pool move.
pub const PANCAKE_SWAP: B256 = b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83");
/// `Mint(address,address,int24,int24,uint128,uint256,uint256)`
pub const MINT: B256 = b256!("7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
/// `Burn(address,int24,int24,uint128,uint256,uint256)`
pub const BURN: B256 = b256!("0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");

/// Stablecoins valued at $1 when a swap's notional is estimated. The notional
/// only decides whether a swap is large enough to search after (§12.4), so a
/// peg's small deviations do not matter; a token not on this list is priced
/// through a pool against one that is, or not at all.
pub const USD_STABLES: [Address; 4] = [
    alloy_primitives::address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), // USDC
    alloy_primitives::address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA"), // USDbC
    alloy_primitives::address!("fde4C96c8593536E31F229EA8f37b2ADa2699bb2"), // USDT
    alloy_primitives::address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb"), // DAI
];

/// One decoded `Swap`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapLog {
    pub pool: Address,
    pub amount0: i128,
    pub amount1: i128,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub block: u64,
    pub log_index: u64,
    pub tx: B256,
    pub pending: bool,
}

/// The `Swap` topic a venue's pools emit.
pub const fn swap_topic(venue: Venue) -> B256 {
    match venue {
        Venue::UniswapV3 | Venue::Slipstream => SWAP,
        Venue::PancakeV3 => PANCAKE_SWAP,
    }
}

/// `None` for anything that is not a well-formed `Swap` with a position: a log
/// that cannot be ordered cannot be applied, because ordering is what stops an
/// old log rolling a pool back. Uniswap's has five words of data and
/// PancakeSwap's seven, the first five the same.
pub fn decode_swap(l: &RawLog) -> Option<SwapLog> {
    let words = match l.topics.first() {
        Some(t) if *t == SWAP => 5,
        Some(t) if *t == PANCAKE_SWAP => 7,
        _ => return None,
    };
    if l.data.len() != words * 32 {
        return None;
    }
    Some(SwapLog {
        pool: l.address,
        amount0: abi::word_int(&l.data, 0, 128)?,
        amount1: abi::word_int(&l.data, 1, 128)?,
        sqrt_price_x96: abi::word_u256(&l.data, 2)?,
        liquidity: abi::word_uint(&l.data, 3, 128)?,
        tick: i32::try_from(abi::word_int(&l.data, 4, 24)?).ok()?,
        block: l.block_number?,
        log_index: l.log_index?,
        tx: l.transaction_hash?,
        pending: l.pending,
    })
}

/// A USD price per whole token for every token the book can price: stables at
/// $1, and anything paired with a priced token through the deepest such pool.
/// Two passes price every token within two pools of a stable — one quoted only
/// against WETH, say — whatever order the pools come in: the first pass can
/// meet that token's pool before WETH has a price. That covers the census
/// universe, which is WETH pairs.
pub fn usd_prices(book: &PoolBook) -> BTreeMap<Address, f64> {
    let snap = book.snapshot();
    let mut prices: BTreeMap<Address, f64> = USD_STABLES.iter().map(|a| (*a, 1.0)).collect();
    for _ in 0..2 {
        let mut pools: Vec<_> = snap.values().collect();
        pools.sort_by(|a, b| b.spec.depth_usd.total_cmp(&a.spec.depth_usd));
        for p in pools {
            let (t0, t1) = (p.spec.token0, p.spec.token1);
            let sqrt = u256_f64(p.state.sqrt_price_x96) / 2f64.powi(96);
            // token1 per token0, in whole units.
            let raw = sqrt * sqrt;
            let one_per_zero = raw * 10f64.powi(i32::from(p.decimals.0) - i32::from(p.decimals.1));
            if !one_per_zero.is_finite() || one_per_zero <= 0.0 {
                continue;
            }
            match (prices.get(&t0).copied(), prices.get(&t1).copied()) {
                (Some(_), Some(_)) | (None, None) => {}
                (None, Some(p1)) => {
                    prices.insert(t0, p1 * one_per_zero);
                }
                (Some(p0), None) => {
                    prices.insert(t1, p0 / one_per_zero);
                }
            }
        }
    }
    prices
}

fn u256_f64(v: ethers_core::types::U256) -> f64 {
    // Exact enough for a notional: the top 64 bits carry all the precision an
    // f64 has anyway.
    let bits = v.bits();
    if bits <= 64 {
        return v.low_u64() as f64;
    }
    let shift = bits - 64;
    ((v >> shift).low_u64() as f64) * 2f64.powi(shift as i32)
}

/// A swap's size in USD, from whichever side can be priced.
pub fn notional_usd(
    s: &SwapLog,
    tokens: (Address, Address),
    decimals: (u8, u8),
    prices: &BTreeMap<Address, f64>,
) -> Option<f64> {
    let side = |amount: i128, token: Address, dec: u8| {
        prices.get(&token).map(|p| (amount.unsigned_abs() as f64) / 10f64.powi(i32::from(dec)) * p)
    };
    let a = side(s.amount0, tokens.0, decimals.0);
    let b = side(s.amount1, tokens.1, decimals.1);
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// What handling one notification asks the caller to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Publish on the fast lane.
    Event(Box<StateEvent>),
    /// Re-read these pools at the latest block.
    Reload(Vec<Address>),
    /// Re-read everything; the book is `Rebuilding` until then.
    FullReload,
}

/// The pure half of the feed task: notifications in, book writes and effects
/// out. The async half — receiving, publishing, reloading, and calling
/// [`Self::flush`] once [`Self::quiet_deadline`] passes — is the caller's, so
/// this can be driven exactly by a test.
pub struct FeedHandler {
    chain: ChainId,
    last_head: Option<Head>,
    /// The burst still arriving, in arrival order, each swap with when it came.
    held: Vec<(SwapLog, UnixNanos)>,
}

impl FeedHandler {
    pub const fn new(chain: ChainId) -> Self {
        Self { chain, last_head: None, held: Vec::new() }
    }

    pub const fn last_head(&self) -> Option<&Head> {
        self.last_head.as_ref()
    }

    pub fn handle(&mut self, book: &PoolBook, n: Notification, now: UnixNanos) -> Vec<Effect> {
        match n {
            Notification::Head(h) => {
                self.last_head = Some(h);
                Vec::new()
            }
            Notification::Reconnected { .. } | Notification::Gap { .. } => {
                // What is held happened, so the book takes it; nothing is
                // published, because no state across a gap is priced until the
                // book has been read again.
                let held = std::mem::take(&mut self.held);
                Self::apply(book, &held);
                book.mark_rebuilding();
                vec![Effect::FullReload]
            }
            // Not the capture feed's: flashblocks are sampled on a connection of
            // their own, for the capacity model (R5). One here changes no pool.
            Notification::Flashblock(_) => Vec::new(),
            Notification::Log(l) if l.removed => vec![Effect::Reload(vec![l.address])],
            Notification::Log(l) => match l.topics.first() {
                Some(t) if *t == MINT || *t == BURN => vec![Effect::Reload(vec![l.address])],
                Some(t) if *t == SWAP || *t == PANCAKE_SWAP => self.hold(book, &l, now),
                _ => Vec::new(),
            },
        }
    }

    /// Whether swaps are held, waiting for their burst to end.
    pub fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// When the burst being held is over unless another swap arrives:
    /// [`BURST_QUIET`] after the latest one.
    pub fn quiet_deadline(&self) -> Option<UnixNanos> {
        let quiet = u64::try_from(BURST_QUIET.as_nanos()).unwrap_or(u64::MAX);
        self.held.last().map(|(_, at)| UnixNanos(at.0.saturating_add(quiet)))
    }

    fn hold(&mut self, book: &PoolBook, l: &RawLog, now: UnixNanos) -> Vec<Effect> {
        let Some(s) = decode_swap(l) else { return Vec::new() };
        // A swap from a later block than a held preconfirmed one: that block
        // began after the held flashblock was published, so the burst is over.
        // Only a preconfirmed swap dates the burst — a held confirmed copy is
        // from an older block, and would end it at the next swap of its own.
        let later = self.held.iter().any(|(h, _)| h.pending && h.block < s.block);
        let effects = if later { self.flush(book) } else { Vec::new() };
        self.held.push((s, now));
        effects
    }

    /// The burst is over: apply every held swap in one write, and publish one
    /// event for the state they leave.
    ///
    /// The event names each pool the burst moved and can be priced — a pool
    /// whose last swap left its ladder is reloaded instead, and one a later swap
    /// brought back is priceable. It covers every swap applied to those pools,
    /// carries the last one's ordinal, the state it describes being the one
    /// after that swap, and the largest one's notional: a burst is as large as
    /// its largest swap, and one that cannot be sized makes the burst
    /// unmeasured, which Engine D admits.
    pub fn flush(&mut self, book: &PoolBook) -> Vec<Effect> {
        let held = std::mem::take(&mut self.held);
        let Some(&(_, observed_at)) = held.last() else { return Vec::new() };
        let results = Self::apply(book, &held);

        // Each pool's last word, and every pool that left its ladder on the way.
        let mut last: BTreeMap<Address, SwapApplied> = BTreeMap::new();
        let mut reload: Vec<Address> = Vec::new();
        for ((s, _), r) in held.iter().zip(&results) {
            if matches!(r, SwapApplied::Updated | SwapApplied::NeedsReload) {
                last.insert(s.pool, *r);
            }
            if *r == SwapApplied::NeedsReload {
                reload.push(s.pool);
            }
        }
        let moved: Vec<&SwapLog> = held
            .iter()
            .zip(&results)
            .filter(|((s, _), r)| {
                matches!(r, SwapApplied::Updated | SwapApplied::NeedsReload)
                    && last.get(&s.pool) == Some(&SwapApplied::Updated)
            })
            .map(|((s, _), _)| s)
            .collect();

        let mut effects = Vec::new();
        if !reload.is_empty() {
            effects.push(Effect::Reload(reload));
        }
        if let Some(e) = self.event(book, &moved, observed_at) {
            effects.push(Effect::Event(Box::new(e)));
        }
        effects
    }

    fn apply(book: &PoolBook, held: &[(SwapLog, UnixNanos)]) -> Vec<SwapApplied> {
        let writes: Vec<SwapWrite> = held
            .iter()
            .map(|(s, _)| SwapWrite {
                pool: s.pool,
                sqrt_price_x96: s.sqrt_price_x96,
                liquidity: s.liquidity,
                tick: s.tick,
                at: (s.block, s.log_index),
            })
            .collect();
        book.apply_swaps(&writes)
    }

    fn event(&self, book: &PoolBook, moved: &[&SwapLog], observed_at: UnixNanos) -> Option<StateEvent> {
        let last = *moved.last()?;
        let prices = usd_prices(book);
        let mut pools: Vec<Address> = Vec::new();
        let mut notional = Some(0.0f64);
        // Identify the change itself, so two different bursts can never share
        // a fingerprint however their other fields line up.
        let mut delta = Vec::with_capacity(moved.len() * (32 + 8 + 20));
        for s in moved {
            if !pools.contains(&s.pool) {
                pools.push(s.pool);
            }
            let size = book
                .get(s.pool)
                .and_then(|p| notional_usd(s, (p.spec.token0, p.spec.token1), p.decimals, &prices));
            notional = notional.zip(size).map(|(a, b)| a.max(b));
            delta.extend_from_slice(s.tx.as_slice());
            delta.extend_from_slice(&s.log_index.to_be_bytes());
            delta.extend_from_slice(s.pool.as_slice());
        }

        let head = self.last_head.as_ref();
        let fingerprint = StateFingerprint {
            chain_id: self.chain,
            parent_block_hash: head.map_or(B256::ZERO, |h| h.hash),
            confirmed_block_number: head.map_or(last.block.saturating_sub(1), |h| h.number),
            preconf_sequence: last.pending.then_some(last.block),
            flashblock_index: None,
            state_root_or_equivalent: None,
            block_hash_if_available: None,
            state_delta_hash: keccak256(&delta),
            venue_state_version: book.versions_for(&pools),
            external_dependency_fingerprint: None,
        };
        Some(StateEvent {
            chain: self.chain,
            // Both copies of one swap get this ordinal, so the plane sees the
            // second as a redelivery. `flashblock_index` is 0 because the provider
            // does not say which flashblock carried it — the ordering within a
            // block is the log index, which it does say.
            at: Ordinal { block: last.block, flashblock_index: 0, tx_index: 0, log_index: last.log_index, payload_id: 0 },
            observed_at,
            fingerprint,
            kind: EventKind::PendingSwap {
                target: last.tx,
                pools: pools.iter().map(|a| PoolId { chain: self.chain, address: *a }).collect(),
                notional_usd: notional,
            },
        })
    }
}
