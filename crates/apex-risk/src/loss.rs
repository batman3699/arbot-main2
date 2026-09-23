//! Loss-event containment (§25.3, §28.2). **INV-43.**
//!
//! > Every loss is classified. A loss class exceeding its expected frequency
//! > automatically tightens its gate or disables the affected module.
//!
//! `LossClass` is exhaustive and has no `Other` variant, which is the point:
//! an automatic gate cannot tighten around a bucket that absorbs everything
//! nobody wanted to classify. [`LossLedger::record`] therefore takes a class
//! rather than inferring one, and the type system makes the caller decide.

use alloy_primitives::U256;
use apex_types::risk::{LossClass, RiskPosture};
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;

/// What a class is expected to do, and what happens when it does more.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClassBudget {
    /// Events of this class allowed within `window` before the gate tightens.
    pub expected_in_window: u32,
    pub window: DurationNanos,
    /// Where the posture floor goes when the budget is exceeded.
    pub on_breach: RiskPosture,
}

/// What the ledger says should happen now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Containment {
    /// Within budget.
    None,
    /// §28.2's "tightens its gate or disables the affected module".
    Tighten { class: LossClass, observed: u32, budget: u32, posture: RiskPosture },
}

impl Containment {
    pub const fn posture_floor(&self) -> Option<RiskPosture> {
        match self {
            Self::None => None,
            Self::Tighten { posture, .. } => Some(*posture),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LossLedger {
    budgets: BTreeMap<LossClass, ClassBudget>,
    events: BTreeMap<LossClass, Vec<(UnixNanos, U256)>>,
}

impl LossLedger {
    /// An hour, which is the shortest window over which a rate is a rate rather
    /// than a coincidence at these event counts.
    pub const HOUR: DurationNanos = DurationNanos(3_600_000_000_000);

    /// Defaults per class.
    ///
    /// **Judgement, stated here rather than spread through the code.** The
    /// shape of the reasoning is: a class is budgeted by how much it should
    /// *ever* happen if the system is correct, not by how much it costs.
    ///
    /// - `Inclusion` and `Venue` losses are the cost of competing. A backrun
    ///   that loses the race reverts, and §28's own calibration note puts the
    ///   normal revert band at 55-65%. They get real budgets.
    /// - `Pricing`, `State`, `Simulation` and `FeeModel` losses mean a model
    ///   disagreed with the chain. A handful an hour is tolerable noise; more
    ///   than that is a model to fix, so they disable the strategy.
    /// - `Contract` and `OperatorConfig` losses should be **zero**. One is a
    ///   deployment that does not do what we think; the other is us. Both halt.
    /// - `ExternalProtocol` sits between: a protocol behaving unexpectedly is
    ///   not our bug, but it is not something to keep trading into either.
    pub fn with_default_budgets() -> Self {
        let b = |expected_in_window, on_breach| ClassBudget {
            expected_in_window,
            window: Self::HOUR,
            on_breach,
        };
        Self {
            budgets: BTreeMap::from([
                (LossClass::Inclusion, b(120, RiskPosture::ReducedSize)),
                (LossClass::Venue, b(30, RiskPosture::ReducedSize)),
                (LossClass::Pricing, b(5, RiskPosture::StrategyDisabled)),
                (LossClass::State, b(5, RiskPosture::StrategyDisabled)),
                (LossClass::Simulation, b(5, RiskPosture::StrategyDisabled)),
                (LossClass::FeeModel, b(5, RiskPosture::StrategyDisabled)),
                (LossClass::ExternalProtocol, b(3, RiskPosture::ChainDisabled)),
                (LossClass::Contract, b(0, RiskPosture::GlobalHalt)),
                (LossClass::OperatorConfig, b(0, RiskPosture::GlobalHalt)),
            ]),
            events: BTreeMap::new(),
        }
    }

    /// Every class has a budget. Asserted by `every_loss_class_has_a_budget`,
    /// because a class without one is a class whose gate never tightens -- the
    /// silent failure INV-43 exists to prevent.
    pub fn budget(&self, class: LossClass) -> Option<ClassBudget> {
        self.budgets.get(&class).copied()
    }

    pub fn set_budget(&mut self, class: LossClass, budget: ClassBudget) {
        self.budgets.insert(class, budget);
    }

    /// Classify a loss and say what containment it calls for.
    pub fn record(&mut self, class: LossClass, amount: U256, at: UnixNanos) -> Containment {
        self.events.entry(class).or_default().push((at, amount));
        self.evaluate(class, at)
    }

    pub fn count_in_window(&self, class: LossClass, now: UnixNanos) -> u32 {
        let Some(budget) = self.budgets.get(&class) else { return 0 };
        self.events.get(&class).map_or(0, |v| {
            v.iter().filter(|(at, _)| now.0.saturating_sub(at.0) <= budget.window.0).count() as u32
        })
    }

    pub fn total_in_window(&self, class: LossClass, now: UnixNanos) -> U256 {
        let Some(budget) = self.budgets.get(&class) else { return U256::ZERO };
        self.events.get(&class).map_or(U256::ZERO, |v| {
            v.iter()
                .filter(|(at, _)| now.0.saturating_sub(at.0) <= budget.window.0)
                .fold(U256::ZERO, |acc, (_, amt)| acc.saturating_add(*amt))
        })
    }

    fn evaluate(&self, class: LossClass, now: UnixNanos) -> Containment {
        let Some(budget) = self.budgets.get(&class) else { return Containment::None };
        let observed = self.count_in_window(class, now);
        if observed > budget.expected_in_window {
            Containment::Tighten {
                class,
                observed,
                budget: budget.expected_in_window,
                posture: budget.on_breach,
            }
        } else {
            Containment::None
        }
    }

    /// The worst containment any class currently calls for. What the posture
    /// gauge reads.
    pub fn worst(&self, now: UnixNanos) -> Containment {
        LossClass::ALL
            .iter()
            .map(|c| self.evaluate(*c, now))
            .max_by_key(|c| c.posture_floor())
            .unwrap_or(Containment::None)
    }
}

impl Default for LossLedger {
    fn default() -> Self {
        Self::with_default_budgets()
    }
}
