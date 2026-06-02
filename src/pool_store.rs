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

#[allow(dead_code)]
pub fn merge_pool_records(existing: Vec<PoolRecord>, incoming: Vec<PoolRecord>) -> Vec<PoolRecord> {
    let mut merged: HashMap<Address, PoolRecord> = HashMap::new();
    for record in existing.into_iter().chain(incoming.into_iter()) {
        merged
            .entry(record.pool)
            .and_modify(|entry| {
                if record.created_block < entry.created_block {
                    entry.created_block = record.created_block;
                }
                entry.fee = record.fee;
                entry.token0 = record.token0;
                entry.token1 = record.token1;
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
    use super::{load_pool_records, merge_pool_records, write_pool_records, PoolRecord};
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
        }];
        let incoming = vec![PoolRecord {
            pool,
            token0: Address::from_low_u64_be(21),
            token1: Address::from_low_u64_be(22),
            fee: 25,
            created_block: 20,
        }];
        let merged = merge_pool_records(existing, incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].created_block, 20);
        assert_eq!(merged[0].fee, 25);
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
