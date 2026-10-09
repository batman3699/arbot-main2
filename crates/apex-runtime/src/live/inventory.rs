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
//! Three venues: Uniswap v3, reached through the executor's `UNIV3` op;
//! Aerodrome Slipstream, through `GENERIC` adapter 1; and PancakeSwap v3,
//! through adapter 2. **PancakeSwap is here on the census's evidence**: every
//! net-positive sample its four runs recorded was WETH/USDC, and the largest
//! share of them — 45 of 87 at the $100k floor — paired a PancakeSwap pool with
//! a Uniswap one. The census also priced a second Slipstream deployment, which
//! produced none and is not here.
//!
//! A venue is only priced once the executor can reach it: pricing routes that
//! cannot execute would fill the funnel with trades nothing could make. So the
//! shadow run reads the executor's adapter registrations at boot and leaves out
//! any venue whose adapter is missing (`live::calls::reachable_venues`).
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
    PancakeV3,
    /// Aerodrome's second Slipstream deployment, factory `0xf8f2…`: the same
    /// pool and router code as the first, with its own pools, router and fee
    /// module (R22). Named after its inventory directory.
    SlipstreamV3,
    /// Aerodrome v2's volatile (x·y=k) pools, factory `0x420D…` (R24). Stable
    /// pools are refused.
    AerodromeV2,
}

impl Venue {
    pub const ALL: [Self; 5] =
        [Self::UniswapV3, Self::Slipstream, Self::PancakeV3, Self::SlipstreamV3, Self::AerodromeV2];

    pub const fn id(self) -> VenueId {
        match self {
            Self::UniswapV3 => venue_ids::UNISWAP_V3,
            Self::Slipstream => venue_ids::AERODROME_SLIPSTREAM,
            Self::PancakeV3 => venue_ids::PANCAKESWAP_V3,
            Self::SlipstreamV3 => venue_ids::AERODROME_SLIPSTREAM_V3,
            Self::AerodromeV2 => venue_ids::AERODROME_VOLATILE,
        }
    }

    /// The inventory directory under `data/base/`.
    pub const fn dir(self) -> &'static str {
        match self {
            Self::UniswapV3 => "uniswap_v3",
            Self::Slipstream => "aerodrome_slipstream",
            Self::PancakeV3 => "pancakeswap_v3",
            Self::SlipstreamV3 => "aerodrome_slipstream_v3",
            Self::AerodromeV2 => "aerodrome_v2",
        }
    }

    /// The factory every pool of this venue on Base was deployed by, read from
    /// the chain: SwapRouter02's `factory()` for Uniswap and Slipstream's
    /// router's for Slipstream (2026-09-30); for PancakeSwap, the `factory()`
    /// of its WETH/USDC pools and of its `SmartRouter` (2026-10-03); for
    /// Slipstream's second deployment, its router's (2026-10-08); for Aerodrome
    /// v2, its router's `defaultFactory()` (2026-10-09).
    pub const fn factory(self) -> Address {
        match self {
            Self::UniswapV3 => address!("33128a8fC17869897dcE68Ed026d694621f6FDfD"),
            Self::Slipstream => address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"),
            Self::PancakeV3 => address!("0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865"),
            Self::SlipstreamV3 => address!("f8f2eB4940CFE7d13603DDDD87f123820Fc061Ef"),
            Self::AerodromeV2 => address!("420DD381b31aEf6683db6B902084cB0FFECe40Da"),
        }
    }

    /// Uniswap and PancakeSwap fix a pool's fee at creation. Slipstream's fee
    /// module can change it — pools at one tick spacing were measured charging
    /// 212 and 2,500 ppm — so its fee is read with the rest of the state, every
    /// time. Aerodrome v2's factory sets a pool's fee, which no swap moves; the
    /// book re-reads it each head (R24).
    pub const fn fee_is_static(self) -> bool {
        matches!(self, Self::UniswapV3 | Self::PancakeV3 | Self::AerodromeV2)
    }

    /// A constant-product venue: reserves and a fee, no ticks (R24).
    pub const fn is_constant_product(self) -> bool {
        matches!(self, Self::AerodromeV2)
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
    /// Aerodrome v2's curve flag (R24).
    #[serde(default)]
    stable: Option<bool>,
}

/// Every pool of the venues that passes `filter`.
///
/// A Slipstream record without a measured fee is **skipped, not guessed**: its
/// `fee` field is the tick spacing, and reading it as a fee would admit a
/// 200-spacing pool as a 200 ppm one. Uniswap's and PancakeSwap's `fee` is the
/// fee tier, so it may stand in — the census's own rule.
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
            // Aerodrome v2: volatile pools only. A stable pool's curve is not
            // the one priced, and a record that does not say is not trusted.
            if venue.is_constant_product() && r.stable != Some(false) {
                continue;
            }
            let fee = match (r.fee_ppm_onchain, venue) {
                (Some(f), _) => f,
                // A dynamic-fee record's `fee` is its tick spacing: skipped,
                // never guessed.
                (None, v) if !v.fee_is_static() => continue,
                (None, _) => match r.fee {
                    Some(f) => f,
                    None => continue,
                },
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
