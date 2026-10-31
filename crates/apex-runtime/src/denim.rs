//! Base's Denim hardfork (Task 8.5 R41): 200 ms blocks, stamped with
//! `timestampMs`, from 2026-11-05 18:00:00 UTC on mainnet.
//!
//! Three clocks, each from the chain's own stamps rather than from when
//! anything arrived:
//! - [`ReadClock`]: per-head reads of slow values at most once an interval;
//! - [`HeadWindow`]: the armed lane's Denim guard, chain cadence over the
//!   last 20 heads and the newest head's lag;
//! - [`lattice`] and [`detect`]: the fork switch's proof, ten consecutive
//!   blocks exactly 200 ms apart, every one at or after the fork.
//!
//! The fork's time is the client's: `base/base` v1.5.1,
//! `crates/common/chains/src/chain.rs`, asserts Denim inactive on mainnet at
//! 1_793_901_599 and active at 1_793_901_600.

use std::collections::VecDeque;

/// Denim on Base mainnet: 2026-11-05 18:00:00 UTC.
pub const DENIM_MAINNET_TS: u64 = 1_793_901_600;
/// Denim on Base Sepolia: 2026-10-21 18:00:00 UTC.
pub const DENIM_SEPOLIA_TS: u64 = 1_792_605_600;
/// A block's time under Denim.
pub const DENIM_BLOCK_MS: u64 = 200;
/// A block's time before it.
pub const PRE_DENIM_BLOCK_MS: u64 = 2_000;
/// Heads the Denim guard measures the chain's cadence over: 4 s of them.
pub const HEAD_WINDOW: usize = 20;
/// Blocks the switch's proof takes.
const LATTICE_BLOCKS: usize = 10;

/// A round of reads at most once every `every_ms`; every head when it is 0.
#[derive(Clone, Debug)]
pub struct ReadClock {
    every_ms: u64,
    last_ms: Option<u64>,
}

impl ReadClock {
    pub const fn new(every_ms: u64) -> Self {
        Self { every_ms, last_ms: None }
    }

    /// Whether a round starts at `now_ms`; one that does is the clock's new
    /// start. A clock that stepped back starts one at once rather than
    /// waiting for its old time to come round.
    pub fn due(&mut self, now_ms: u64) -> bool {
        let due = self.every_ms == 0
            || self.last_ms.is_none_or(|last| now_ms < last || now_ms - last >= self.every_ms);
        if due {
            self.last_ms = Some(now_ms);
        }
        due
    }
}

/// The last [`HEAD_WINDOW`] stamped heads: what the chain's own clock says
/// about the feed. A burst of arrivals cannot move it, nor can a skipped head.
#[derive(Clone, Debug, Default)]
pub struct HeadWindow {
    /// `(number, timestamp_ms)`, oldest first.
    samples: VecDeque<(u64, u64)>,
    last: Option<u64>,
    /// The newest head carried no `timestampMs`.
    newest_unstamped: bool,
    skips: u64,
    without_ms: u64,
}

impl HeadWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// One head. A head not newer than the last is ignored; a gap is counted.
    pub fn observe(&mut self, number: u64, timestamp_ms: Option<u64>) {
        if self.last.is_some_and(|last| number <= last) {
            return;
        }
        if let Some(last) = self.last {
            self.skips += number - last - 1;
        }
        self.last = Some(number);
        match timestamp_ms {
            Some(ms) => {
                if self.samples.len() == HEAD_WINDOW {
                    self.samples.pop_front();
                }
                self.samples.push_back((number, ms));
                self.newest_unstamped = false;
            }
            None => {
                self.without_ms += 1;
                self.newest_unstamped = true;
            }
        }
    }

    /// The chain's milliseconds a block over the window, once it is full and
    /// its newest head is stamped.
    pub fn chain_cadence_ms(&self) -> Option<u64> {
        if self.newest_unstamped || self.samples.len() < HEAD_WINDOW {
            return None;
        }
        let (&(n0, ms0), &(n1, ms1)) = (self.samples.front()?, self.samples.back()?);
        Some(ms1.saturating_sub(ms0) / (n1 - n0))
    }

    /// How old the newest head's stamp is at `now_ms`; none for an unstamped
    /// newest head. A stamp after now has no lag.
    pub fn head_lag_ms(&self, now_ms: u64) -> Option<u64> {
        if self.newest_unstamped {
            return None;
        }
        self.samples.back().map(|&(_, ms)| now_ms.saturating_sub(ms))
    }

    /// The Denim guard: the chain keeps `block_time_ms`, within half of it
    /// either way, and its newest head is at most `max_lag_ms` old.
    pub fn holds(&self, block_time_ms: u64, max_lag_ms: u64, now_ms: u64) -> bool {
        self.chain_cadence_ms().is_some_and(|c| c.abs_diff(block_time_ms).saturating_mul(2) <= block_time_ms)
            && self.head_lag_ms(now_ms).is_some_and(|lag| lag <= max_lag_ms)
    }

    /// Head numbers never seen.
    pub const fn skips(&self) -> u64 {
        self.skips
    }

    /// Heads that carried no `timestampMs`.
    pub const fn without_ms(&self) -> u64 {
        self.without_ms
    }
}

/// One block as the switch reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatticeBlock {
    pub number: u64,
    pub timestamp: u64,
    pub timestamp_ms: Option<u64>,
}

/// The switch's proof that Denim is live: ten consecutive blocks, each
/// stamped on the 200 ms lattice within its own second, each exactly 200 ms
/// after the last, the oldest at or after `fork_at`. Ten 2 s blocks pass
/// none of it. The error names the first rule broken.
pub fn lattice(blocks: &[LatticeBlock], fork_at: u64) -> Result<(), String> {
    if blocks.len() != LATTICE_BLOCKS {
        return Err(format!("{} blocks, not ten", blocks.len()));
    }
    for w in blocks.windows(2) {
        if w[1].number != w[0].number + 1 {
            return Err(format!("not consecutive at block {}", w[1].number));
        }
    }
    let mut stamps = Vec::with_capacity(LATTICE_BLOCKS);
    for b in blocks {
        let Some(ms) = b.timestamp_ms else { return Err(format!("block {} has no timestampMs", b.number)) };
        if ms % DENIM_BLOCK_MS != 0 {
            return Err(format!("block {} is off the 200 ms lattice", b.number));
        }
        if ms / 1_000 != b.timestamp {
            return Err(format!("block {}'s timestampMs is not in its second", b.number));
        }
        stamps.push((b.number, ms));
    }
    for w in stamps.windows(2) {
        let step = w[1].1.saturating_sub(w[0].1);
        if step != DENIM_BLOCK_MS {
            return Err(format!("a {step} ms step at block {}", w[1].0));
        }
    }
    match blocks.iter().find(|b| b.timestamp < fork_at) {
        Some(b) => Err(format!("block {} is before the fork", b.number)),
        None => Ok(()),
    }
}

/// What one round of the switch found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detect {
    Denim,
    NotYet(String),
    Anomaly(String),
}

/// One round: the wrong chain is an anomaly; a block before the fork is not
/// yet; then the lattice decides.
pub fn detect(chain_id: u64, want_chain: u64, blocks: &[LatticeBlock], fork_at: u64) -> Detect {
    if chain_id != want_chain {
        return Detect::Anomaly(format!("chain {chain_id}, not {want_chain}"));
    }
    if let Some(b) = blocks.iter().min_by_key(|b| b.timestamp).filter(|b| b.timestamp < fork_at) {
        return Detect::NotYet(format!("block {} at {} is before the fork", b.number, b.timestamp));
    }
    match lattice(blocks, fork_at) {
        Ok(()) => Detect::Denim,
        Err(e) => Detect::Anomaly(e),
    }
}
