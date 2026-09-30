//! The Flashblock scheduler (§21.2, Blueprint §22.1, §22.2). **INV-37, INV-38.**
//!
//! ```text
//! k_eligible = min{ k : G_t <= Q(k) }
//! ```
//!
//! > `Q(k)` is the **MEASURED** allocation policy, never a hard-coded
//! > one-tenth rule. The blueprint is explicit that a fixed fractional rule
//! > must not be hard-coded because chain parameters may change.
//!
//! So [`MeasuredCapacityModel`] is built from observations and **cannot be
//! built any other way** — there is no `Default`, no `from_fraction`, and no
//! constructor taking a block gas limit. A model with no observations for an
//! index answers `Unknown` for that index rather than interpolating, because
//! §5.6 forbids converting a gap into "probably the usual".
//!
//! # What `Q(k)` measures
//!
//! Cumulative gas available **through** flashblock `k`, not the increment at
//! `k`. A transaction is eligible for the first window by which the block has
//! accumulated room for it; asking whether it fits one increment would reject
//! every transaction larger than a single flashblock's share, which is not how
//! the chain behaves.
//!
//! # The conservative quantile is the point
//!
//! `Q(k)` reports the **p10** of observed capacity, not the median. A median
//! would be right half the time, and the half it is wrong about is a
//! transaction signed for a window it does not fit — which costs a nonce, a
//! lane, and the opportunity. Being early is free; being optimistic is not.
//!
//! # Where the observations come from
//!
//! The provider's `newFlashblocks`, sampled by [`sample`] into a
//! [`FlashblockRecorder`], which derives each flashblock's index, refuses any
//! block it did not see whole, and files cumulative gas **used** as the budget
//! — a lower bound, so the model errs the permitted way.

use serde::{Deserialize, Serialize};

/// One observed flashblock. The cumulative figure is what §22.2's `Q` is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashblockObservation {
    pub block: u64,
    pub index: u32,
    /// Gas cumulatively available through this flashblock, inclusive.
    pub cumulative_gas_budget: u64,
}

/// What the model knows about one index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capacity {
    /// §5.6: a gap is reported, never filled in.
    Unknown { samples: usize, required: usize },
    Measured {
        samples: usize,
        /// The capacity eligibility is decided on.
        p10: u64,
        median: u64,
        p90: u64,
    },
}

impl Capacity {
    /// The figure `earliest_eligible` compares against. `None` when unknown --
    /// and an unknown index is not "probably like its neighbour".
    pub const fn reliable(&self) -> Option<u64> {
        match self {
            Self::Unknown { .. } => None,
            Self::Measured { p10, .. } => Some(*p10),
        }
    }

    /// §22.1 stores the estimate with a confidence interval. This is it.
    pub const fn interval(&self) -> Option<(u64, u64)> {
        match self {
            Self::Unknown { .. } => None,
            Self::Measured { p10, p90, .. } => Some((*p10, *p90)),
        }
    }
}

/// Why a model could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// Not one observation. A model built from nothing would be a hard-coded
    /// rule wearing a measurement's name.
    NoObservations,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoObservations => f.write_str("a capacity model needs observations"),
        }
    }
}

impl std::error::Error for ModelError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasuredCapacityModel {
    /// Indexed by flashblock index.
    per_index: Vec<Capacity>,
    observations: usize,
    min_samples: usize,
}

impl MeasuredCapacityModel {
    /// How many observations an index needs before it is trusted. Below this
    /// the estimate is one or two blocks' idiosyncrasy, and eligibility built
    /// on it would swing with the traffic that happened to be sampled.
    pub const DEFAULT_MIN_SAMPLES: usize = 8;

    /// The only constructor. Note what it does **not** take: a block gas limit,
    /// a flashblock count, or a fraction.
    pub fn from_observations(obs: &[FlashblockObservation]) -> Result<Self, ModelError> {
        Self::from_observations_with(obs, Self::DEFAULT_MIN_SAMPLES)
    }

    pub fn from_observations_with(
        obs: &[FlashblockObservation],
        min_samples: usize,
    ) -> Result<Self, ModelError> {
        if obs.is_empty() {
            return Err(ModelError::NoObservations);
        }
        let highest = obs.iter().map(|o| o.index).max().unwrap_or(0);
        let mut buckets: Vec<Vec<u64>> = vec![Vec::new(); highest as usize + 1];
        for o in obs {
            buckets[o.index as usize].push(o.cumulative_gas_budget);
        }
        let per_index = buckets
            .into_iter()
            .map(|mut v| {
                if v.len() < min_samples.max(1) {
                    return Capacity::Unknown { samples: v.len(), required: min_samples.max(1) };
                }
                v.sort_unstable();
                Capacity::Measured {
                    samples: v.len(),
                    p10: quantile(&v, 10),
                    median: quantile(&v, 50),
                    p90: quantile(&v, 90),
                }
            })
            .collect();
        Ok(Self { per_index, observations: obs.len(), min_samples })
    }

    pub const fn observations(&self) -> usize {
        self.observations
    }

    pub const fn min_samples(&self) -> usize {
        self.min_samples
    }

    pub fn windows(&self) -> usize {
        self.per_index.len()
    }

    pub fn capacity_at(&self, index: u32) -> Option<Capacity> {
        self.per_index.get(index as usize).copied()
    }

    /// `Q(k)`. `None` for an index the model has not measured.
    pub fn q(&self, k: u32) -> Option<u64> {
        self.per_index.get(k as usize).and_then(Capacity::reliable)
    }

    /// The largest capacity any measured window offers. What a rejection quotes
    /// so an operator can see how far off the transaction was.
    pub fn largest_measured_window(&self) -> u64 {
        self.per_index.iter().filter_map(Capacity::reliable).max().unwrap_or(0)
    }
}

/// Order-preserving nearest-rank quantile. Not interpolated: an interpolated
/// p10 is a capacity nobody observed, and the whole point of this model is that
/// every number in it was seen.
fn quantile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (pct * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// §22.2: `k_eligible = min{ k : G_t <= Q(k) }`.
///
/// `None` means no measured window fits — which §21.3 turns into a rejection
/// **before signing**, not a smaller gas limit chosen on the fly.
pub fn earliest_eligible(g_t: u64, q: &MeasuredCapacityModel) -> Option<u32> {
    (0..q.windows() as u32).find(|k| q.q(*k).is_some_and(|cap| g_t <= cap))
}

/// **INV-38, the ordering lock (§22.3).**
///
/// > Once a Flashblock is built its ordering is fixed. The scheduler only ever
/// > computes a *future* eligible index; there is no "pay more later and still
/// > land earlier" path.
///
/// So this never returns an index below `current`, and it does not do it by
/// clamping a smaller answer upward either — a clamp would silently claim a
/// window whose capacity was never checked. It searches from `current`.
pub fn earliest_eligible_from(
    current: u32,
    g_t: u64,
    q: &MeasuredCapacityModel,
) -> Option<u32> {
    (current..q.windows() as u32).find(|k| q.q(*k).is_some_and(|cap| g_t <= cap))
}

/// Turns a provider's `newFlashblocks` notifications into
/// [`FlashblockObservation`]s (Task 8.5 R5).
///
/// # The index is a position, so only whole blocks count
///
/// A notification is the pending block as built through one flashblock —
/// cumulative gas used, cumulative transactions — and it does **not** carry
/// Base's flashblock index. Measured against BlockPI 2026-10-01: eleven per
/// block, the first holding only the L1-info deposit, so the index is the
/// notification's position in its block. A dropped notification would shift
/// every later one down an index and record its larger cumulative gas as an
/// earlier window's — overstating capacity, the one direction this model must
/// never err in. So a block counts only if it was seen **whole**:
///
/// - **anchored** — its first notification holds nothing but deposits, which
///   only index 0 does (a block joined mid-way fails this);
/// - **in order** — gas and transactions never shrink within it;
/// - **complete** — it had as many flashblocks as most blocks in the window.
///   The mode, not the maximum: one duplicated notification would lift the
///   maximum and refuse every normal block for as long as it stayed in the
///   window. The count is measured, never assumed.
///
/// A block that fails is refused and counted, never repaired.
///
/// # Used gas is a lower bound on the budget
///
/// What a block *used* through flashblock `k` never exceeds what it was
/// *allowed* through `k`, so a model built from use reports less capacity than
/// exists — the permitted direction. The recorder files use as
/// [`FlashblockObservation::cumulative_gas_budget`] and knows no fraction of
/// the gas limit (§22.2 forbids the fixed one-tenth rule).
#[derive(Clone, Debug)]
pub struct FlashblockRecorder {
    window: usize,
    building: Option<Building>,
    /// Anchored, in-order blocks, oldest first: cumulative gas used at each
    /// index.
    blocks: std::collections::VecDeque<(u64, Vec<u64>)>,
    refused: u64,
    last_refused: Option<u64>,
}

#[derive(Clone, Debug)]
struct Building {
    block: u64,
    gas: Vec<u64>,
    transactions: usize,
    whole: bool,
}

impl FlashblockRecorder {
    /// Blocks kept. Sampled a few at a time, 200 blocks is hours of the
    /// chain's traffic — and blocks older than that describe load it is no
    /// longer carrying.
    pub const DEFAULT_WINDOW: usize = 200;

    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            building: None,
            blocks: Default::default(),
            refused: 0,
            last_refused: None,
        }
    }

    pub fn observe(&mut self, f: &crate::rpc::ws::Flashblock) {
        if let Some(b) = self.building.as_mut() {
            if f.number == b.block {
                if f.gas_used < b.gas.last().copied().unwrap_or(0) || f.transactions < b.transactions {
                    b.whole = false;
                }
                b.gas.push(f.gas_used);
                b.transactions = f.transactions;
                return;
            }
            // An older block's flashblock after a newer one's: nothing to place
            // it against.
            if f.number < b.block {
                return;
            }
        }
        // A newer block: the one being built is over.
        self.close();
        self.building = Some(Building {
            block: f.number,
            gas: vec![f.gas_used],
            transactions: f.transactions,
            whole: f.transactions > 0 && f.transactions == f.deposits,
        });
    }

    /// Continuity was lost — a reconnect, a dropped notification, the end of a
    /// sample. The block being built may be missing flashblocks, so it is
    /// refused rather than closed.
    pub fn reset(&mut self) {
        if let Some(b) = self.building.take() {
            self.refuse(b.block);
        }
    }

    /// Once per block: a block cut by a reset is refused for its head, and its
    /// tail — which cannot anchor — is the same block.
    fn refuse(&mut self, block: u64) {
        if self.last_refused != Some(block) {
            self.refused += 1;
            self.last_refused = Some(block);
        }
    }

    fn close(&mut self) {
        let Some(b) = self.building.take() else { return };
        if !b.whole {
            self.refuse(b.block);
            return;
        }
        self.blocks.push_back((b.block, b.gas));
        while self.blocks.len() > self.window {
            self.blocks.pop_front();
        }
    }

    /// Anchored, in-order blocks held.
    pub fn blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Blocks refused — joined mid-way, out of order, or cut off — each once.
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// How many flashblocks most blocks in the window had, the larger on a tie.
    fn usual_count(&self) -> Option<usize> {
        let mut counts: std::collections::BTreeMap<usize, usize> = Default::default();
        for (_, g) in &self.blocks {
            *counts.entry(g.len()).or_default() += 1;
        }
        counts.into_iter().max_by_key(|&(len, n)| (n, len)).map(|(len, _)| len)
    }

    /// The observations a model is built from: every index of every complete
    /// block in the window.
    pub fn observations(&self) -> Vec<FlashblockObservation> {
        let Some(usual) = self.usual_count() else { return Vec::new() };
        self.blocks
            .iter()
            .filter(|(_, g)| g.len() == usual)
            .flat_map(|(block, g)| {
                g.iter().enumerate().map(move |(i, gas)| FlashblockObservation {
                    block: *block,
                    index: i as u32,
                    cumulative_gas_budget: *gas,
                })
            })
            .collect()
    }

    /// §22.2's `Q`, from the window.
    pub fn model(&self) -> Result<MeasuredCapacityModel, ModelError> {
        MeasuredCapacityModel::from_observations(&self.observations())
    }
}

/// Watch `newFlashblocks` for `duration` on a connection of its own, recording
/// into `rec`, then close it; returns the flashblocks seen.
///
/// A sample, not a subscription held open: at ~750 KiB/s the stream would move
/// ~65 GB a day through the provider to refresh a distribution that changes
/// over hours. Blocks cut off at either end are refused by the recorder.
pub async fn sample(
    url: &str,
    settings: crate::rpc::ws::WsSettings,
    duration: std::time::Duration,
    rec: &mut FlashblockRecorder,
) -> Result<u64, crate::rpc::ws::FeedError> {
    use crate::rpc::ws::{Notification, Subscription, WsFeed};
    let feed = WsFeed::new(url, vec![Subscription::NewFlashblocks], settings)?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let (task, mut rx) = feed.spawn(stopped);
    let deadline = tokio::time::Instant::now() + duration;
    let mut seen = 0u64;
    while let Ok(Some(n)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match n {
            Notification::Flashblock(f) => {
                seen += 1;
                rec.observe(&f);
            }
            Notification::Reconnected { .. } | Notification::Gap { .. } => rec.reset(),
            Notification::Head(_) | Notification::Log(_) => {}
        }
    }
    rec.reset();
    let _ = stop.send(true);
    let _ = task.await;
    Ok(seen)
}
