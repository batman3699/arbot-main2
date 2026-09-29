//! §29's nine resource classes and §46.2's concurrency (BP-132, BP-175).
//!
//! # Two separate claims
//!
//! **BP-175 — parallel, not serial.** §46.2: "on a state event, exact repricing
//! / finite-size sizing / competitor scenarios / cost refresh run concurrently
//! and join. No artificial serialization where tasks are independent."
//! [`refine_concurrently`] is that join, and it is one `try_join!` rather than
//! four `await`s because the difference between those two lines is the whole
//! latency budget.
//!
//! **BP-132 — nine classes, nine budgets.** §29 names the classes; §29.3 says
//! "no strategy may create an unbounded queue". [`Budgets`] gives each class its
//! own permits, so saturating candidate generation cannot take the permit
//! submission needed. A single shared pool would make the *most numerous* work
//! win, and candidate generation is always the most numerous.
//!
//! # What §29.2 does and does not say
//!
//! It names three classes to **protect**: state ingestion, exact simulation for
//! high-EV candidates, submission. Its shed list — "exotic searches",
//! "low-confidence routes", "expensive low-hit-rate strategies" — describes
//! properties of *work*, not the nine classes, so there is no total order over
//! the classes to be had. [`ResourceClass::is_protected`] therefore states
//! exactly the three, and nothing here invents a ranking for the other six. A
//! derived `Ord` over this enum would look like a priority and would be a
//! fabrication; it is not derived.

use crate::bus::Lane;
use crate::plane::{Decline, Economics, Refinement};
use apex_types::candidate::Candidate;
use tokio::sync::{Semaphore, SemaphorePermit};

/// §29's nine classes, verbatim.
///
/// No `Ord`: see the module header. The variants are in §29's listed order,
/// which is a reading order and not a priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceClass {
    StateIngestion,
    StatePatching,
    CandidateGeneration,
    ExactPricing,
    SizingAllocation,
    Simulation,
    Submission,
    /// §29's "liquidation/event strategies".
    EventStrategies,
    Telemetry,
}

impl ResourceClass {
    pub const ALL: [Self; 9] = [
        Self::StateIngestion,
        Self::StatePatching,
        Self::CandidateGeneration,
        Self::ExactPricing,
        Self::SizingAllocation,
        Self::Simulation,
        Self::Submission,
        Self::EventStrategies,
        Self::Telemetry,
    ];

    /// §29.2's three protected classes, and only those.
    ///
    /// Matched exhaustively rather than written as a list, so adding a tenth
    /// class is a compile error here — which is the one place that decision has
    /// to be made consciously.
    pub const fn is_protected(self) -> bool {
        match self {
            Self::StateIngestion | Self::Simulation | Self::Submission => true,
            Self::StatePatching
            | Self::CandidateGeneration
            | Self::ExactPricing
            | Self::SizingAllocation
            | Self::EventStrategies
            | Self::Telemetry => false,
        }
    }

    /// The complement, named so that shedding code reads as what it does. Stated
    /// as the negation rather than as a second list: two lists drift.
    pub const fn is_sheddable(self) -> bool {
        !self.is_protected()
    }

    /// Which of §2.6's paths this class serves. Telemetry and research sit on the
    /// slow lane because a stalled exporter must never delay a ticket.
    pub const fn lane(self) -> Lane {
        match self {
            Self::StateIngestion
            | Self::StatePatching
            | Self::CandidateGeneration
            | Self::ExactPricing
            | Self::SizingAllocation
            | Self::Simulation
            | Self::Submission => Lane::Fast,
            Self::EventStrategies | Self::Telemetry => Lane::Slow,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::StateIngestion => "state_ingestion",
            Self::StatePatching => "state_patching",
            Self::CandidateGeneration => "candidate_generation",
            Self::ExactPricing => "exact_pricing",
            Self::SizingAllocation => "sizing_allocation",
            Self::Simulation => "simulation",
            Self::Submission => "submission",
            Self::EventStrategies => "event_strategies",
            Self::Telemetry => "telemetry",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::StateIngestion => 0,
            Self::StatePatching => 1,
            Self::CandidateGeneration => 2,
            Self::ExactPricing => 3,
            Self::SizingAllocation => 4,
            Self::Simulation => 5,
            Self::Submission => 6,
            Self::EventStrategies => 7,
            Self::Telemetry => 8,
        }
    }
}

/// One bounded budget per class (§29.3). **Nine semaphores, not one.**
#[derive(Debug)]
pub struct Budgets {
    per_class: [Semaphore; 9],
}

impl Budgets {
    /// Every class gets `permits`. Real deployments will differ per class — that
    /// is configuration, and `apex-config` owns it — but the *isolation* is
    /// structural and does not depend on the numbers.
    pub fn with_capacity(permits: usize) -> Self {
        Self { per_class: std::array::from_fn(|_| Semaphore::new(permits)) }
    }

    pub fn per_class(permits: [usize; 9]) -> Self {
        Self { per_class: std::array::from_fn(|i| Semaphore::new(permits[i])) }
    }

    /// Non-blocking. `None` means no permit, which is a fact to report rather
    /// than a queue to join — §29.3's "no unbounded queue" is only true if the
    /// refusal is visible.
    ///
    /// The two reasons for `None` are *at budget* (pressure) and *shed*
    /// (a decision, §29.2), and they call for different responses. They are not
    /// folded into this return value because a caller on the capture path wants
    /// one branch; [`Self::is_shed`] answers the second question for the operator
    /// who has to act on it.
    pub fn reserve(&self, class: ResourceClass) -> Option<SemaphorePermit<'_>> {
        self.per_class[class.index()].try_acquire().ok()
    }

    pub fn available(&self, class: ResourceClass) -> usize {
        self.per_class[class.index()].available_permits()
    }

    /// Shedding, §29.2. Closes the sheddable classes' budgets so in-flight work
    /// finishes and no new work starts; the three protected classes are left
    /// alone, whatever the pressure.
    ///
    /// Returns the classes it actually shed, so an operator sees the action
    /// rather than inferring it. The same discipline as `Scheduler::shed`: the
    /// request is a target it will miss rather than a licence.
    pub fn shed(&self) -> Vec<ResourceClass> {
        ResourceClass::ALL
            .into_iter()
            .filter(|c| c.is_sheddable())
            .inspect(|c| self.per_class[c.index()].close())
            .collect()
    }

    pub fn is_shed(&self, class: ResourceClass) -> bool {
        self.per_class[class.index()].is_closed()
    }
}

/// **§46.2's join.** The four independent answers a state event needs, in flight
/// at once.
///
/// `try_join!` and not four `await`s. The four stages read the same candidate and
/// write nothing, so serialising them buys nothing and costs their sum — and
/// §29.5's latency budget is the reason the plan says "no artificial
/// serialization where tasks are independent" rather than leaving it to taste.
///
/// Short-circuits on the first decline, which is correct rather than merely
/// convenient: a candidate with no profitable size does not need its cost model
/// refreshed, and the §29 compute it would spend is the scarce thing.
pub async fn refine_concurrently(
    econ: &dyn Economics,
    candidate: &Candidate,
) -> Result<Refinement, Decline> {
    let (expected_output, input_amount, robustness_margin, costs) = tokio::try_join!(
        econ.reprice(candidate),
        econ.size(candidate),
        econ.scenarios(candidate),
        econ.refresh_costs(candidate),
    )?;

    Ok(Refinement { expected_output, input_amount, robustness_margin, costs })
}
