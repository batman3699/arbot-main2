//! The universe the shadow run prices, from the pool inventories.
//!
//! # Which universe, and why this one
//!
//! The event census is the only measurement in this repository that found
//! net-positive samples, and it priced exactly this shape: **pairs carrying two
//! or more pools at ≤ 500 ppm and ≥ $100k depth**, each pair a set of 2-hop
//! cycles. Both thresholds were measured rather than chosen — see
//! `scripts/data/event_census.py` and `sweep_depth_floor.py`.
//!
//! Narrowed to the two venues the Phase 5 executor can reach: Uniswap v3 through
//! its `UNIV3` op, and Aerodrome Slipstream through `GENERIC` adapter 1. The
//! census also priced PancakeSwap v3 and a second Slipstream deployment; each
//! needs its own adapter registration before a route through it could execute,
//! and pricing routes that cannot execute would fill the funnel with trades
//! nothing could make.
//!
//! # The inventories are not trusted
//!
//! They live in `data/`, outside git, and one of them has already been found
//! carrying fabricated addresses. So this module only *proposes* a universe; the
//! pool book reads every pool's tokens and factory from the chain and refuses
//! any that disagree, and admission refuses any whose factory is not the
//! venue's.

use alloy_primitives::{address, Address};
use apex_types::ids::VenueId;
use apex_venues::adapter::venue_ids;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Venue {
    UniswapV3,
    Slipstream,
}

impl Venue {
    pub const ALL: [Self; 2] = [Self::UniswapV3, Self::Slipstream];

    pub const fn id(self) -> VenueId {
        match self {
            Self::UniswapV3 => venue_ids::UNISWAP_V3,
            Self::Slipstream => venue_ids::AERODROME_SLIPSTREAM,
        }
    }

    /// The inventory directory under `data/base/`.
    pub const fn dir(self) -> &'static str {
        match self {
            Self::UniswapV3 => "uniswap_v3",
            Self::Slipstream => "aerodrome_slipstream",
        }
    }

    /// The factory every pool of this venue on Base was deployed by. Both read
    /// from the chain 2026-09-30: SwapRouter02's `factory()` for Uniswap, and
    /// Slipstream's router's `factory()` for Slipstream.
    pub const fn factory(self) -> Address {
        match self {
            Self::UniswapV3 => address!("33128a8fC17869897dcE68Ed026d694621f6FDfD"),
            Self::Slipstream => address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"),
        }
    }

    /// Uniswap v3 fixes a pool's fee at creation. Slipstream's fee module can
    /// change it — pools at one tick spacing were measured charging 212 and
    /// 2,500 ppm — so its fee is read with the rest of the state, every time.
    pub const fn fee_is_static(self) -> bool {
        matches!(self, Self::UniswapV3)
    }
}

/// One pool the inventory proposes.
#[derive(Clone, Debug, PartialEq)]
pub struct PoolSpec {
    pub pool: Address,
    pub venue: Venue,
    pub token0: Address,
    pub token1: Address,
    /// The fee the inventory measured. Re-read from the pool at load.
    pub fee_ppm: u32,
    /// From a third-party feed: ranks and filters, never sizes (§18.5).
    pub depth_usd: f64,
}

impl PoolSpec {
    /// The pair, lower address first, so both orientations name one pair.
    pub fn pair(&self) -> (Address, Address) {
        if self.token0 <= self.token1 {
            (self.token0, self.token1)
        } else {
            (self.token1, self.token0)
        }
    }
}

/// The census's thresholds, both measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UniverseFilter {
    /// A fee ceiling: 500 ppm is 5 bps a hop, the band the tradeable cross-venue
    /// pairs occupy.
    pub max_fee_ppm: u32,
    /// $100k: the same hit rate as every lower floor, against a third of the
    /// pools (`sweep_depth_floor.py`).
    pub min_depth_usd: f64,
}

impl Default for UniverseFilter {
    fn default() -> Self {
        Self { max_fee_ppm: 500, min_depth_usd: 100_000.0 }
    }
}

#[derive(Debug)]
pub enum InventoryError {
    Io { path: String, detail: String },
    Parse { path: String, line: usize, detail: String },
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, detail } => write!(f, "{path}: {detail}"),
            Self::Parse { path, line, detail } => write!(f, "{path}:{line}: {detail}"),
        }
    }
}

impl std::error::Error for InventoryError {}

#[derive(Deserialize)]
struct Record {
    pool: Address,
    token0: Address,
    token1: Address,
    #[serde(default)]
    fee: Option<u64>,
    #[serde(default)]
    fee_ppm_onchain: Option<u64>,
    #[serde(default)]
    hub_usd_liquidity: Option<f64>,
}

/// Every pool of the two venues that passes `filter`.
///
/// A Slipstream record without a measured fee is **skipped, not guessed**: its
/// `fee` field is the tick spacing, and reading it as a fee would admit a
/// 200-spacing pool as a 200 ppm one. Uniswap's `fee` is the fee tier, so it may
/// stand in — the census's own rule.
pub fn load(data_dir: &Path, filter: UniverseFilter) -> Result<Vec<PoolSpec>, InventoryError> {
    let mut out = Vec::new();
    for venue in Venue::ALL {
        let path = data_dir.join(venue.dir()).join("pools.jsonl");
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(&path)
            .map_err(|e| InventoryError::Io { path: shown.clone(), detail: e.to_string() })?;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let r: Record = serde_json::from_str(line).map_err(|e| InventoryError::Parse {
                path: shown.clone(),
                line: i + 1,
                detail: e.to_string(),
            })?;
            let fee = match (r.fee_ppm_onchain, venue) {
                (Some(f), _) => f,
                (None, Venue::UniswapV3) => match r.fee {
                    Some(f) => f,
                    None => continue,
                },
                (None, Venue::Slipstream) => continue,
            };
            let Ok(fee) = u32::try_from(fee) else { continue };
            let depth = r.hub_usd_liquidity.unwrap_or(0.0);
            if fee == 0 || fee > filter.max_fee_ppm || depth < filter.min_depth_usd {
                continue;
            }
            out.push(PoolSpec {
                pool: r.pool,
                venue,
                token0: r.token0,
                token1: r.token1,
                fee_ppm: fee,
                depth_usd: depth,
            });
        }
    }
    Ok(out)
}

/// Pairs carrying at least two pools — each such pair is a set of 2-hop cycles.
/// A pair with one pool has nothing to arbitrage against.
pub fn pairs(specs: &[PoolSpec]) -> BTreeMap<(Address, Address), Vec<PoolSpec>> {
    let mut by_pair: BTreeMap<(Address, Address), Vec<PoolSpec>> = BTreeMap::new();
    for s in specs {
        by_pair.entry(s.pair()).or_default().push(s.clone());
    }
    by_pair.retain(|_, v| v.len() >= 2);
    by_pair
}
