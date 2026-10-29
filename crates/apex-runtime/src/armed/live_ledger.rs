//! Armed lane v2's write-ahead live ledger and replayable risk state (Task 8.5 R36).
//!
//! Every `Submitting` record is synced before the transaction is sent. A restart
//! can therefore recover uncertain sends, their nonce slots, and their reserved
//! worst-case exposure. The last unterminated line is treated as a torn append
//! and truncated before the file is reopened for appending.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
        /// R37: the arm's start token when not WETH. Absent: WETH.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Address>,
        /// Decision 4: the arm's rate (`Arm::start_per_eth`), which values a
        /// USDC landing in wei.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_per_eth: Option<[String; 2]>,
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
        /// R37: a USDC trade's start token, and what it paid in it.
        /// `gross_wei` and `paid_wei` are then 0, and `net_wei` values the USDC
        /// paid at its arm's rate (decision 4); with no rate, at nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Address>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paid_start: Option<String>,
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
    /// The chain passed this nonce with no receipt among ours, or with one
    /// that could not be read: what came of our transactions there is
    /// unknown, so the largest worst case pending at it is counted as spent.
    Abandoned {
        sender: Address,
        nonce: u64,
        #[serde(serialize_with = "dec::serialize", deserialize_with = "dec::deserialize")]
        worst_case_wei: u128,
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
    /// R37: the start token when not WETH.
    pub start: Option<Address>,
    /// Decision 4: its arm's rate, for a USDC landing's value in wei.
    pub start_per_eth: Option<[String; 2]>,
}

/// The risk state reconstructed from the append-only live ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveState {
    pub pending: BTreeMap<(Address, u64), Vec<PendingTx>>,
    pub day: String,
    pub day_net: i128,
    /// Decision 4 (2026-10-29): the day's losses, which wins do not offset:
    /// each losing landing's loss and each abandoned worst case.
    pub day_loss: u128,
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
            day_loss: 0,
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
                start,
                start_per_eth,
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
                        start: *start,
                        start_per_eth: start_per_eth.clone(),
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
                    if *net_wei < 0 {
                        self.day_loss = self.day_loss.saturating_add(net_wei.unsigned_abs());
                    }
                    // Infer a missing Halted append after a crash between a
                    // synced Landed record and the explicit Halted record.
                    if self.loss_reached(budget_wei) {
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
            LiveRecord::Abandoned { sender, nonce, worst_case_wei, at_ms } => {
                self.pending.remove(&(*sender, *nonce));
                if utc_day(*at_ms) == self.day {
                    self.day_net = self.day_net.saturating_sub(i128::try_from(*worst_case_wei).unwrap_or(i128::MAX));
                    self.day_loss = self.day_loss.saturating_add(*worst_case_wei);
                    if self.loss_reached(budget_wei) {
                        self.loss_capped = true;
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

    /// Move on to `now_ms`'s UTC day. A new day starts with no net and no loss
    /// cap; pending nonces and a tripwire carry over. Never back: a time from
    /// before the last roll leaves the day as it is.
    pub fn roll(&mut self, now_ms: u64) {
        let day = utc_day(now_ms);
        if day > self.day {
            self.day = day;
            self.day_net = 0;
            self.day_loss = 0;
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
    /// the daily budget. Unrealized pending exposure and the day's realized
    /// losses are counted together; a win gives none of it back (decision 4,
    /// 2026-10-29: losses are counted as losses).
    pub fn admits(&self, worst_case_wei: u128, budget_wei: u128) -> bool {
        self.day_loss.saturating_add(self.reserved_wei).saturating_add(worst_case_wei) <= budget_wei
    }

    /// Whether the day's losses have spent its budget.
    pub fn loss_reached(&self, budget_wei: u128) -> bool {
        self.day_loss >= budget_wei
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
        let critical = matches!(
            record,
            LiveRecord::Submitting { .. } | LiveRecord::Landed { .. } | LiveRecord::Expired { .. } | LiveRecord::Abandoned { .. } | LiveRecord::Halted { .. }
        );
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

impl LiveLedger {
    /// Compact the ledger at `path` when it holds more than `above` records:
    /// the original is kept beside it, byte for byte, as
    /// `<stem>.<now_ms>.<ext>`, and its path returned. A hard link keeps the
    /// original first; the rename that puts the compacted file in its place is
    /// atomic, so a crash leaves one or the other at `path`, never neither.
    pub fn compact_file(path: &Path, now_ms: u64, budget_wei: u128, above: usize) -> io::Result<Option<std::path::PathBuf>> {
        let (ledger, records) = Self::open(path)?;
        drop(ledger);
        if records.len() <= above {
            return Ok(None);
        }
        let Some(kept) = compact(&records, now_ms, budget_wei) else {
            return Err(io::Error::other("the compacted ledger would not replay the same; left as it is"));
        };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("armed-live");
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
        let archive = path.with_file_name(format!("{stem}.{now_ms}.{ext}"));
        let staged = path.with_file_name(format!("{stem}.compacting"));
        let mut text = Vec::new();
        for r in &kept {
            serde_json::to_writer(&mut text, r).map_err(io::Error::other)?;
            text.push(b'\n');
        }
        let mut f = File::create(&staged)?;
        f.write_all(&text)?;
        f.sync_all()?;
        drop(f);
        std::fs::hard_link(path, &archive)?;
        std::fs::rename(&staged, path)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            File::open(dir)?.sync_all()?;
        }
        Ok(Some(archive))
    }
}

/// Each sender's next nonce as the ledger left it: a submission's own, one
/// past a landing's, an expiry's own. An abandoned nonce leaves its sender
/// none, and a reset leaves none at all: the chain's nonce is taken then.
pub fn nonce_views(records: &[LiveRecord]) -> BTreeMap<Address, u64> {
    let mut views = BTreeMap::new();
    for record in records {
        match record {
            LiveRecord::Submitting { sender, nonce, .. } | LiveRecord::Expired { sender, nonce, .. } => {
                views.insert(*sender, *nonce);
            }
            LiveRecord::Landed { sender, nonce, .. } => {
                views.insert(*sender, nonce.saturating_add(1));
            }
            LiveRecord::Abandoned { sender, .. } => {
                views.remove(sender);
            }
            LiveRecord::Reset { .. } => views.clear(),
            _ => {}
        }
    }
    views
}

/// The records a replay needs to rebuild both `LiveState::from_records` (as
/// of `now_ms`) and `nonce_views` exactly. Kept:
/// - every landing, abandonment, halt and reset, which are few, and which
///   the census reads too;
/// - every record of a hash still pending;
/// - for each sender, the record that sets its nonce view, with the
///   submission and answer of its hash.
///
/// Skips and settled renewals go. `None` when the result would not replay
/// the same, which is checked, not assumed.
pub fn compact(records: &[LiveRecord], now_ms: u64, budget_wei: u128) -> Option<Vec<LiveRecord>> {
    let state = LiveState::from_records(records, now_ms, budget_wei);
    let mut keep_tx: BTreeSet<B256> = state.pending.values().flatten().map(|p| p.tx).collect();
    let mut view_at: BTreeMap<Address, usize> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        if let LiveRecord::Submitting { sender, .. } | LiveRecord::Expired { sender, .. } | LiveRecord::Landed { sender, .. } = r {
            view_at.insert(*sender, i);
        }
    }
    let pins: BTreeSet<usize> = view_at.values().copied().collect();
    for &i in &pins {
        if let LiveRecord::Submitting { tx, .. } | LiveRecord::Expired { tx: Some(tx), .. } = &records[i] {
            keep_tx.insert(*tx);
        }
    }
    let kept: Vec<LiveRecord> = records
        .iter()
        .enumerate()
        .filter(|(i, r)| match r {
            LiveRecord::Landed { .. } | LiveRecord::Abandoned { .. } | LiveRecord::Halted { .. } | LiveRecord::Reset { .. } => true,
            LiveRecord::Submitting { tx, .. } | LiveRecord::Answered { tx, .. } => keep_tx.contains(tx),
            LiveRecord::Expired { .. } => pins.contains(i),
            LiveRecord::Skipped { .. } => false,
        })
        .map(|(_, r)| r.clone())
        .collect();
    let same = LiveState::from_records(&kept, now_ms, budget_wei) == state && nonce_views(&kept) == nonce_views(records);
    same.then_some(kept)
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
