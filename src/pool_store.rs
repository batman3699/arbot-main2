use anyhow::{Context, Result};
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{create_dir_all, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct PoolRecord {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    #[allow(dead_code)]
    pub created_block: u64,
    /// Hub-side USD liquidity from offline ranking (`rank_base_pools.py`).
    /// When present, cold-pool truncation prefers highest-liquidity pools.
    pub hub_usd_liquidity: Option<f64>,
    /// The pool's REAL swap fee in ppm, when a builder could determine it.
    ///
    /// Deliberately separate from `fee`, which is the venue's POOL KEY: a fee
    /// tier on univ3, where the two coincide, but a TICK SPACING on Slipstream,
    /// where they do not. Slipstream fees are dynamic and not derivable from the
    /// key -- measured pools at spacing 100 charge 212 and 2500 ppm, and spacing
    /// 200 charges 8000. Any ranking that reads `fee` as a cost is therefore
    /// wrong for Slipstream, which is how the runtime came to price those pools
    /// at their tick spacing.
    ///
    /// `None` means unknown, never cheap.
    pub fee_ppm_onchain: Option<u32>,
    /// Which hub token `hub_usd_liquidity` was measured against.
    ///
    /// Load-bearing, not decoration: a writer that never identified a hub
    /// cannot have measured the hub side, so its absence means the number in
    /// `hub_usd_liquidity` is some OTHER quantity. See
    /// `trusted_hub_usd_liquidity`.
    pub hub_symbol: Option<String>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct ResolvedUniV2PoolCfg {
    pub pair: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub fee_bps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PoolRecordJson {
    pool: String,
    token0: String,
    token1: String,
    fee: u32,
    created_block: u64,
    #[serde(default)]
    hub_usd_liquidity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hub_symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fee_ppm_onchain: Option<u32>,
}

impl PoolRecord {
    fn from_json(record: PoolRecordJson) -> Result<Self> {
        Ok(Self {
            pool: Address::from_str(record.pool.trim()).context("pool must be a valid address")?,
            token0: Address::from_str(record.token0.trim())
                .context("token0 must be a valid address")?,
            token1: Address::from_str(record.token1.trim())
                .context("token1 must be a valid address")?,
            fee: record.fee,
            created_block: record.created_block,
            hub_usd_liquidity: record.hub_usd_liquidity,
            hub_symbol: record.hub_symbol,
            fee_ppm_onchain: record.fee_ppm_onchain,
        })
    }

    #[allow(dead_code)]
    fn to_json(&self) -> PoolRecordJson {
        PoolRecordJson {
            pool: format!("0x{}", hex::encode(self.pool)),
            token0: format!("0x{}", hex::encode(self.token0)),
            token1: format!("0x{}", hex::encode(self.token1)),
            fee: self.fee,
            created_block: self.created_block,
            hub_usd_liquidity: self.hub_usd_liquidity,
            hub_symbol: self.hub_symbol.clone(),
            fee_ppm_onchain: self.fee_ppm_onchain,
        }
    }
}

pub fn pool_data_path(chain: &str, venue: &str) -> PathBuf {
    let chain = chain.to_ascii_lowercase();
    let venue = venue.to_ascii_lowercase();

    if let Ok(root) = std::env::var("POOL_DATA_ROOT") {
        return PathBuf::from(root)
            .join(&chain)
            .join(&venue)
            .join("pools.jsonl");
    }

    let absolute = PathBuf::from("/data")
        .join(&chain)
        .join(&venue)
        .join("pools.jsonl");
    if absolute.exists() {
        return absolute;
    }

    PathBuf::from("data")
        .join(chain)
        .join(venue)
        .join("pools.jsonl")
}

pub fn load_pool_records(path: impl AsRef<Path>) -> Result<Vec<PoolRecord>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line.context("read pool record line")?;
        if line.trim().is_empty() {
            continue;
        }
        let json: PoolRecordJson =
            serde_json::from_str(&line).context("decode pool record json")?;
        records.push(PoolRecord::from_json(json)?);
    }
    Ok(records)
}

#[allow(dead_code)]
pub fn write_pool_records(path: impl AsRef<Path>, records: &[PoolRecord]) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        create_dir_all(parent)
            .with_context(|| format!("create pool store dir {}", parent.display()))?;
    }
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for record in records {
        let json = serde_json::to_string(&record.to_json()).context("encode pool record json")?;
        writer
            .write_all(json.as_bytes())
            .context("write pool record")?;
        writer
            .write_all(b"\n")
            .context("write pool record newline")?;
    }
    writer.flush().context("flush pool record file")?;
    Ok(())
}

/// Values above this are almost certainly raw UniV3 `liquidity()` scores, not USD.
///
/// Was 1e12. That is roughly total DeFi TVL across every chain, so it only ever
/// caught the most extreme corruption and let merely absurd values rank first.
/// Measured 2026-09-06 in `data/base/uniswap_v3/pools.jsonl`: four records
/// carried fabricated liquidity of 3.2e18, 1.7e12, 8.2e11 and 8.2e11 USD. The
/// old cap rejected the first two and passed the other two, which then ranked
/// #1 and #2 in the hot-pool list -- the exact outcome the cap exists to
/// prevent. Re-measured on-chain, those four pools hold $262, $1.50, $0.02 and
/// $0.02.
///
/// 1e11 is $100 billion in a single pool. The largest value in any inventory in
/// this repo is $2.4e9, the largest pool that has ever existed on any chain is
/// single-digit billions, and total DeFi TVL is around $1e11 -- so no real pool
/// can approach this, while every value in the corrupted cohort is caught.
const MAX_SANE_HUB_USD_LIQUIDITY: f64 = 1e11;

pub(crate) fn sanitize_hub_usd_liquidity(value: Option<f64>) -> Option<f64> {
    value.filter(|usd| usd.is_finite() && *usd > 0.0 && *usd <= MAX_SANE_HUB_USD_LIQUIDITY)
}

/// The offline hub-side USD for a record, or `None` if it cannot be trusted.
///
/// A record must carry BOTH the number and the hub it was measured against.
/// `hub_symbol` is not decoration: the writers that omit it
/// (`Convert.py`, `scripts/aerodrome_*_99k.py`, `scripts/pancakeswap_v3_99k.py`)
/// never identify a hub token at all. They write GeckoTerminal's
/// `reserve_in_usd` -- WHOLE-POOL TVL -- into a field that means the USD value
/// of the hub token's balance. Roughly double for a balanced pool, and
/// unrelated for a concentrated-liquidity pool priced out of range.
///
/// Audited on-chain 2026-09-06 across the 32 records in
/// `data/base/aerodrome_slipstream_gauge`: 23 sat at a plausible 1-4x (the
/// whole-pool-vs-hub-side factor) and 9 were fabricated, including the six
/// largest claims in the file. The pool claiming $2,364,678,243 holds $0.04 of
/// WETH; the one claiming $481,706,729 holds $0.000011 of USDC. Being the
/// largest numbers in their venue, they ranked FIRST.
///
/// Returning None here is not a loss: `univ3_hub_usd_liquidity_score` falls
/// through to a live `balanceOf`, which is the correct value. It costs one RPC
/// call per such record (186 across four Base inventories as of this writing).
pub(crate) fn trusted_hub_usd_liquidity(record: &PoolRecord) -> Option<f64> {
    record.hub_symbol.as_ref()?;
    sanitize_hub_usd_liquidity(record.hub_usd_liquidity)
}

/// Keep the most liquid cold-pool candidates when inventory exceeds the cap.
/// Prefers `hub_usd_liquidity` from offline ranking; falls back to newest
/// `created_block` when liquidity metadata is absent.
#[allow(dead_code)]
pub fn prioritize_cold_pool_inventory(records: &mut Vec<PoolRecord>, max_cold: usize) {
    if records.len() <= max_cold {
        return;
    }
    records.sort_by(|left, right| {
        // Same trust rule as the ranker. Sorting on an untrusted number would
        // keep exactly the pools that claim the most and hold the least.
        match (
            trusted_hub_usd_liquidity(left),
            trusted_hub_usd_liquidity(right),
        ) {
            (Some(l), Some(r)) => r
                .partial_cmp(&l)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.created_block.cmp(&left.created_block)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right.created_block.cmp(&left.created_block),
        }
    });
    records.truncate(max_cold);
}

#[allow(dead_code)]
pub fn merge_pool_records(existing: Vec<PoolRecord>, incoming: Vec<PoolRecord>) -> Vec<PoolRecord> {
    let mut merged: HashMap<Address, PoolRecord> = HashMap::new();
    for record in existing.into_iter().chain(incoming) {
        merged
            .entry(record.pool)
            .and_modify(|entry| {
                if record.created_block < entry.created_block {
                    entry.created_block = record.created_block;
                }
                entry.fee = record.fee;
                entry.token0 = record.token0;
                entry.token1 = record.token1;
                if record.hub_usd_liquidity.is_some() {
                    entry.hub_usd_liquidity = record.hub_usd_liquidity;
                }
            })
            .or_insert(record);
    }
    let mut records: Vec<PoolRecord> = merged.into_values().collect();
    records.sort_by_key(|record| (record.created_block, record.pool));
    records
}

#[allow(dead_code)]
pub fn univ2_configs_from_records(records: &[PoolRecord]) -> Vec<ResolvedUniV2PoolCfg> {
    let mut configs = Vec::with_capacity(records.len().saturating_mul(2));
    for record in records {
        configs.push(ResolvedUniV2PoolCfg {
            pair: record.pool,
            token_in: record.token0,
            token_out: record.token1,
            fee_bps: record.fee,
        });
        configs.push(ResolvedUniV2PoolCfg {
            pair: record.pool,
            token_in: record.token1,
            token_out: record.token0,
            fee_bps: record.fee,
        });
    }
    configs
}

#[cfg(test)]
mod tests {
    use super::{load_pool_records, merge_pool_records, prioritize_cold_pool_inventory, write_pool_records, PoolRecord};
    use ethers::types::Address;
    use tempfile::tempdir;

    #[test]
    fn writes_and_reads_pool_records() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("pools.jsonl");
        let records = vec![PoolRecord {
            pool: Address::from_low_u64_be(1),
            token0: Address::from_low_u64_be(2),
            token1: Address::from_low_u64_be(3),
            fee: 30,
            created_block: 12,
            hub_usd_liquidity: Some(1_000_000.0),
            hub_symbol: Some("WETH".to_string()),
            fee_ppm_onchain: None,
        }];
        write_pool_records(&path, &records).expect("write");
        let loaded = load_pool_records(&path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pool, records[0].pool);
        assert_eq!(loaded[0].fee, records[0].fee);
    }

    #[test]
    fn merges_pool_records_by_address() {
        let pool = Address::from_low_u64_be(42);
        let existing = vec![PoolRecord {
            pool,
            token0: Address::from_low_u64_be(11),
            token1: Address::from_low_u64_be(12),
            fee: 30,
            created_block: 50,
            hub_usd_liquidity: None,
            hub_symbol: None,
            fee_ppm_onchain: None,
        }];
        let incoming = vec![PoolRecord {
            pool,
            token0: Address::from_low_u64_be(21),
            token1: Address::from_low_u64_be(22),
            fee: 25,
            created_block: 20,
            hub_usd_liquidity: Some(2_000_000.0),
            hub_symbol: Some("WETH".to_string()),
            fee_ppm_onchain: None,
        }];
        let merged = merge_pool_records(existing, incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].created_block, 20);
        assert_eq!(merged[0].fee, 25);
    }

    #[test]
    fn prioritize_cold_pool_inventory_prefers_hub_liquidity() {
        let mut pools = vec![
            PoolRecord {
                pool: Address::from_low_u64_be(1),
                token0: Address::from_low_u64_be(2),
                token1: Address::from_low_u64_be(3),
                fee: 500,
                created_block: 99,
                hub_usd_liquidity: Some(10_000.0),
                hub_symbol: Some("WETH".to_string()),
                fee_ppm_onchain: None,
            },
            PoolRecord {
                pool: Address::from_low_u64_be(4),
                token0: Address::from_low_u64_be(5),
                token1: Address::from_low_u64_be(6),
                fee: 500,
                created_block: 1,
                hub_usd_liquidity: Some(5_000_000.0),
                hub_symbol: Some("WETH".to_string()),
                fee_ppm_onchain: None,
            },
        ];
        prioritize_cold_pool_inventory(&mut pools, 1);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool, Address::from_low_u64_be(4));
    }

    #[test]
    fn prioritize_ignores_corrupt_hub_liquidity() {
        let mut pools = vec![
            PoolRecord {
                pool: Address::from_low_u64_be(1),
                token0: Address::from_low_u64_be(2),
                token1: Address::from_low_u64_be(3),
                fee: 500,
                created_block: 99,
                hub_usd_liquidity: Some(1e33),
                hub_symbol: Some("WETH".to_string()),
                fee_ppm_onchain: None,
            },
            PoolRecord {
                pool: Address::from_low_u64_be(4),
                token0: Address::from_low_u64_be(5),
                token1: Address::from_low_u64_be(6),
                fee: 500,
                created_block: 1,
                hub_usd_liquidity: Some(5_000_000.0),
                hub_symbol: Some("WETH".to_string()),
                fee_ppm_onchain: None,
            },
        ];
        prioritize_cold_pool_inventory(&mut pools, 1);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool, Address::from_low_u64_be(4));
    }

    #[test]
    fn pool_data_path_prefers_configured_root() {
        std::env::set_var("POOL_DATA_ROOT", "/tmp/arbot-pools");
        let path = super::pool_data_path("Ethereum", "Uniswap_V3");
        std::env::remove_var("POOL_DATA_ROOT");
        assert_eq!(
            path,
            std::path::PathBuf::from("/tmp/arbot-pools/ethereum/uniswap_v3/pools.jsonl")
        );
    }
}
