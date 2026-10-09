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
//! # A Slipstream fee is a function of the pool, not a number read once
//!
//! Aerodrome Slipstream takes its swap fee from the factory's fee module, and
//! the module's fee moves: `min(cap, base + |tick − TWAP| × K / 10⁶)` over a
//! ten-minute TWAP, except that the **first** swap on a pool in a block pays the
//! module's initial fee (150 ppm against a ~600–900 ppm dynamic fee on WETH/USDC,
//! measured 2026-10-01). A trigger swap moves the tick off the TWAP and raises
//! the fee for everything after it in the block, so a fee read at load is wrong
//! exactly when it matters. The book holds each Slipstream pool's
//! [`DynamicFee`] and recomputes the fee **from the tick** on every write, and
//! [`PoolBook::refresh_twaps`] follows the TWAP between swaps. Pricing uses the
//! after-first fee: ordering within a block is not ours to choose.
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
    /// Slipstream's fee regime; `None` for a pool with a fixed fee. While set,
    /// `state.fee_ppm` is its after-first fee at `state.tick`, maintained by
    /// the book.
    pub dynamic_fee: Option<DynamicFee>,
    /// The book's write sequence at this pool's last change. **Assigned by the
    /// book** — whatever a caller sets is replaced when the snapshot enters one —
    /// from a counter every write advances, so it moves on every change to this
    /// pool and on no other. See [`PoolBook::versions_for`].
    pub seq: u64,
    /// A constant-product pool's reserves (R24): its whole state with its fee,
    /// `state.fee_ppm`. `None` for a concentrated-liquidity pool. A
    /// constant-product pool's tick state is empty — zero price and liquidity,
    /// no balances, no ladder — so a concentrated-liquidity path that missed the
    /// dispatch fails closed.
    pub reserves: Option<crate::live::cp::Reserves>,
}

impl PoolSnapshot {
    /// Whether the current price is inside the range the ladder proved. Outside
    /// it a quote would walk off the ladder, so the pricer refuses the pool until
    /// it is reloaded.
    pub fn ladder_covers_price(&self) -> bool {
        // A constant-product pool has no ladder to leave.
        self.reserves.is_some() || self.ladder.covers(self.state.tick)
    }

    /// Set the fee from the tick, for a pool whose fee is a function of it.
    fn fee_from_tick(&mut self) {
        if let Some(d) = self.dynamic_fee {
            self.state.fee_ppm = d.after_first(self.state.tick);
        }
    }
}

/// A Slipstream pool's fee regime, as its factory's `DynamicSwapFeeModule`
/// computes it (`aerodrome-finance/slipstream`,
/// `contracts/core/fees/DynamicSwapFeeModule.sol`). Checked against `fee()` on
/// eleven Base blocks, 2026-10-01, every one exact.
///
/// The module's per-origin discount is left out: it is keyed on `tx.origin`,
/// and no address of ours is registered for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicFee {
    /// The pool's base fee, or the factory's for its tick spacing when unset.
    pub base: u32,
    /// The pool's cap, or the module's default when the pool has no scaling.
    pub cap: u32,
    /// `K`: fee per tick of |tick − TWAP|, scaled by 10⁶.
    pub scaling: u64,
    /// What the first swap on the pool in a block pays, when enabled.
    pub initial: Option<u32>,
    /// The TWAP's window.
    pub seconds_ago: u32,
    /// The pool's time-weighted tick over `seconds_ago`, as last read. `None`
    /// where the module adds nothing: too few observations, or `observe`
    /// reverted.
    pub twap_tick: Option<i32>,
}

impl DynamicFee {
    /// The fee on a pool configured to charge nothing (`ZERO_FEE_INDICATOR`).
    pub const FREE: Self = Self { base: 0, cap: 0, scaling: 0, initial: None, seconds_ago: 0, twap_tick: None };

    /// What a swap pays once the block's first swap on the pool has written its
    /// observation — what the book prices by.
    pub fn after_first(&self, tick: i32) -> u32 {
        let dynamic = self.twap_tick.map_or(0u128, |t| {
            u128::from((i64::from(tick) - i64::from(t)).unsigned_abs()) * u128::from(self.scaling) / 1_000_000
        });
        u32::try_from((u128::from(self.base) + dynamic).min(u128::from(self.cap))).unwrap_or(u32::MAX)
    }

    /// What the first swap on the pool in a block pays.
    pub fn first_in_block(&self, tick: i32) -> u32 {
        self.initial.unwrap_or_else(|| self.after_first(tick))
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
    /// A Slipstream pool's fee regime could not be read: which read failed. A
    /// pool priced at a fee nobody read is priced wrong by up to its cap.
    FeeUnreadable { read: &'static str },
    /// An Aerodrome v2 pool on the stable curve, which is not the one priced.
    NotVolatile,
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

/// One `Swap` log's post-swap state, as [`PoolBook::apply_swaps`] writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapWrite {
    pub pool: Address,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub at: LogPosition,
}

/// One `Sync` log's reserves, as [`PoolBook::apply_writes`] writes them (R24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncWrite {
    pub pool: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    pub at: LogPosition,
}

/// A log's post-trade state, of either kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateWrite {
    Swap(SwapWrite),
    Sync(SyncWrite),
}

pub struct PoolBook {
    pools: Versioned<BTreeMap<Address, Arc<PoolSnapshot>>>,
    writer: Mutex<WriterState>,
    /// Every pool the book was built over, whether or not it holds it now: what
    /// a full reload reads.
    universe: Vec<PoolSpec>,
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

/// `ZERO_FEE_INDICATOR`: the module's "configured to charge nothing".
const ZERO_FEE: u128 = 420;

/// A fee module's defaults: `defaultScalingFactor`, `defaultFeeCap`, and the
/// TWAP window `secondsAgo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleDefaults {
    pub scaling: u128,
    pub cap: u128,
    pub seconds_ago: u128,
}

/// One pool's fee regime from its `dynamicFeeConfig(pool)` answer, by the
/// module's own rules: a pool configured fee-free charges nothing at all; an
/// unset base is the factory's `tickSpacingToFee` for the pool (`spacing_fee`,
/// consulted only then); an unset scaling takes the module's default scaling
/// **and** default cap; an unset initial fee is the base. The TWAP is not here —
/// it is read after, over the window this names.
pub fn compose_dynamic_fee(
    config: &[u8],
    defaults: ModuleDefaults,
    spacing_fee: Option<u128>,
) -> Result<DynamicFee, &'static str> {
    let word = |w: usize, bits: u32| abi::word_uint(config, w, bits).ok_or("dynamicFeeConfig");
    let (base, cap, k, on, initial) = (word(0, 24)?, word(1, 24)?, word(2, 64)?, word(3, 1)?, word(4, 24)?);
    if base == ZERO_FEE {
        return Ok(DynamicFee::FREE);
    }
    let base = if base != 0 { base } else { spacing_fee.ok_or("tickSpacingToFee")? };
    let (k, cap) = if k != 0 { (k, cap) } else { (defaults.scaling, defaults.cap) };
    let initial = (on == 1).then_some(match initial {
        0 => base,
        ZERO_FEE => 0,
        x => x,
    });
    let n = |v: u128| u32::try_from(v).map_err(|_| "dynamicFeeConfig");
    Ok(DynamicFee {
        base: n(base)?,
        cap: n(cap)?,
        scaling: u64::try_from(k).map_err(|_| "dynamicFeeConfig")?,
        initial: initial.map(n).transpose()?,
        seconds_ago: u32::try_from(defaults.seconds_ago).map_err(|_| "secondsAgo")?,
        twap_tick: None,
    })
}

/// Each Slipstream pool's fee regime at `block`, or which read failed.
///
/// Three rounds, because each names what the next reads: the factory's fee
/// module; then the module's defaults and window, each pool's configuration and
/// the factory's fee for its spacing; then each pool's TWAP. Composed by the
/// module's own rules — an unset base is the spacing's, an unset scaling takes
/// the defaults for scaling *and* cap, an unset initial fee is the base — and a
/// pool with fewer observations than the window needs gets no TWAP, as the
/// module gives it no dynamic fee.
async fn read_dynamic_fees(
    reads: &ChainReads,
    pools: &[(Address, Address, i32, u64)],
    block: u64,
) -> Result<Vec<(Address, Result<DynamicFee, &'static str>)>, ReadError> {
    if pools.is_empty() {
        return Ok(Vec::new());
    }
    let factories: Vec<Address> =
        pools.iter().map(|p| p.1).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let a = reads
        .multicall(&factories.iter().map(|f| (*f, abi::call0(selector::SWAP_FEE_MODULE))).collect::<Vec<_>>(), block)
        .await?;
    let module: BTreeMap<Address, Address> = factories
        .iter()
        .zip(a)
        .filter_map(|(f, r)| Some((*f, abi::word_address(r.as_deref()?, 0)?)))
        .collect();

    let modules: Vec<Address> = module.values().copied().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let mut calls = Vec::new();
    for m in &modules {
        calls.push((*m, abi::call0(selector::DEFAULT_SCALING_FACTOR)));
        calls.push((*m, abi::call0(selector::DEFAULT_FEE_CAP)));
        calls.push((*m, abi::call0(selector::SECONDS_AGO)));
    }
    for (pool, factory, spacing, _) in pools {
        calls.push((module.get(factory).copied().unwrap_or(Address::ZERO), abi::call_address(selector::DYNAMIC_FEE_CONFIG, *pool)));
        calls.push((*factory, abi::call_signed(selector::TICK_SPACING_TO_FEE, i64::from(*spacing))));
    }
    let b = reads.multicall(&calls, block).await?;
    let uint = |i: usize, bits: u32| b[i].as_deref().and_then(|d| abi::word_uint(d, 0, bits));
    let defaults: BTreeMap<Address, Option<ModuleDefaults>> = modules
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let d = (|| {
                Some(ModuleDefaults {
                    scaling: uint(3 * i, 64)?,
                    cap: uint(3 * i + 1, 32)?,
                    seconds_ago: uint(3 * i + 2, 32)?,
                })
            })();
            (*m, d)
        })
        .collect();

    let mut composed = Vec::with_capacity(pools.len());
    for (j, (pool, factory, _, cardinality)) in pools.iter().enumerate() {
        let at = 3 * modules.len() + 2 * j;
        let fee = (|| {
            let m = module.get(factory).ok_or("swapFeeModule")?;
            let d = defaults.get(m).copied().flatten().ok_or("module defaults")?;
            compose_dynamic_fee(b[at].as_deref().ok_or("dynamicFeeConfig")?, d, uint(at + 1, 24))
        })();
        // The module reads a TWAP only with the observations its window needs
        // (`MIN_SECONDS_AGO` = 2); a pool without them pays no dynamic fee.
        let observable = |d: &DynamicFee| *cardinality >= u64::from(d.seconds_ago) / 2;
        composed.push((*pool, fee.map(|d| (d, observable(&d)))));
    }

    let observe: Vec<(Address, Vec<u8>)> = composed
        .iter()
        .filter_map(|(p, f)| match f {
            Ok((d, true)) => Some((*p, abi::call_observe(d.seconds_ago))),
            _ => None,
        })
        .collect();
    let c = reads.multicall(&observe, block).await?;
    let twap: BTreeMap<Address, Option<i32>> = observe
        .iter()
        .zip(c)
        .map(|((p, _), r)| {
            let d = composed.iter().find(|(q, _)| q == p).and_then(|(_, f)| f.as_ref().ok()).map(|(d, _)| d.seconds_ago);
            (*p, r.as_deref().zip(d).and_then(|(r, sa)| abi::decode_twap(r, sa)))
        })
        .collect();
    Ok(composed
        .into_iter()
        .map(|(p, f)| (p, f.map(|(d, _)| DynamicFee { twap_tick: twap.get(&p).copied().flatten(), ..d })))
        .collect())
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
        let book = Self::from_snapshots(pools.into_values(), ReconstructionStatus::Verified);
        // The refused as well: a pool that failed to read at boot is read again
        // by the first full reload.
        Ok((Self { universe: specs.to_vec(), ..book }, refused))
    }

    async fn read(
        reads: &ChainReads,
        specs: &[PoolSpec],
        block: u64,
    ) -> Result<(BTreeMap<Address, PoolSnapshot>, Vec<Unloaded>), ReadError> {
        // Constant-product pools are read on their own below (R24).
        let (cp_specs, specs): (Vec<PoolSpec>, Vec<PoolSpec>) =
            specs.iter().cloned().partition(|s| s.venue.is_constant_product());
        let specs = &specs[..];
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
                    dynamic_fee: None,
                    seq: 0,
                    reserves: None,
                },
            );
        }

        // Slipstream's fees, after the states: which pools survived to need one
        // is known only now.
        let slip: Vec<(Address, Address, i32, u64)> = specs
            .iter()
            .zip(answers.chunks(STATE_READS))
            // Every dynamic-fee venue follows its TWAP.
            .filter(|(s, _)| !s.venue.fee_is_static())
            .filter_map(|(s, a)| {
                let p = pools.get(&s.pool)?;
                let cardinality = a[5].as_deref().and_then(|d| abi::word_uint(d, 3, 16)).unwrap_or(0);
                Some((s.pool, p.factory, p.state.tick_spacing, cardinality as u64))
            })
            .collect();
        for (pool, fee) in read_dynamic_fees(reads, &slip, block).await? {
            match fee {
                Ok(d) => {
                    if let Some(p) = pools.get_mut(&pool) {
                        p.dynamic_fee = Some(d);
                        p.fee_from_tick();
                    }
                }
                Err(read) => {
                    pools.remove(&pool);
                    refused.push(Unloaded { pool, why: Unloadable::FeeUnreadable { read } });
                }
            }
        }

        // Constant-product pools: their own reads, no ladder, no fee module.
        if !cp_specs.is_empty() {
            let calls: Vec<(Address, Vec<u8>)> = cp_specs.iter().flat_map(crate::live::cp::state_calls).collect();
            let answers = reads.multicall(&calls, block).await?;
            for (spec, a) in cp_specs.iter().zip(answers.chunks(crate::live::cp::READS)) {
                match crate::live::cp::decode_state(spec, a) {
                    Ok(loaded) => {
                        let code_hash = reads.code_hash(spec.pool, block).await?;
                        pools.insert(spec.pool, crate::live::cp::snapshot(spec, loaded, code_hash, block));
                    }
                    Err(why) => refused.push(Unloaded { pool: spec.pool, why }),
                }
            }
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
                // And a dynamic fee is the book's to state, from the tick.
                p.fee_from_tick();
                newest_read = newest_read.max(p.block);
                (p.spec.pool, Arc::new(p))
            })
            .collect::<BTreeMap<_, _>>();
        let universe = pools.values().map(|p| p.spec.clone()).collect();
        Self {
            pools: Versioned::new(pools, provenance),
            writer: Mutex::new(WriterState { seq: 0, last_gap: 0, newest_read }),
            universe,
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

    /// Follow each Slipstream pool's TWAP to `block`. The fee moves with it even
    /// when nothing swaps — ±15 ppm steps between swaps, measured — so without
    /// this the book's fee drifts off the module's by a tick's worth at a time.
    /// Returns how many pools' fees changed.
    ///
    /// An `observe` that reverts leaves the pool with no TWAP, which is the
    /// module's own answer to one: it catches the revert and adds no dynamic
    /// fee. A pool loaded without one stays without until it is reloaded.
    pub async fn refresh_twaps(&self, reads: &ChainReads, block: u64) -> Result<usize, ReadError> {
        let snap = self.snapshot();
        let targets: Vec<(Address, u32)> = snap
            .values()
            .filter_map(|p| p.dynamic_fee.filter(|d| d.twap_tick.is_some()).map(|d| (p.spec.pool, d.seconds_ago)))
            .collect();
        if targets.is_empty() {
            return Ok(0);
        }
        let calls: Vec<(Address, Vec<u8>)> = targets.iter().map(|(p, sa)| (*p, abi::call_observe(*sa))).collect();
        let answers = reads.multicall(&calls, block).await?;
        let twaps: Vec<(Address, Option<i32>)> = targets
            .iter()
            .zip(answers)
            .map(|((p, sa), a)| (*p, a.as_deref().and_then(|d| abi::decode_twap(d, *sa))))
            .collect();
        Ok(self.write(|map, w, status| {
            let mut changed = 0;
            for (pool, twap) in twaps {
                let Some(old) = map.get(&pool) else { continue };
                let Some(d) = old.dynamic_fee.filter(|d| d.twap_tick != twap) else { continue };
                let mut next = (**old).clone();
                next.dynamic_fee = Some(DynamicFee { twap_tick: twap, ..d });
                next.fee_from_tick();
                changed += usize::from(next.state.fee_ppm != old.state.fee_ppm);
                next.seq = w.seq;
                map.insert(pool, Arc::new(next));
            }
            (changed, status)
        }))
    }

    /// Re-read each constant-product pool's fee at `block` (R24): the
    /// factory's fee manager can set one per pool. Returns how many changed. A
    /// pool whose fee does not decode keeps the one it has.
    pub async fn refresh_cp_fees(&self, reads: &ChainReads, block: u64) -> Result<usize, ReadError> {
        let snap = self.snapshot();
        let targets: Vec<(Address, Address)> =
            snap.values().filter(|p| p.reserves.is_some()).map(|p| (p.spec.pool, p.spec.venue.factory())).collect();
        if targets.is_empty() {
            return Ok(0);
        }
        let calls: Vec<(Address, Vec<u8>)> =
            targets.iter().map(|(p, f)| (*f, abi::call_address_bool(selector::GET_FEE, *p, false))).collect();
        let answers = reads.multicall(&calls, block).await?;
        Ok(self.write(|map, w, status| {
            let mut changed = 0;
            for ((pool, _), a) in targets.iter().zip(answers) {
                let Some(fee) = a.as_deref().and_then(crate::live::cp::fee_ppm_from) else { continue };
                let Some(old) = map.get(pool).filter(|o| o.state.fee_ppm != fee) else { continue };
                let mut next = (**old).clone();
                next.state.fee_ppm = fee;
                next.seq = w.seq;
                map.insert(*pool, Arc::new(next));
                changed += 1;
            }
            (changed, status)
        }))
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
        self.apply_swaps(&[SwapWrite { pool, sqrt_price_x96, liquidity, tick, at }])[0]
    }

    /// Apply several `Swap` logs, in order, as **one** write: a reader sees the
    /// book before all of them or after all of them, never between two.
    ///
    /// A flashblock is published whole, so the state between two of its
    /// transactions is one no trade can ever land in — and a pricer that read
    /// it there would price a gap that never existed (R17: the first two
    /// routes ever to pay were exactly that). Each swap is judged newer or not
    /// against the pools as the swaps before it in `swaps` left them.
    pub fn apply_swaps(&self, swaps: &[SwapWrite]) -> Vec<SwapApplied> {
        self.apply_writes(&swaps.iter().map(|s| StateWrite::Swap(*s)).collect::<Vec<_>>())
    }

    /// Several logs' states, swaps and syncs, in order, as **one** write: a
    /// reader sees the book before all of them or after all of them (R24).
    pub fn apply_writes(&self, writes: &[StateWrite]) -> Vec<SwapApplied> {
        self.write(|map, w, status| {
            let applied = writes
                .iter()
                .map(|s| match s {
                    StateWrite::Swap(s) => Self::swap_into(map, w.seq, s),
                    StateWrite::Sync(s) => Self::sync_into(map, w.seq, s),
                })
                .collect();
            (applied, status)
        })
    }

    /// Apply a `Sync` log's reserves, if newer than what the pool holds.
    pub fn apply_sync(&self, pool: Address, reserve0: U256, reserve1: U256, at: LogPosition) -> SwapApplied {
        self.apply_writes(&[StateWrite::Sync(SyncWrite { pool, reserve0, reserve1, at })])[0]
    }

    /// Newer than the last log applied, or, for a pool whose state came from a
    /// read, from a later block than the read.
    fn is_newer(old: &PoolSnapshot, at: LogPosition) -> bool {
        match old.last_log {
            Some(last) => at > last,
            None => at.0 > old.block,
        }
    }

    /// A `Sync` replaces a constant-product pool's reserves outright; it is
    /// refused for any other pool.
    fn sync_into(map: &mut BTreeMap<Address, Arc<PoolSnapshot>>, seq: u64, s: &SyncWrite) -> SwapApplied {
        let Some(old) = map.get(&s.pool).filter(|o| o.reserves.is_some()) else { return SwapApplied::Unknown };
        if !Self::is_newer(old, s.at) {
            return SwapApplied::Stale;
        }
        let mut next = (**old).clone();
        next.reserves = Some(crate::live::cp::Reserves {
            reserve0: u256_to_ethers(s.reserve0),
            reserve1: u256_to_ethers(s.reserve1),
        });
        next.block = s.at.0;
        next.last_log = Some(s.at);
        next.seq = seq;
        map.insert(s.pool, Arc::new(next));
        SwapApplied::Updated
    }

    fn swap_into(map: &mut BTreeMap<Address, Arc<PoolSnapshot>>, seq: u64, s: &SwapWrite) -> SwapApplied {
        // A tick write is refused for a constant-product pool.
        let Some(old) = map.get(&s.pool).filter(|o| o.reserves.is_none()) else { return SwapApplied::Unknown };
        if !Self::is_newer(old, s.at) {
            return SwapApplied::Stale;
        }
        let mut next = (**old).clone();
        next.state.sqrt_price_x96 = u256_to_ethers(s.sqrt_price_x96);
        next.state.liquidity = s.liquidity;
        next.state.tick = s.tick;
        next.block = s.at.0;
        next.last_log = Some(s.at);
        // The swap moved the tick, and on Slipstream the fee with it.
        next.fee_from_tick();
        next.seq = seq;
        let covered = next.ladder_covers_price();
        map.insert(s.pool, Arc::new(next));
        if covered { SwapApplied::Updated } else { SwapApplied::NeedsReload }
    }

    /// A feed gap: everything may have missed updates. `Rebuilding` until a
    /// full [`Self::reload`] that **began after** this gap completes.
    pub fn mark_rebuilding(&self) {
        self.write(|_, w, _| {
            w.last_gap = w.seq;
            ((), ReconstructionStatus::Rebuilding)
        });
    }

    /// Re-read `pools` at `block` and replace them; if `pools` is empty, every
    /// pool the book was built over.
    ///
    /// **A full reload reads the universe, not what the book still holds.** A
    /// pool that fails a read is removed, and a full reload over only the pools
    /// left would never read it again: a 14-day run's universe would erode one
    /// transient failure at a time. So a removed pool comes back at the next
    /// full reload that reads it.
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
        let full = pools.is_empty();
        let specs: Vec<PoolSpec> = if full {
            self.universe.clone()
        } else {
            self.snapshot().values().filter(|p| pools.contains(&p.spec.pool)).map(|p| p.spec.clone()).collect()
        };
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
                    snap.reserves = old.reserves;
                    // The fresh regime at the kept tick.
                    snap.fee_from_tick();
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
