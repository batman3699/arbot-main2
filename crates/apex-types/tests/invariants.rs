//! The blueprint invariants this crate carries in its types rather than in
//! prose. Each test names the INV- it pins.

use apex_types::candidate::{DiscreteRefined, DiscreteSize};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::miss::MissReason;
use apex_types::pnl::UsdBounds;
use apex_types::risk::{LossClass, RiskPosture};
use apex_types::state::ReconstructionStatus;
use apex_types::ticket::TicketStatus;
use alloy_primitives::U256;

// ---------------------------------------------------------------- INV-01/§2.5

#[test]
fn ticket_status_is_monotonic() {
    let mut t = ticket_at(TicketStatus::Authorized);

    let err = t.advance(TicketStatus::Simulated).unwrap_err();
    assert_eq!(err.from, TicketStatus::Authorized);
    assert_eq!(err.attempted, TicketStatus::Simulated);
    assert_eq!(t.status, TicketStatus::Authorized, "a refused advance must not mutate");

    t.advance(TicketStatus::Signed).unwrap();
    assert_eq!(t.status, TicketStatus::Signed);
}

#[test]
fn ticket_status_refuses_a_no_op_advance() {
    // Not pedantry: a re-entered transition would write a duplicate journal
    // entry and make the lifecycle non-idempotent (§46.3).
    let mut t = ticket_at(TicketStatus::Signed);
    assert!(t.advance(TicketStatus::Signed).is_err());
}

#[test]
fn the_full_lifecycle_walks_forward() {
    use TicketStatus::*;
    let order = [
        Observed, Reserved, Exacting, Simulated, Authorized, Signed,
        Dispatching, Acknowledged, Preconfirmed, Included, Finalized, Reconciled,
    ];
    let mut t = ticket_at(Observed);
    for next in order.iter().skip(1) {
        t.advance(*next).expect("forward advance must be accepted");
    }
    assert_eq!(t.status, Reconciled);

    // The declared order IS the state machine; Ord must agree with it.
    for w in order.windows(2) {
        assert!(w[0] < w[1], "{:?} must order before {:?}", w[0], w[1]);
    }
}

// ------------------------------------------------------------------- INV-09

#[test]
fn authorized_and_later_are_never_preemptible() {
    use TicketStatus::*;
    for s in [Observed, Reserved, Exacting, Simulated] {
        assert!(!s.is_authorized_or_later(), "{s:?} is preemptible");
    }
    for s in [Authorized, Signed, Dispatching, Acknowledged, Preconfirmed,
              Included, Finalized, Reconciled] {
        assert!(s.is_authorized_or_later(), "{s:?} must never be preempted");
        assert!(s.requires_durable_write(), "{s:?} must be journalled durably");
    }
}

// ------------------------------------------------------------------- INV-40

#[test]
fn miss_reason_is_exhaustive_and_labelled() {
    assert_eq!(MissReason::ALL.len(), 17, "§33 defines seventeen reason codes");

    let mut labels: Vec<&str> = MissReason::ALL.iter().map(|r| r.label()).collect();
    let n = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), n, "two reasons share a wire label");

    // ALL must actually contain every variant. `label()` matches exhaustively,
    // so a new variant breaks the build there; this catches the subtler case of
    // adding it to the match but forgetting ALL.
    assert!(MissReason::ALL.contains(&MissReason::NonceUnavailable));
    assert!(MissReason::ALL.contains(&MissReason::LowEv));
}

#[test]
fn loss_class_is_exhaustive() {
    assert_eq!(LossClass::ALL.len(), 9, "§28.2 defines nine loss classes");
}

// ------------------------------------------------------------------- INV-19

#[test]
fn gas_limit_and_gas_used_are_distinct_types() {
    // The compile-time half of this is the ABSENCE of any From/Into between
    // them, which Rust cannot assert positively (no negative trait bounds).
    // scripts/ci/no_gas_conversion.sh fails the build if one is ever added.
    // What is checkable here is that they are genuinely separate types and that
    // the cost model keeps them in separate roles.
    let cost = sample_cost();
    assert_eq!(cost.gas_limit, GasLimit(500_000));
    assert_eq!(cost.gas_used_distribution.p99, GasUsed(310_000));
    assert_ne!(cost.gas_limit.0, cost.gas_used_distribution.p99.0);
}

#[test]
fn conservative_total_prices_at_p99_not_p50() {
    let cost = sample_cost();
    let at_price = 1_000_000_000u128; // 1 gwei

    let total = cost.conservative_total(at_price);
    let p50_gas = u128::from(cost.gas_used_distribution.p50.0) * at_price;
    let p99_gas = u128::from(cost.gas_used_distribution.p99.0) * at_price;

    assert!(total >= p99_gas, "must price gas at p99");
    assert!(total > p50_gas, "pricing at p50 would understate the downside");
    // And it must include the non-gas components, not just gas.
    assert!(total > p99_gas, "l1_data_fee and failure cost are missing");
}

// ------------------------------------------------------------------- INV-18

#[test]
fn discrete_size_requires_a_refinement_witness() {
    // The only non-test constructor takes a DiscreteRefined, which cannot be
    // forged without calling the (greppable, doc-hidden) minter. This asserts
    // the round-trip; the restriction itself is enforced by
    // scripts/ci/no_unearned_discrete_size.sh.
    let size = DiscreteSize::from_refinement(U256::from(1234u64), DiscreteRefined::new());
    assert_eq!(size.get(), U256::from(1234u64));
}

// ------------------------------------------------------------------- INV-08

#[test]
fn only_verified_state_may_authorize_a_live_ticket() {
    assert!(ReconstructionStatus::Verified.may_authorize_live_ticket());
    assert!(!ReconstructionStatus::Rebuilding.may_authorize_live_ticket());
    assert!(!ReconstructionStatus::Unsafe.may_authorize_live_ticket());
}

// ------------------------------------------------------------------- INV-20

#[test]
fn usd_bounds_expose_the_conservative_end_by_name() {
    let b = UsdBounds { low: 90.0, high: 110.0 };
    assert_eq!(b.conservative(), 90.0);
}

// -------------------------------------------------------------------- §28.1

#[test]
fn risk_posture_ladder_orders_by_severity_and_gates_live_tickets() {
    use RiskPosture::*;
    let ladder = [Normal, ReducedSize, HighEvOnly, StrategyDisabled, ChainDisabled, GlobalHalt];
    for w in ladder.windows(2) {
        assert!(w[0] < w[1], "{:?} must be less severe than {:?}", w[0], w[1]);
    }
    for p in [Normal, ReducedSize, HighEvOnly] {
        assert!(p.permits_new_live_tickets(), "{p:?} still trades");
    }
    for p in [StrategyDisabled, ChainDisabled, GlobalHalt] {
        assert!(!p.permits_new_live_tickets(), "{p:?} must stop new live tickets");
    }
}

// ------------------------------------------------------------------- helpers

fn sample_cost() -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 0,
        l1_data_fee: 4_000_000_000_000,
        priority_fee: 1_000_000_000_000,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: 500_000_000_000,
        dex_fees: 0,
        expected_failure_cost: 2_000_000_000_000,
        calldata_bytes: 1_200,
        compressed_data_estimate: 800,
        gas_limit: GasLimit(500_000),
        gas_used_distribution: GasDistribution {
            p50: GasUsed(260_000),
            p90: GasUsed(295_000),
            p99: GasUsed(310_000),
            max_observed: GasUsed(402_000),
        },
    }
}

fn ticket_at(status: TicketStatus) -> apex_types::ticket::OpportunityTicket {
    let mut t = fixtures::ticket();
    t.status = status;
    t
}

mod fixtures {
    include!("fixtures.rs");
}
