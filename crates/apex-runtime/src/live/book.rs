//! Live pool state for the capture path (Task 8.5 R2).
//!
//! # One block, or it is not a state
//!
//! A pool's state is assembled from eleven reads — tokens, factory, fee, tick
//! spacing, `slot0`, liquidity, both balances, both decimals — plus its tick
//! ladder. Every one is **pinned to the same block**, because a state assembled
//! from reads at different blocks never existed on chain and anything priced
//! against it is priced against a chimera.
//!
//! # Readers never wait
//!
//! The book is `apex_state::Versioned`: a pricer takes a snapshot and holds a
//! consistent view for as long as it likes, while swaps land (INV-11, §2.4).
//! Writes take a writer lock — load, modify, store must not interleave with
//! another write or one of them is lost — and readers never touch it.
//!
//! # A swap is exact, and a ladder knows where it ends
//!
//! A `Swap` log carries the pool's post-swap `sqrtPriceX96`, `liquidity` and
//! `tick`, so applying one sets the state exactly rather than estimating it. It
//! is applied only if it is **newer** than what the pool holds — the last log
//! applied, or, after a read, the whole of the read's block — so neither a
//! confirmed log trailing a later preconfirmed one nor a log the read already
//! contains can roll the pool back; and a reload never rolls back a swap newer
//! than its read. When a swap moves the price outside the tick range the ladder
//! was built over, the snapshot says so ([`PoolSnapshot::ladder_covers_price`])
//! and the pricer refuses it until the pool is reloaded — the alternative is
//! pricing at the last known liquidity, which is the constant-liquidity error
//! behind the legacy fast path's ~140 bps.
//!
//! Balances are **not** advanced by swap deltas. A swap seen first preconfirmed
//! and then confirmed would be counted twice, so balances stay as of the last
//! full read. They bound how much a pool can pay out, and our trade sizes are a
//! small fraction of every admitted pool's holdings; a reload refreshes them.

use crate::live::abi::{self, selector};
use crate::live::inventory::PoolSpec;
use crate::live::reads::{ChainReads, ReadError};
use alloy_primitives::{Address, B256, U256};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_state::Versioned;
use apex_types::compat::{address_to_ethers, u256_to_ethers};
use apex_types::state::ReconstructionStatus;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Bitmap words read on each side of the current one. A word spans
/// `256 × tickSpacing` ticks: ±29% of price at spacing 10, ±2.6% at spacing 1 —
/// far past anything a trade of our size moves a pool this deep.
pub const WORDS_PER_SIDE: usize = 2;

/// Where a log sits in the chain's order: `(block, log index)`.
pub type LogPosition = (u64, u64);

/// One pool, as of one block.
#[derive(Clone, Debug)]
pub struct PoolSnapshot {
    pub spec: PoolSpec,
    pub state: ClPoolState,
    pub ladder: TickLadder,
    pub decimals: (u8, u8),
    /// `factory()`, read from the pool.
    pub factory: Address,
    pub code_hash: B256,
    /// The block the state was read at, or the block of the last swap applied.
    pub block: u64,
    /// The last log applied, so an older one is refused.
    pub last_log: Option<LogPosition>,
    /// The book's write sequence at this pool's last change. **Assigned by the
    /// book** — whatever a caller sets is replaced when the snapshot enters one —
    /// from a counter every write advances, so it moves on every change to this
    /// pool and on no other. See [`PoolBook::versions_for`].
    pub seq: u64,
}

impl PoolSnapshot {
    /// Whether the current price is inside the range the ladder proved. Outside
    /// it a quote would walk off the ladder, so the pricer refuses the pool until
    /// it is reloaded.
    pub fn ladder_covers_price(&self) -> bool {
        self.ladder.covers(self.state.tick)
    }
}

/// Why a proposed pool could not be loaded.
///
/// A fact about the **pool**, established at boot or reload, before any
/// candidate exists — it removes a pool from the universe and declines no
/// trade. So it is not an INV-40 rejection and carries no miss bucket; a route
/// that needs an unloaded pool is never proposed, because the frontier is built
/// from the book.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unloadable {
    /// One of its eleven state reads failed: not a pool, or not this kind.
    Unreadable { read: &'static str },
    /// The chain's tokens are not the inventory's.
    WrongTokens { chain: (Address, Address) },
    /// Deployed by a factory other than the venue's.
    WrongFactory { factory: Address },
    /// No liquidity in range: nothing to price.
    NoLiquidity,
    /// The tick ladder could not be read.
    LadderUnreadable { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unloaded {
    pub pool: Address,
    pub why: Unloadable,
}

/// What applying a swap did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapApplied {
    Updated,
    /// Applied, and the price has left the ladder: reload before pricing.
    NeedsReload,
    /// Older than the last log applied; ignored.
    Stale,
    /// Not a pool in this book.
    Unknown,
}

pub struct PoolBook {
    pools: Versioned<BTreeMap<Address, Arc<PoolSnapshot>>>,
    writer: Mutex<WriterState>,
}

/// What the writer lock guards besides the right to write.
#[derive(Debug, Default)]
struct WriterState {
    /// The last write sequence issued.
    seq: u64,
    /// The sequence of the last feed gap marked.
    last_gap: u64,
    /// The newest block any read has stored.
    newest_read: u64,
}

/// Why a reload wrote nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum ReloadError {
    Read(ReadError),
    /// Older than a read the book already holds. Replacing a newer read would
    /// roll pools back — ladders and balances as well as prices — so the reload
    /// is refused whole; read again at the latest block.
    Older { block: u64, newest: u64 },
}

impl From<ReadError> for ReloadError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "{e}"),
            Self::Older { block, newest } => {
                write!(f, "a read at block {block} is older than the book's newest, {newest}")
            }
        }
    }
}

impl std::fmt::Debug for PoolBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.pools.load();
        f.debug_struct("PoolBook")
            .field("pools", &s.value.len())
            .field("status", &s.reconstruction)
            .finish()
    }
}

/// The eleven state reads for one pool, in the order they are decoded.
fn state_calls(s: &PoolSpec) -> Vec<(Address, Vec<u8>)> {
    vec![
        (s.pool, abi::call0(selector::TOKEN0)),
        (s.pool, abi::call0(selector::TOKEN1)),
        (s.pool, abi::call0(selector::FACTORY)),
        (s.pool, abi::call0(selector::FEE)),
        (s.pool, abi::call0(selector::TICK_SPACING)),
        (s.pool, abi::call0(selector::SLOT0)),
        (s.pool, abi::call0(selector::LIQUIDITY)),
        (s.token0, abi::call_address(selector::BALANCE_OF, s.pool)),
        (s.token1, abi::call_address(selector::BALANCE_OF, s.pool)),
        (s.token0, abi::call0(selector::DECIMALS)),
        (s.token1, abi::call0(selector::DECIMALS)),
    ]
}

const STATE_READS: usize = 11;

/// Decode one pool's eleven answers, or say which one was missing.
fn decode_state(
    s: &PoolSpec,
    a: &[Option<Vec<u8>>],
) -> Result<(ClPoolState, Address, (u8, u8)), Unloadable> {
    let get = |i: usize, read: &'static str| a[i].as_deref().ok_or(Unloadable::Unreadable { read });
    let token0 = abi::word_address(get(0, "token0")?, 0).ok_or(Unloadable::Unreadable { read: "token0" })?;
    let token1 = abi::word_address(get(1, "token1")?, 0).ok_or(Unloadable::Unreadable { read: "token1" })?;
    if (token0, token1) != (s.token0, s.token1) {
        return Err(Unloadable::WrongTokens { chain: (token0, token1) });
    }
    let factory =
        abi::word_address(get(2, "factory")?, 0).ok_or(Unloadable::Unreadable { read: "factory" })?;
    if factory != s.venue.factory() {
        return Err(Unloadable::WrongFactory { factory });
    }
    let fee = abi::word_uint(get(3, "fee")?, 0, 24).ok_or(Unloadable::Unreadable { read: "fee" })?;
    let spacing = abi::word_int(get(4, "tickSpacing")?, 0, 24)
        .ok_or(Unloadable::Unreadable { read: "tickSpacing" })?;
    let slot0 = get(5, "slot0")?;
    let sqrt_price = abi::word_u256(slot0, 0).ok_or(Unloadable::Unreadable { read: "slot0" })?;
    let tick = abi::word_int(slot0, 1, 24).ok_or(Unloadable::Unreadable { read: "slot0" })?;
    let liquidity =
        abi::word_uint(get(6, "liquidity")?, 0, 128).ok_or(Unloadable::Unreadable { read: "liquidity" })?;
    if liquidity == 0 || sqrt_price.is_zero() {
        return Err(Unloadable::NoLiquidity);
    }
    // Balances may legitimately fail to read (a token that reverts on
    // `balanceOf` for a contract). `ClPoolState` carries them as `Option`, and
    // the pricer fails closed on `None`.
    let balance = |i: usize| a[i].as_deref().and_then(|d| abi::word_u256(d, 0)).map(u256_to_ethers);
    let d0 = abi::word_uint(get(9, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let d1 = abi::word_uint(get(10, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let state = ClPoolState {
        sqrt_price_x96: u256_to_ethers(sqrt_price),
        liquidity,
        tick: i32::try_from(tick).map_err(|_| Unloadable::Unreadable { read: "slot0" })?,
        tick_spacing: i32::try_from(spacing).map_err(|_| Unloadable::Unreadable { read: "tickSpacing" })?,
        fee_ppm: u32::try_from(fee).map_err(|_| Unloadable::Unreadable { read: "fee" })?,
        balance0: balance(7),
        balance1: balance(8),
    };
    Ok((state, factory, (d0 as u8, d1 as u8)))
}

impl PoolBook {
    /// Read every proposed pool at `block`. Pools that disagree with the chain
    /// are refused and reported, never loaded.
    pub async fn load(
        reads: &ChainReads,
        specs: &[PoolSpec],
        block: u64,
    ) -> Result<(Self, Vec<Unloaded>), ReadError> {
        let (pools, refused) = Self::read(reads, specs, block).await?;
        Ok((Self::from_snapshots(pools.into_values(), ReconstructionStatus::Verified), refused))
    }

    async fn read(
        reads: &ChainReads,
        specs: &[PoolSpec],
        block: u64,
    ) -> Result<(BTreeMap<Address, PoolSnapshot>, Vec<Unloaded>), ReadError> {
        let calls: Vec<(Address, Vec<u8>)> = specs.iter().flat_map(state_calls).collect();
        let answers = reads.multicall(&calls, block).await?;

        let mut pools = BTreeMap::new();
        let mut refused = Vec::new();
        for (spec, a) in specs.iter().zip(answers.chunks(STATE_READS)) {
            let (state, factory, decimals) = match decode_state(spec, a) {
                Ok(x) => x,
                Err(why) => {
                    refused.push(Unloaded { pool: spec.pool, why });
                    continue;
                }
            };
            let ladder = match apex_venues::cl_ticks::build_ladder(
                reads,
                address_to_ethers(spec.pool),
                &state,
                ethers_core::types::U64::from(block),
                WORDS_PER_SIDE,
            )
            .await
            {
                Ok(l) => l,
                Err(e) => {
                    refused.push(Unloaded {
                        pool: spec.pool,
                        why: Unloadable::LadderUnreadable { detail: e.to_string() },
                    });
                    continue;
                }
            };
            let code_hash = reads.code_hash(spec.pool, block).await?;
            pools.insert(
                spec.pool,
                PoolSnapshot {
                    spec: spec.clone(),
                    state,
                    ladder,
                    decimals,
                    factory,
                    code_hash,
                    block,
                    last_log: None,
                    seq: 0,
                },
            );
        }
        Ok((pools, refused))
    }

    /// A book over snapshots the caller already holds — a replay, a test.
    ///
    /// The caller states the provenance, because `Versioned` requires it and
    /// defaulting either way is wrong: `Verified` would authorize on state nobody
    /// read from the chain, and `Unsafe` would make the safe path the one taken by
    /// forgetting.
    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = PoolSnapshot>,
        provenance: ReconstructionStatus,
    ) -> Self {
        let mut newest_read = 0;
        let pools = snapshots
            .into_iter()
            .map(|mut p| {
                // Replaced, not kept: a caller's larger value would put every
                // later write to another pool behind it.
                p.seq = 0;
                newest_read = newest_read.max(p.block);
                (p.spec.pool, Arc::new(p))
            })
            .collect();
        Self {
            pools: Versioned::new(pools, provenance),
            writer: Mutex::new(WriterState { seq: 0, last_gap: 0, newest_read }),
        }
    }

    /// One consistent view of every pool. Wait-free.
    pub fn snapshot(&self) -> Arc<BTreeMap<Address, Arc<PoolSnapshot>>> {
        Arc::clone(&self.pools.load().value)
    }

    pub fn get(&self, pool: Address) -> Option<Arc<PoolSnapshot>> {
        self.pools.load().value.get(&pool).cloned()
    }

    /// The state version of each venue, **over `pools` only**: the latest write
    /// sequence among them.
    ///
    /// Scoped to a route's own pools on purpose. Last-mile revalidation compares
    /// a reading taken before the route is priced with one taken before signing,
    /// by equality, and a venue-wide version would refuse a ticket because some
    /// *other* pool on the venue swapped — noise that would dominate the
    /// capture-assurance figure and say nothing about the trade.
    ///
    /// **A write sequence, not a chain position.** Every write takes a sequence
    /// newer than every write before it, so the maximum moves whenever any of the
    /// pools changes. The first version used the latest `(block, log index)`
    /// applied, and a maximum of positions can stay put while a pool changes
    /// underneath it: a reload reads at the latest *sealed* block while another
    /// pool on the route already holds a preconfirmed swap from the block after,
    /// and a swap the preconfirmed feed missed arrives confirmed behind one it
    /// delivered. Either way last-mile would have passed a ticket priced against
    /// state that had moved.
    ///
    /// A pool the book no longer carries makes the reading **empty**, which
    /// last-mile reads as every venue gone rather than as unchanged (§5.6).
    pub fn versions_for(&self, pools: &[Address]) -> BTreeMap<apex_types::ids::VenueId, u64> {
        let snap = self.snapshot();
        let mut out = BTreeMap::new();
        for a in pools {
            let Some(p) = snap.get(a) else { return BTreeMap::new() };
            let v = out.entry(p.spec.venue.id()).or_insert(0);
            *v = (*v).max(p.seq);
        }
        out
    }

    /// `Verified` after a full read; `Rebuilding` from a feed gap until the next
    /// one. Only `Verified` may authorize a live ticket (INV-08).
    pub fn status(&self) -> ReconstructionStatus {
        self.pools.load().reconstruction
    }

    pub fn len(&self) -> usize {
        self.pools.load().value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Load, modify, store, under the writer lock so no write is lost to
    /// another. The closure gets the lock's state — `seq` is already this
    /// write's — and the **current** status, and returns the status to store.
    ///
    /// The status is read here, inside the lock, and not by the caller before
    /// it: the first version read it outside, so a partial reload in flight when
    /// the feed marked a gap stored its stale `Verified` back over `Rebuilding`,
    /// and the book authorized on state with a hole in it (INV-08).
    fn write<R>(
        &self,
        f: impl FnOnce(
            &mut BTreeMap<Address, Arc<PoolSnapshot>>,
            &mut WriterState,
            ReconstructionStatus,
        ) -> (R, ReconstructionStatus),
    ) -> R {
        let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        w.seq += 1;
        let current = self.pools.load();
        let mut map = (*current.value).clone();
        let (r, status) = f(&mut map, &mut w, current.reconstruction);
        self.pools.store(map, status);
        r
    }

    /// Apply a `Swap` log's post-swap state, if it is newer than what the pool
    /// holds.
    ///
    /// Newer than the last log applied — or, for a pool whose state came from a
    /// read, from a **later block** than the read. A read at block `n` is the
    /// state after every log in `n`, and the confirmed feed trails the
    /// preconfirmed one by 0.5–2 s, so a log from `n` arriving after the read is
    /// already in it; applying it would roll the pool back to the middle of the
    /// block.
    pub fn apply_swap(
        &self,
        pool: Address,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        at: LogPosition,
    ) -> SwapApplied {
        self.write(|map, w, status| {
            let Some(old) = map.get(&pool) else { return (SwapApplied::Unknown, status) };
            let newer = match old.last_log {
                Some(last) => at > last,
                None => at.0 > old.block,
            };
            if !newer {
                return (SwapApplied::Stale, status);
            }
            let mut next = (**old).clone();
            next.state.sqrt_price_x96 = u256_to_ethers(sqrt_price_x96);
            next.state.liquidity = liquidity;
            next.state.tick = tick;
            next.block = at.0;
            next.last_log = Some(at);
            next.seq = w.seq;
            let covered = next.ladder_covers_price();
            map.insert(pool, Arc::new(next));
            (if covered { SwapApplied::Updated } else { SwapApplied::NeedsReload }, status)
        })
    }

    /// A feed gap: everything may have missed updates. `Rebuilding` until a
    /// full [`Self::reload`] that **began after** this gap completes.
    pub fn mark_rebuilding(&self) {
        self.write(|_, w, _| {
            w.last_gap = w.seq;
            ((), ReconstructionStatus::Rebuilding)
        });
    }

    /// Re-read `pools` at `block` and replace them; every pool if `pools` is
    /// empty.
    ///
    /// **A read never rolls back a swap already applied.** A reload reads at the
    /// latest sealed block, and a pool may already hold a preconfirmed swap from
    /// the block after — the ladder-exit reload is triggered by exactly such a
    /// swap. That pool keeps its price, tick, liquidity and position, and takes
    /// the fresh ladder and balances. If its price is off the fresh ladder it
    /// stays unpriceable, and the caller reloads it again once a later block is
    /// sealed.
    ///
    /// A full reload returns the book to `Verified` — unless a gap was marked
    /// while it was reading: the logs that gap dropped may postdate the read,
    /// and it is that gap's own full reload that may clear it.
    ///
    /// A read older than one the book already holds is refused whole
    /// ([`ReloadError::Older`]) — checked at the write, because another reload
    /// may land while this one reads.
    pub async fn reload(
        &self,
        reads: &ChainReads,
        pools: &[Address],
        block: u64,
    ) -> Result<Vec<Unloaded>, ReloadError> {
        let started = self.writer.lock().unwrap_or_else(|p| p.into_inner()).seq;
        let current = self.snapshot();
        let full = pools.is_empty();
        let specs: Vec<PoolSpec> = current
            .values()
            .filter(|p| full || pools.contains(&p.spec.pool))
            .map(|p| p.spec.clone())
            .collect();
        let (fresh, refused) = Self::read(reads, &specs, block).await?;
        self.write(|map, w, status| {
            if block < w.newest_read {
                return (Err(ReloadError::Older { block, newest: w.newest_read }), status);
            }
            w.newest_read = block;
            for (addr, mut snap) in fresh {
                if let Some(old) = map.get(&addr).filter(|o| o.last_log.is_some_and(|(b, _)| b > block)) {
                    snap.state.sqrt_price_x96 = old.state.sqrt_price_x96;
                    snap.state.liquidity = old.state.liquidity;
                    snap.state.tick = old.state.tick;
                    snap.block = old.block;
                    snap.last_log = old.last_log;
                }
                snap.seq = w.seq;
                map.insert(addr, Arc::new(snap));
            }
            // A pool that can no longer be read is removed rather than kept at
            // a state nobody can confirm.
            for r in &refused {
                map.remove(&r.pool);
            }
            let status =
                if full && w.last_gap <= started { ReconstructionStatus::Verified } else { status };
            (Ok(refused), status)
        })
    }
}
