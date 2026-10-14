//! The canary's limits and halts (spec §8). Rebuilt from the ledger at boot,
//! so a halt survives a restart; cleared only by an operator's reset record.

use super::ledger::{Halt, Ledger, Outcome, Record, Sent, Settled};
use alloy_primitives::B256;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub loss_cap_wei: u128,
    pub max_consecutive_reverts: u32,
    pub max_consecutive_refusals: u32,
    pub max_landed: u32,
}

/// Since the last reset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub sent: u64,
    pub landed: u32,
    pub reverted_early: u32,
    pub reverted_late: u32,
    pub refused: u32,
    pub silent: u32,
    pub unresolved: u32,
    /// Losses on losing trades only: a revert's whole cost, a landing's net
    /// when below zero. Wins never offset it.
    pub lost_wei: u128,
    pub gross_wei: i128,
    pub cost_wei: u128,
    pub net_wei: i128,
    pub reverts_in_a_row: u32,
    pub refusals_in_a_row: u32,
}

#[derive(Debug, Default)]
struct State {
    totals: Totals,
    halted: Option<Halt>,
    open: BTreeMap<B256, Sent>,
}

impl State {
    /// Apply one record; the halt it trips, if it trips one now.
    fn apply(&mut self, r: &Record, limits: &Limits) -> Option<Halt> {
        let t = &mut self.totals;
        match r {
            Record::Sent(s) => {
                t.sent += 1;
                self.open.insert(s.tx, s.clone());
            }
            Record::Outcome(Outcome { tx, settled, .. }) => match settled {
                Settled::Landed { gross_wei, cost_wei, net_wei, .. } => {
                    self.open.remove(tx);
                    t.landed += 1;
                    t.gross_wei = t.gross_wei.saturating_add(*gross_wei);
                    t.cost_wei = t.cost_wei.saturating_add(*cost_wei);
                    t.net_wei = t.net_wei.saturating_add(*net_wei);
                    if *net_wei < 0 {
                        t.lost_wei = t.lost_wei.saturating_add(net_wei.unsigned_abs());
                    }
                    t.reverts_in_a_row = 0;
                    t.refusals_in_a_row = 0;
                }
                Settled::Reverted { cost_wei, early, .. } => {
                    self.open.remove(tx);
                    if *early { t.reverted_early += 1 } else { t.reverted_late += 1 }
                    t.cost_wei = t.cost_wei.saturating_add(*cost_wei);
                    t.net_wei = t.net_wei.saturating_sub(i128::try_from(*cost_wei).unwrap_or(i128::MAX));
                    t.lost_wei = t.lost_wei.saturating_add(*cost_wei);
                    t.reverts_in_a_row += 1;
                    t.refusals_in_a_row = 0;
                }
                Settled::Refused { .. } => {
                    self.open.remove(tx);
                    t.refused += 1;
                    t.refusals_in_a_row += 1;
                }
                // Both may have reached the chain: they stay open for recovery.
                Settled::Silent { .. } => {
                    t.silent += 1;
                    return self.trip(Halt::Unresolved { tx: *tx });
                }
                Settled::Unresolved { .. } => {
                    t.unresolved += 1;
                    return self.trip(Halt::Unresolved { tx: *tx });
                }
                Settled::Unexplained { .. } => {
                    self.open.remove(tx);
                    return self.trip(Halt::Unexplained { tx: *tx });
                }
            },
            Record::Halted { halt, .. } => {
                self.halted = Some(halt.clone());
                return None;
            }
            Record::Reset { .. } => {
                self.totals = Totals::default();
                self.halted = None;
                return None;
            }
        }
        let t = &self.totals;
        let tripped = if t.lost_wei >= limits.loss_cap_wei {
            Some(Halt::LossCap { lost_wei: t.lost_wei })
        } else if t.reverts_in_a_row >= limits.max_consecutive_reverts {
            Some(Halt::Reverts { in_a_row: t.reverts_in_a_row })
        } else if t.refusals_in_a_row >= limits.max_consecutive_refusals {
            Some(Halt::Refusals { in_a_row: t.refusals_in_a_row })
        } else if t.landed >= limits.max_landed {
            Some(Halt::Done { landed: t.landed })
        } else {
            None
        };
        tripped.and_then(|h| self.trip(h))
    }

    fn trip(&mut self, h: Halt) -> Option<Halt> {
        if self.halted.is_some() {
            return None;
        }
        self.halted = Some(h.clone());
        Some(h)
    }
}

/// What the five-minute report carries (spec §11).
#[derive(Clone, Debug, Serialize)]
pub struct CanaryReport {
    pub totals: Totals,
    pub halted: Option<String>,
    pub stop_file: bool,
    pub open: usize,
}

pub struct Canary {
    ledger: Ledger,
    limits: Limits,
    stop_file: PathBuf,
    state: Mutex<State>,
    sent_since_boot: AtomicU64,
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

impl Canary {
    pub fn open(ledger: &Path, stop_file: &Path, limits: Limits) -> io::Result<Self> {
        let (ledger, records) = Ledger::open(ledger)?;
        let mut state = State::default();
        for r in &records {
            // A halt tripped while replaying was written when it tripped.
            let _ = state.apply(r, &limits);
        }
        Ok(Self {
            ledger,
            limits,
            stop_file: stop_file.to_path_buf(),
            state: Mutex::new(state),
            sent_since_boot: AtomicU64::new(0),
        })
    }

    fn record(&self, r: &Record) -> io::Result<()> {
        self.ledger.append(r)?;
        let tripped = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).apply(r, &self.limits);
        if let Some(halt) = tripped {
            tracing::error!(%halt, "the canary halted");
            self.ledger.append(&Record::Halted { at_ms: now_ms(), halt })?;
        }
        Ok(())
    }

    /// Written and synced before the transaction leaves the process.
    pub fn sent(&self, s: Sent) -> io::Result<()> {
        self.record(&Record::Sent(s))?;
        self.sent_since_boot.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn settle(&self, o: Outcome) -> io::Result<()> {
        self.record(&Record::Outcome(o))
    }

    /// Halt for a reason no record computes (the tripwire).
    pub fn halt(&self, halt: Halt) -> io::Result<()> {
        let fresh = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).trip(halt.clone());
        if fresh.is_some() {
            tracing::error!(%halt, "the canary halted");
            self.ledger.append(&Record::Halted { at_ms: now_ms(), halt })?;
        }
        Ok(())
    }

    pub fn halted(&self) -> Option<Halt> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).halted.clone()
    }

    /// Sends with no outcome: what boot recovery must resolve.
    pub fn open_sends(&self) -> Vec<Sent> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).open.values().cloned().collect()
    }

    pub fn sent_since_boot(&self) -> u64 {
        self.sent_since_boot.load(Ordering::Relaxed)
    }

    pub fn report(&self) -> CanaryReport {
        let s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        CanaryReport {
            totals: s.totals.clone(),
            halted: s.halted.as_ref().map(ToString::to_string),
            stop_file: self.stop_file.exists(),
            open: s.open.len(),
        }
    }
}

impl crate::risk::Hold for Canary {
    fn reason(&self) -> Option<String> {
        if let Some(h) = self.halted() {
            return Some(format!("canary halted: {h}"));
        }
        self.stop_file
            .exists()
            .then(|| format!("canary stop file {} is present", self.stop_file.display()))
    }
}
