//! `canary.jsonl`: one JSON object a line, append-only, every line synced.
//!
//! **Write-ahead.** A send record is on disk before the transaction leaves the
//! process, so a crash can never leave a sent transaction the canary does not
//! know about. A crash mid-append leaves a torn **last** line; nothing was sent
//! after it, so it is ignored. A torn line anywhere else is damage, and the
//! canary refuses to open over it.

use alloy_primitives::{B256, U256};
use apex_types::ids::{ChainId, StrategyId, VenueId};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Sent(Sent),
    Outcome(Outcome),
    Halted { at_ms: u64, halt: Halt },
    /// The operator's, with the service stopped (`scripts/canary-reset.sh`).
    Reset { at_ms: u64 },
}

/// Everything known about a transaction before it leaves: enough to reconcile
/// its receipt after a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub ticket: u64,
    pub nonce: u64,
    pub tx: B256,
    pub at_ms: u64,
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub route_hash: B256,
    pub venues: Vec<VenueId>,
    pub gas_limit: u64,
    #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
    pub max_fee_per_gas: u128,
    #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
    pub max_priority_fee_per_gas: u128,
    pub expected_gross_wei: U256,
    #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
    pub expected_net_wei: i128,
    pub waived: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub tx: B256,
    pub at_ms: u64,
    pub settled: Settled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Settled {
    Landed {
        block: u64,
        gas_used: u64,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        effective_gas_price_wei: u128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        l1_fee_wei: u128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        gross_wei: i128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        cost_wei: u128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        net_wei: i128,
    },
    Reverted {
        block: u64,
        gas_used: u64,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        effective_gas_price_wei: u128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        l1_fee_wei: u128,
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        cost_wei: u128,
        class: String,
        /// Under 100,000 gas: the deadline, the commitment or the caller.
        early: bool,
    },
    /// The endpoint answered no. Nothing reached the chain.
    Refused { detail: String },
    /// No answer: it may or may not have left.
    Silent { detail: String },
    /// No receipt by the timeout.
    Unresolved { detail: String },
    /// Landed, and the executor's transfers and its event disagree.
    Unexplained { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "halt", rename_all = "snake_case")]
pub enum Halt {
    LossCap {
        #[serde(serialize_with = "dec::ser", deserialize_with = "dec::de")]
        lost_wei: u128,
    },
    Reverts { in_a_row: u32 },
    Refusals { in_a_row: u32 },
    Unresolved { tx: B256 },
    Unexplained { tx: B256 },
    Tripwire { boot: u64, now: u64, sent: u64 },
    Done { landed: u32 },
}

impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LossCap { lost_wei } => write!(f, "loss cap: {lost_wei} wei lost on losing trades"),
            Self::Reverts { in_a_row } => write!(f, "{in_a_row} reverts in a row"),
            Self::Refusals { in_a_row } => write!(f, "{in_a_row} refused sends in a row"),
            Self::Unresolved { tx } => write!(f, "{tx} is unresolved"),
            Self::Unexplained { tx } => write!(f, "{tx} landed and its transfers disagree with its event"),
            Self::Tripwire { boot, now, sent } => {
                write!(f, "the trader's nonce is {now}, beyond {boot} at boot plus {sent} sends: the key is in use elsewhere")
            }
            Self::Done { landed } => write!(f, "done: {landed} trades landed"),
        }
    }
}

/// 128-bit amounts as decimal strings. serde's internally tagged enums cannot
/// carry a u128 or an i128 through the buffer they parse into, and `jq` reads a
/// string exactly where it would round a number above 2^53.
mod dec {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn ser<T: std::fmt::Display, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(v)
    }

    pub fn de<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
        D: Deserializer<'de>,
    {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

/// The open file, and every record it held when opened.
pub struct Ledger {
    file: Mutex<File>,
}

impl Ledger {
    pub fn open(path: &Path) -> io::Result<(Self, Vec<Record>)> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut records = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            match serde_json::from_str(line) {
                Ok(r) => records.push(r),
                // Torn by a crash mid-append: nothing was sent after it.
                Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => break,
                Err(e) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("canary ledger line {}: {e}", i + 1)))
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok((Self { file: Mutex::new(file) }, records))
    }

    /// Append one record and sync it. Returns only once it is on disk.
    pub fn append(&self, r: &Record) -> io::Result<()> {
        let mut line = serde_json::to_string(r).map_err(io::Error::other)?;
        line.push('\n');
        let mut f = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.write_all(line.as_bytes())?;
        f.sync_data()
    }
}
