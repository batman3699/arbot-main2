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
