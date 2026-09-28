//! Task 8.3 — P&L attribution by optimization layer. §32, §26, §7.9.

use alloy_primitives::B256;
use apex_obs::pnl::{Counterfactual, PnlLedger};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::ids::{ChainId, StrategyId, TicketId, VenueId};
use apex_types::pnl::{OptimizationLayer as L, PnlAttribution, UsdBounds};

fn cost() -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 400,
        l1_data_fee: 300,
        priority_fee: 0,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: 40,
        dex_fees: 6,
        expected_failure_cost: 0,
        calldata_bytes: 1_200,
        compressed_data_estimate: 700,
        gas_limit: GasLimit(500_000),
        gas_used_distribution: GasDistribution {
            p50: GasUsed(300_000),
            p90: GasUsed(300_000),
            p99: GasUsed(300_000),
            max_observed: GasUsed(300_000),
        },
    }
}

fn trade(
    id: u64,
    chain: u64,
    strategy: u16,
    venues: &[u16],
    layers: &[L],
    net: i128,
) -> PnlAttribution {
    PnlAttribution {
        ticket_id: TicketId(id),
        chain: ChainId(chain),
        strategy: StrategyId(strategy),
        venues: venues.iter().map(|v| VenueId(*v)).collect(),
        route_hash: B256::repeat_byte(id as u8),
        optimization_layers: layers.to_vec(),
        gross_profit: net + 746,
        realized_cost: cost(),
        net_profit_token: net,
        net_profit_usd_bounds: UsdBounds { low: 0.0, high: 0.0 },
    }
}

/// **The plan's Step 1 test.** A trade touching two optimization layers
/// attributes to both.
#[test]
fn a_trade_touching_two_layers_attributes_to_both() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1], &[L::ParallelSplit, L::V4Route], 10_000));

    let r = ledger.attribute();
    assert_eq!(r.by_layer.get(L::ParallelSplit).expect("row").net_profit, 10_000);
    assert_eq!(r.by_layer.get(L::V4Route).expect("row").net_profit, 10_000);
    assert_eq!(r.total_net, 10_000, "and the trade made 10,000 once");
}

/// **The corollary the plan does not state, and the reason the type is
/// separate.** Attributing one trade to two layers means the layer column adds
/// up to more than was earned. That is not an error to be corrected by
/// splitting the profit — splitting needs a rule, and every such rule invents a
/// fact. The report says how much is counted twice instead.
#[test]
fn the_layer_column_does_not_sum_to_the_total_and_says_so() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1], &[L::ParallelSplit, L::V4Route], 10_000));
    ledger.record(trade(2, 8453, 1, &[1], &[L::SinglePath], 3_000));

    let r = ledger.attribute();
    assert_eq!(r.total_net, 13_000);
    assert_eq!(r.by_layer.sum_of_rows(), 23_000, "10,000 counted twice");
    assert_eq!(r.by_layer.double_counted(), 10_000);
    assert!(!r.by_layer.happens_to_partition());

    // The real total is available from the report, never from the column.
    assert_eq!(r.by_layer.total_net(), 13_000);
}

/// Chain and strategy DO partition — a trade has exactly one of each — and the
/// report checks that rather than assuming it.
#[test]
fn the_partitioning_dimensions_reconstruct_the_total() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1, 2], &[L::SinglePath], 10_000));
    ledger.record(trade(2, 8453, 2, &[3], &[L::EventDriven], 3_000));
    ledger.record(trade(3, 1, 1, &[1], &[L::SinglePath], -500));

    let r = ledger.attribute();
    assert_eq!(r.total_net, 12_500);
    assert!(r.by_chain.sums_to(r.total_net));
    assert!(r.by_strategy.sums_to(r.total_net));
    assert!(r.by_route.sums_to(r.total_net));

    assert_eq!(r.by_chain.get(ChainId(8453)).expect("row").net_profit, 13_000);
    assert_eq!(r.by_chain.get(ChainId(1)).expect("row").net_profit, -500);
    assert_eq!(r.by_strategy.get(StrategyId(1)).expect("row").trades, 2);
}

/// **`sums_to` has to be able to say no.**
///
/// Found by mutation: making it return `true` unconditionally broke nothing,
/// because every other test asserts only that a correct partition sums. A
/// predicate never exercised in its false direction is not tested — it is
/// decoration that happens to be true.
#[test]
fn sums_to_discriminates() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1], &[L::SinglePath], 10_000));
    ledger.record(trade(2, 1, 2, &[2], &[L::EventDriven], 2_500));

    let r = ledger.attribute();
    assert!(r.by_chain.sums_to(12_500));
    assert!(!r.by_chain.sums_to(12_501), "a wrong total must not be accepted");
    assert!(!r.by_chain.sums_to(0));
    assert!(!r.by_strategy.sums_to(r.total_net - 1));
    assert_eq!(r.by_chain.total(), 12_500);
}

/// Venues overlap the same way layers do — a trade touches several — so the
/// venue column carries the same warning.
#[test]
fn the_venue_column_overlaps_too() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1, 2, 3], &[L::SinglePath], 9_000));

    let r = ledger.attribute();
    assert_eq!(r.by_venue.sum_of_rows(), 27_000);
    assert_eq!(r.by_venue.double_counted(), 18_000);
    for v in [1u16, 2, 3] {
        assert_eq!(r.by_venue.get(VenueId(v)).expect("row").net_profit, 9_000);
    }
}

/// A single-venue, single-layer day makes both columns look like partitions.
/// `happens_to_partition` says that is a fact about today, not about the
/// dimension — which is why it is asked rather than assumed.
#[test]
fn a_column_that_happens_to_partition_says_it_is_a_coincidence() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1], &[L::SinglePath], 5_000));
    ledger.record(trade(2, 8453, 1, &[2], &[L::EventDriven], 2_000));

    let r = ledger.attribute();
    assert!(r.by_layer.happens_to_partition());
    assert!(r.by_venue.happens_to_partition());
    assert_eq!(r.by_layer.sum_of_rows(), r.total_net);
    // It is still an `Overlapping`, so the next trade touching two layers
    // cannot silently turn a dashboard column into a lie.
    assert_eq!(r.by_layer.double_counted(), 0);
}

/// A venue listed twice on one trade is one venue. Touching it twice does not
/// double the profit attributable to it.
#[test]
fn a_repeated_venue_is_counted_once() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[7, 7, 7], &[L::SinglePath], 4_000));

    let r = ledger.attribute();
    assert_eq!(r.by_venue.get(VenueId(7)).expect("row").net_profit, 4_000);
    assert_eq!(r.by_venue.get(VenueId(7)).expect("row").trades, 1);
    assert!(r.by_venue.happens_to_partition());
}

/// **§26's word is *incremental*.** Where the system recorded what the same
/// opportunity was worth without a layer, the difference is that layer's actual
/// contribution — a counterfactual the system computed, not one this module
/// invented.
#[test]
fn an_incremental_figure_comes_from_a_recorded_counterfactual() {
    let mut ledger = PnlLedger::new();
    let t = trade(1, 8453, 1, &[1], &[L::ParallelSplit], 10_000);
    let route = t.route_hash;
    ledger.record(t);
    // §26 records single_route_ev alongside split_route_ev; the split was
    // worth 10,000 and the single route would have been worth 6,500.
    ledger.record_counterfactual(
        route,
        Counterfactual { layer: L::ParallelSplit, without_layer: 6_500 },
    );

    let r = ledger.attribute();
    let row = r.by_layer.get(L::ParallelSplit).expect("row");
    assert_eq!(row.net_profit, 10_000, "the trade's whole profit touched this layer");
    assert_eq!(row.incremental, Some(3_500), "but the layer contributed 3,500 of it");
}

/// **Absent is not zero.** "This layer contributed nothing" and "nobody
/// measured what this layer contributed" are different claims and must not
/// print the same.
#[test]
fn an_unmeasured_layer_reports_none_not_zero() {
    let mut ledger = PnlLedger::new();
    ledger.record(trade(1, 8453, 1, &[1], &[L::ComputeScheduling], 10_000));

    let row = ledger.attribute().by_layer.get(L::ComputeScheduling).expect("row");
    assert_eq!(row.net_profit, 10_000);
    assert_eq!(row.incremental, None, "no counterfactual was supplied");
}

/// A layer that made things worse reports a negative increment rather than
/// being dropped. A layer nobody can turn off because its cost is invisible is
/// how a system accumulates them.
#[test]
fn a_layer_that_cost_money_reports_a_negative_increment() {
    let mut ledger = PnlLedger::new();
    let t = trade(1, 8453, 1, &[1], &[L::CrossCyclePacking], 4_000);
    let route = t.route_hash;
    ledger.record(t);
    ledger.record_counterfactual(
        route,
        Counterfactual { layer: L::CrossCyclePacking, without_layer: 6_000 },
    );

    let row = ledger.attribute().by_layer.get(L::CrossCyclePacking).expect("row");
    assert_eq!(row.incremental, Some(-2_000), "packing cost 2,000 on this route");
}

/// An empty ledger reports zeroes and no rows, and its partitions still hold.
#[test]
fn an_empty_ledger_is_consistent() {
    let r = PnlLedger::new().attribute();
    assert_eq!(r.trades, 0);
    assert_eq!(r.total_net, 0);
    assert!(r.by_chain.sums_to(0));
    assert_eq!(r.by_layer.double_counted(), 0);
    assert!(r.by_layer.rows().is_empty());
}
