//! Armed lane v2's write-ahead live ledger and replayable risk state (Task 8.5 R36).
//!
//! Every `Submitting` record is synced before the transaction is sent. A restart
//! can therefore recover uncertain sends, their nonce slots, and their reserved
//! worst-case exposure. The last unterminated line is treated as a torn append
//! and truncated before the file is reopened for appending.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveRecord {
    /// Write-ahead intent. The worker must not send unless this append has
    /// returned successfully (including `sync_data`).
    Submitting {
        arm_id: u64,
        pools: [Address; 2],
        sender: Address,
        nonce: u64,
        tx: B256,
        to_block: u64,
        gas_limit: u64,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        max_fee_per_gas: u128,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        max_priority_fee_per_gas: u128,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        worst_case_wei: u128,
        at_ms: u64,
    },
    /// `accepted = None` means the endpoint was silent: this submission may
    /// have reached the sequencer and must retain its reservation.
    Answered {
        tx: B256,
        answer: String,
        accepted: Option<bool>,
        at_ms: u64,
    },
    Landed {
        arm_id: u64,
        pools: [Address; 2],
        sender: Address,
        nonce: u64,
        tx: B256,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        gross_wei: u128,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        paid_wei: u128,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        cost_wei: u128,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        net_wei: i128,
        at_ms: u64,
    },
    /// `tx`'s bound has passed with the sender's nonce unmoved, so it can no
    /// longer land; with no `tx`, every hash at this sender/nonce has.
    Expired {
        sender: Address,
        nonce: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tx: Option<B256>,
        at_ms: u64,
    },
    Skipped {
        arm_id: u64,
        sender: Address,
        reason: String,
        at_ms: u64,
    },
    Halted { reason: Halt, at_ms: u64 },
    /// Operator acknowledgement. Clears a tripwire, but deliberately does not
    /// clear a same-day loss-cap halt: that halt expires at the next UTC day.
    Reset { at_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "halt", rename_all = "snake_case")]
pub enum Halt {
    LossCap { day: String },
    Tripwire { sender: Address, detail: String },
}

/// A single potentially executable hash within a `(sender, nonce)` slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTx {
    pub arm_id: u64,
    pub pools: [Address; 2],
    pub tx: B256,
    pub to_block: u64,
    pub worst_case_wei: u128,
    pub accepted: Option<bool>,
}

/// The risk state reconstructed from the append-only live ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveState {
    pub pending: BTreeMap<(Address, u64), Vec<PendingTx>>,
    pub day: String,
    pub day_net: i128,
    pub reserved_wei: u128,
    pub halted: Option<Halt>,
    loss_capped: bool,
    /// The last tripwire not yet reset: it outlives the day.
    tripwire: Option<Halt>,
}

impl LiveState {
    /// Nothing pending, nothing halted, on `now_ms`'s UTC day.
    pub fn new(now_ms: u64) -> Self {
        Self {
            pending: BTreeMap::new(),
            day: utc_day(now_ms),
            day_net: 0,
            reserved_wei: 0,
            halted: None,
            loss_capped: false,
            tripwire: None,
        }
    }

    /// Replay the ledger as of `now_ms`.
    ///
    /// A nonce may have multiple submitted hashes due to validity transaction
    /// replacement. At most one can land, so `reserved_wei` charges only the
    /// largest still-possible worst case per `(sender, nonce)`. A refused hash
    /// is removed independently; silence remains uncertain and stays pending.
    pub fn from_records(records: &[LiveRecord], now_ms: u64, budget_wei: u128) -> Self {
        let mut state = Self::new(now_ms);
        for record in records {
            state.apply(record, budget_wei);
        }
        state
    }

    /// Fold one more record in, on the state's day: the live worker applies
    /// each record as it appends it.
    pub fn apply(&mut self, record: &LiveRecord, budget_wei: u128) {
        match record {
            LiveRecord::Submitting {
                arm_id,
                pools,
                sender,
                nonce,
                tx,
                to_block,
                worst_case_wei,
                ..
            } => {
                let pending = self.pending.entry((*sender, *nonce)).or_default();
                // Idempotence protects replay from a duplicate append/record.
                if !pending.iter().any(|p| p.tx == *tx) {
                    pending.push(PendingTx {
                        arm_id: *arm_id,
                        pools: *pools,
                        tx: *tx,
                        to_block: *to_block,
                        worst_case_wei: *worst_case_wei,
                        accepted: None,
                    });
                }
            }
            LiveRecord::Answered { tx, accepted, .. } => {
                let mut empty_key = None;
                for (key, pending) in self.pending.iter_mut() {
                    if let Some(index) = pending.iter().position(|p| p.tx == *tx) {
                        if *accepted == Some(false) {
                            pending.remove(index);
                        } else {
                            pending[index].accepted = *accepted;
                        }
                        if pending.is_empty() {
                            empty_key = Some(*key);
                        }
                        break;
                    }
                }
                if let Some(key) = empty_key {
                    self.pending.remove(&key);
                }
            }
            LiveRecord::Landed { sender, nonce, net_wei, at_ms, .. } => {
                // A landed hash resolves every replacement sharing its nonce.
                self.pending.remove(&(*sender, *nonce));
                if utc_day(*at_ms) == self.day {
                    self.day_net = self.day_net.saturating_add(*net_wei);
                    // Infer a missing Halted append after a crash between a
                    // synced Landed record and the explicit Halted record.
                    if self.day_net < 0 && self.day_net.unsigned_abs() >= budget_wei {
                        self.loss_capped = true;
                    }
                }
            }
            LiveRecord::Expired { sender, nonce, tx, .. } => {
                let key = (*sender, *nonce);
                match tx {
                    None => {
                        self.pending.remove(&key);
                    }
                    Some(tx) => {
                        if let Some(pending) = self.pending.get_mut(&key) {
                            pending.retain(|p| p.tx != *tx);
                            if pending.is_empty() {
                                self.pending.remove(&key);
                            }
                        }
                    }
                }
            }
            LiveRecord::Skipped { .. } => {}
            LiveRecord::Halted { reason, .. } => match reason {
                Halt::LossCap { day: halted_day } if *halted_day == self.day => {
                    self.loss_capped = true;
                }
                Halt::LossCap { .. } => {}
                Halt::Tripwire { .. } => {
                    self.tripwire = Some(reason.clone());
                }
            },
            LiveRecord::Reset { .. } => {
                // LossCap deliberately survives Reset until UTC day rollover.
                self.tripwire = None;
            }
        }
        self.settle();
    }

    /// Move to `now_ms`'s UTC day. A new day starts with no net and no loss
    /// cap; pending nonces and a tripwire carry over.
    pub fn roll(&mut self, now_ms: u64) {
        let day = utc_day(now_ms);
        if day != self.day {
            self.day = day;
            self.day_net = 0;
            self.loss_capped = false;
            self.settle();
        }
    }

    fn settle(&mut self) {
        self.reserved_wei = self
            .pending
            .values()
            .map(|pending| {
                pending
                    .iter()
                    .filter(|p| p.accepted != Some(false))
                    .map(|p| p.worst_case_wei)
                    .max()
                    .unwrap_or(0)
            })
            .fold(0u128, u128::saturating_add);

        // A same-day loss cap has priority in the report; a tripwire remains
        // sticky across day rollover until Reset.
        self.halted = if self.loss_capped {
            Some(Halt::LossCap { day: self.day.clone() })
        } else {
            self.tripwire.clone()
        };
    }

    /// Whether another worst-case exposure can be reserved without exceeding
    /// the daily budget. Unrealized pending exposure and realized net loss are
    /// counted together; positive P&L cannot create more room than the cap.
    pub fn admits(&self, worst_case_wei: u128, budget_wei: u128) -> bool {
        let realized_loss = if self.day_net < 0 { self.day_net.unsigned_abs() } else { 0 };
        realized_loss
            .saturating_add(self.reserved_wei)
            .saturating_add(worst_case_wei)
            <= budget_wei
    }
}

/// The append-only JSONL file. Critical state transitions are synced before
/// the caller proceeds with a network side effect.
pub struct LiveLedger {
    file: Mutex<File>,
}

impl LiveLedger {
    /// Open the ledger and reconstruct all complete records.
    ///
    /// If the process died mid-append, discard the last unterminated fragment
    /// *and truncate it on disk*. Truncating is important: otherwise a later
    /// append would be concatenated to the fragment, corrupting the next line.
    /// Corruption in any newline-terminated record is not silently ignored.
    pub fn open(path: &Path) -> io::Result<(Self, Vec<LiveRecord>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let complete_len = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
        if complete_len != bytes.len() {
            file.set_len(complete_len as u64)?;
            file.sync_data()?;
        }
        let complete = &bytes[..complete_len];
        let text = std::str::from_utf8(complete).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidData, format!("armed live ledger is not UTF-8: {error}"))
        })?;
        let mut records: Vec<LiveRecord> = Vec::new();
        for (index, line) in text.split_terminator('\n').enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record = serde_json::from_str(line).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("armed live ledger line {}: {error}", index + 1))
            })?;
            records.push(record);
        }
        Ok((Self { file: Mutex::new(file) }, records))
    }

    /// Append a record. Returns only after critical records have been synced.
    /// On a write/sync error, best-effort rollback prevents a partial record
    /// from poisoning the next append; the caller must not send on an error.
    pub fn append(&self, record: &LiveRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        let critical = matches!(record, LiveRecord::Submitting { .. } | LiveRecord::Landed { .. } | LiveRecord::Expired { .. } | LiveRecord::Halted { .. });
        let mut file = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = file.metadata()?.len();
        let result = file.write_all(&line).and_then(|()| if critical { file.sync_data() } else { Ok(()) });
        if let Err(error) = result {
            // If Submitting cannot be durably recorded, the transaction must
            // not be sent. Remove its partial/uncertain row before returning.
            if file.set_len(start).is_ok() {
                let _ = file.sync_data();
            }
            return Err(error);
        }
        Ok(())
    }
}

/// JSON numbers cannot exactly represent all u128/i128 wei values. Store them
/// as base-10 strings in the append-only ledger.
mod dec {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<T: Display, S: Serializer>(value: &T, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?.parse().map_err(D::Error::custom)
    }
}

fn utc_day(at_ms: u64) -> String {
    // Howard Hinnant's civil_from_days algorithm, with Unix epoch as day zero.
    // All arithmetic is integer-only, avoiding locale and timezone dependencies.
    let days = (at_ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::utc_day;

    #[test]
    fn utc_day_uses_utc_midnight_and_rolls_over_exactly() {
        assert_eq!(utc_day(1_792_800_000_000), "2026-10-24");
        assert_eq!(utc_day(1_792_886_399_999), "2026-10-24");
        assert_eq!(utc_day(1_792_886_400_000), "2026-10-25");
        assert_eq!(utc_day(0), "1970-01-01");
    }
}
