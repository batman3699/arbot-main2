//! `TickLadder` + the pure multi-tick exact-input swap loop.
//!
//! Everything here is synchronous and total: given a ladder, the quote is a
//! deterministic function with no I/O. Tick acquisition lives in `cl_ticks`.

use crate::cl_math::{
    compute_swap_step, get_sqrt_ratio_at_tick, max_sqrt_ratio, min_sqrt_ratio, MAX_TICK, MIN_TICK,
};
use crate::cl_sim::ClPoolState;
use ethers::types::U256;

/// Outcome of asking a ladder for the next initialized tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderStep {
    Initialized { tick: i32, liquidity_net: i128 },
    /// The search left the range this ladder is known-complete over. The
    /// caller must NOT assume constant liquidity beyond this point.
    Exhausted,
}

/// A materialised, sorted view of a pool's initialized ticks over a bounded
/// range, plus the bounds over which that view is known complete.
///
/// The bounds are the load-bearing part. A ladder that merely held ticks would
/// let a large swap walk off the end and silently continue at the last known
/// liquidity — exactly the constant-liquidity error in
/// `quote_exact_input_single_tick`. Recording where knowledge stops turns that
/// into a reported `Exhausted` and a caller-side fallback.
#[derive(Clone, Debug)]
pub struct TickLadder {
    /// `(tick, liquidity_net)` sorted ascending by tick, deduplicated.
    ticks: Vec<(i32, i128)>,
    lower_bound: i32,
    upper_bound: i32,
}

impl TickLadder {
    pub fn new(mut ticks: Vec<(i32, i128)>, lower_bound: i32, upper_bound: i32) -> Self {
        ticks.sort_unstable_by_key(|(t, _)| *t);
        ticks.dedup_by_key(|(t, _)| *t);
        Self {
            ticks,
            lower_bound: lower_bound.max(MIN_TICK),
            upper_bound: upper_bound.min(MAX_TICK),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn lower_bound(&self) -> i32 {
        self.lower_bound
    }

    pub fn upper_bound(&self) -> i32 {
        self.upper_bound
    }

    /// Whether `tick` falls inside the range this ladder proved complete.
    pub fn covers(&self, tick: i32) -> bool {
        tick >= self.lower_bound && tick <= self.upper_bound
    }

    /// Next initialized tick in the direction of travel.
    ///
    /// Mirrors v3-core's `nextInitializedTickWithinOneWord`: `zero_for_one`
    /// (price falling) searches for the greatest initialized tick `<= from`,
    /// the other direction for the least initialized tick `> from`.
    pub fn next_initialized(&self, from_tick: i32, zero_for_one: bool) -> LadderStep {
        if !self.covers(from_tick) {
            return LadderStep::Exhausted;
        }
        let found = if zero_for_one {
            self.ticks
                .iter()
                .rev()
                .find(|(t, _)| *t <= from_tick)
                .copied()
        } else {
            self.ticks.iter().find(|(t, _)| *t > from_tick).copied()
        };
        match found {
            Some((tick, liquidity_net)) if self.covers(tick) => {
                LadderStep::Initialized { tick, liquidity_net }
            }
            _ => LadderStep::Exhausted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cl_sim::ClPoolState;
    use ethers::types::U256;

    fn ladder() -> TickLadder {
        // Symmetric band around 0, 60-spaced, with liquidity added at the
        // inner ticks and removed at the outer ones.
        TickLadder::new(
            vec![
                (-180, 1_000_000_000),
                (-120, 2_000_000_000),
                (-60, 3_000_000_000),
                (60, -3_000_000_000),
                (120, -2_000_000_000),
                (180, -1_000_000_000),
            ],
            -180,
            180,
        )
    }

    #[test]
    fn next_initialized_walks_down_for_zero_for_one() {
        let l = ladder();
        // zero_for_one searches for the greatest initialized tick <= from.
        assert_eq!(
            l.next_initialized(0, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-60, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-61, true),
            LadderStep::Initialized { tick: -120, liquidity_net: 2_000_000_000 }
        );
    }

    #[test]
    fn next_initialized_walks_up_for_one_for_zero() {
        let l = ladder();
        // one_for_zero searches for the least initialized tick strictly > from.
        assert_eq!(
            l.next_initialized(0, false),
            LadderStep::Initialized { tick: 60, liquidity_net: -3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(60, false),
            LadderStep::Initialized { tick: 120, liquidity_net: -2_000_000_000 }
        );
    }

    /// Running off the end of proven coverage must be reported, never guessed.
    /// This is the defect the whole plan exists to remove.
    #[test]
    fn next_initialized_reports_exhaustion_past_coverage() {
        let l = ladder();
        assert_eq!(l.next_initialized(-181, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(180, false), LadderStep::Exhausted);
    }

    #[test]
    fn empty_ladder_is_immediately_exhausted() {
        let l = TickLadder::new(Vec::new(), -60, 60);
        assert_eq!(l.next_initialized(0, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(0, false), LadderStep::Exhausted);
    }
}
