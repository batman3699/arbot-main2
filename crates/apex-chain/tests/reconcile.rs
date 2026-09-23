//! Task 7.5 — §21.5, §2.10. Outcome observation and final-state reconciliation.
//!
//! The receipt in `tests/fixtures/base_receipt.json` is a **real Base
//! transaction**, recorded by forge during a deploy and trimmed to its fee
//! fields. It is in the repository already; nothing here needs egress.

use alloy_primitives::{Address, B256};
use apex_chain::base::observe::{Receipt, TransactionObservation};
use apex_chain::base::reconcile::{reconcile, BalanceDelta, ReconcileError, ReconcileInputs};
use apex_types::ack::LifecycleStage;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, TicketId, VenueId};
use apex_types::pnl::{OptimizationLayer, UsdBounds};
use apex_types::time::UnixNanos;

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);
const WETH: Address = Address::repeat_byte(0x01);
const USDC: Address = Address::repeat_byte(0x02);

/// Parsed from the committed fixture. Hand-rolled rather than pulling in a JSON
/// dependency for six hex strings — and the numbers are written out so a reader
/// can check them against the file without running anything.
///
/// ```text
/// tx                  0xff234d403b58ff0e72d5f8adea9ce99959c040bf4d29c80d0334c406224f09cd
/// blockNumber         0x2cf260d  = 47_130_125
/// status              0x1
/// gasUsed             0x5a4b03   = 5_917_443
/// effectiveGasPrice   0x5b8d80   = 6_000_000
/// l1Fee               0x51a2e8b2 = 1_369_630_898
/// ```
fn recorded_base_receipt() -> Receipt {
    Receipt {
        tx: B256::new(hex_32(
            "ff234d403b58ff0e72d5f8adea9ce99959c040bf4d29c80d0334c406224f09cd",
        )),
        block_number: 47_130_125,
        success: true,
        gas_used: 5_917_443,
        effective_gas_price_wei: 6_000_000,
        l1_fee_wei: 1_369_630_898,
    }
}

fn hex_32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex");
    }
    out
}

fn inputs() -> ReconcileInputs {
    ReconcileInputs {
        ticket: TicketId(1),
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        venues: vec![VenueId(1), VenueId(2)],
        route_hash: B256::repeat_byte(0x44),
        optimization_layers: vec![OptimizationLayer::SinglePath],
        profit_token: WETH,
        gas_limit: GasLimit(6_500_000),
        usd_bounds: UsdBounds { low: 3.9, high: 4.1 },
        dex_fees_wei: 6_000,
        flash_fee_wei: 40_000,
        calldata_bytes: 1_200,
        compressed_data_estimate: 700,
    }
}

// ------------------------------------------------------ observation staging

/// **The three stages are three different facts about money.** Collapsing any
/// pair is how a ledger books a trade that later un-happens.
#[test]
fn observation_distinguishes_preconfirmed_included_and_finalized() {
    let r = recorded_base_receipt();
    let pre = TransactionObservation::preconfirmed(r.tx, T0);
    let inc = TransactionObservation::included(r, T0);
    let fin = TransactionObservation::finalized(r, T0);

    assert_eq!(pre.stage, LifecycleStage::Preconfirmed);
    assert_eq!(inc.stage, LifecycleStage::Included);
    assert_eq!(fin.stage, LifecycleStage::Finalized);

    // A preconfirmation has no receipt. That is the mechanical reason it cannot
    // be mistaken for an inclusion.
    assert!(pre.receipt.is_none());
    assert!(inc.receipt.is_some());

    // Executed is not settled.
    assert!(!pre.has_executed());
    assert!(inc.has_executed() && !inc.is_settled());
    assert!(fin.has_executed() && fin.is_settled());
}

/// Only finalization is safe to book against: an included transaction can still
/// be reorganized out.
#[test]
fn only_finalization_is_safe_to_book_against() {
    let r = recorded_base_receipt();
    for o in [
        TransactionObservation::preconfirmed(r.tx, T0),
        TransactionObservation::included(r, T0),
    ] {
        assert!(!o.is_settled(), "{:?} must not be bookable", o.stage);
    }
    assert!(TransactionObservation::finalized(r, T0).is_settled());
}

// ---------------------------------------------------------- reconciliation

/// **Against a recorded real trade, to the wei.**
///
/// The realized cost is what the chain charged, and every component is read
/// rather than re-derived — the L1 data fee especially, because recomputing it
/// with the estimator's own formula would make the estimator unfalsifiable.
#[test]
fn reconciliation_matches_the_recorded_receipt_to_the_wei() {
    let r = recorded_base_receipt();
    let deltas = [BalanceDelta { token: WETH, delta: 50_000_000_000 }];
    let pnl = reconcile(&r, &deltas, &inputs()).expect("a successful receipt");

    // 5_917_443 gas x 6_000_000 wei = 35_504_658_000_000 wei, exactly.
    assert_eq!(pnl.realized_cost.l2_execution_fee, 35_504_658_000_000);
    assert_eq!(pnl.realized_cost.l2_execution_fee, r.l2_execution_fee_wei());
    // Read from the receipt, not recomputed.
    assert_eq!(pnl.realized_cost.l1_data_fee, 1_369_630_898);
    assert_eq!(r.total_chain_fee_wei(), 35_504_658_000_000 + 1_369_630_898);

    // And the net is the gross less exactly that, to the wei.
    assert_eq!(pnl.gross_profit, 50_000_000_000);
    let chain_fee: i128 = 35_504_658_000_000 + 1_369_630_898;
    assert_eq!(
        pnl.net_profit_token,
        50_000_000_000_i128 - chain_fee,
        "net must be gross minus the realized chain fee, to the wei"
    );
}

/// A realized cost has no spread. Reporting p50 != p99 would invent uncertainty
/// about something already observed.
#[test]
fn a_realized_gas_figure_is_a_point_not_a_distribution() {
    let pnl =
        reconcile(&recorded_base_receipt(), &[BalanceDelta { token: WETH, delta: 1 }], &inputs())
            .expect("reconcilable");
    let d = pnl.realized_cost.gas_used_distribution;
    assert_eq!(d.p50.0, 5_917_443);
    assert_eq!(d.p50, d.p90);
    assert_eq!(d.p90, d.p99);
    assert_eq!(d.p99, d.max_observed);
    // INV-19: the limit is what was signed, the used figure is what was spent.
    assert_eq!(pnl.realized_cost.gas_limit, GasLimit(6_500_000));
    assert_ne!(pnl.realized_cost.gas_limit.0, d.p99.0);
}

/// The priority fee is already inside `effective_gas_price`, so counting it
/// again would double-charge the trade.
#[test]
fn the_priority_fee_is_not_counted_twice() {
    let pnl =
        reconcile(&recorded_base_receipt(), &[BalanceDelta { token: WETH, delta: 1 }], &inputs())
            .expect("reconcilable");
    assert_eq!(pnl.realized_cost.priority_fee, 0);
    // And the expected failure cost is ex-ante; this transaction succeeded.
    assert_eq!(pnl.realized_cost.expected_failure_cost, 0);
}

/// A reverted transaction is not a zero-profit trade. It has a P&L — the gas
/// was spent — but `PnlAttribution` is not the type for it; §28.2's `LossClass`
/// is, and it needs a cause.
#[test]
fn a_reverted_receipt_is_refused_with_what_it_cost() {
    let mut r = recorded_base_receipt();
    r.success = false;
    let err = reconcile(&r, &[BalanceDelta { token: WETH, delta: 0 }], &inputs()).unwrap_err();
    let ReconcileError::Reverted { gas_used, spent_wei, .. } = err else {
        panic!("got {err:?}");
    };
    assert_eq!(gas_used, 5_917_443);
    assert_eq!(spent_wei, 35_504_658_000_000 + 1_369_630_898);
}

/// A profit token that never moved is not a zero-profit trade either: either
/// the route did not do what it said or the wrong token was named, and
/// reporting zero would put a false zero in the ledger.
#[test]
fn a_profit_token_that_never_moved_is_refused() {
    let err = reconcile(
        &recorded_base_receipt(),
        &[BalanceDelta { token: USDC, delta: 12_345 }],
        &inputs(),
    )
    .unwrap_err();
    assert_eq!(err, ReconcileError::ProfitTokenAbsent { token: WETH });
}

/// Two deltas for one token is ambiguous accounting, and picking either is a
/// guess.
#[test]
fn duplicate_token_deltas_are_refused() {
    let err = reconcile(
        &recorded_base_receipt(),
        &[
            BalanceDelta { token: WETH, delta: 10 },
            BalanceDelta { token: WETH, delta: 20 },
        ],
        &inputs(),
    )
    .unwrap_err();
    assert_eq!(err, ReconcileError::DuplicateToken { token: WETH });
}

/// §2.10: the profit token decides, and the USD bounds are an input. A
/// reconciliation that invented a price would be quietly deciding
/// profitability with an oracle nobody chose.
#[test]
fn the_usd_bounds_come_from_the_caller() {
    let mut i = inputs();
    i.usd_bounds = UsdBounds { low: -1.0, high: 2.0 };
    let pnl = reconcile(&recorded_base_receipt(), &[BalanceDelta { token: WETH, delta: 1 }], &i)
        .expect("reconcilable");
    assert_eq!(pnl.net_profit_usd_bounds, UsdBounds { low: -1.0, high: 2.0 });
}

/// **The fixture is load-bearing, not decorative.**
///
/// `recorded_base_receipt()` writes the numbers out so a reader can check them
/// by eye; this reads the committed JSON and asserts they agree. Without it the
/// fixture is a file nobody consults and the "real receipt" claim rests on a
/// comment.
#[test]
fn the_hardcoded_receipt_matches_the_committed_fixture() {
    let raw = include_str!("fixtures/base_receipt.json");
    let v: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
    let hex = |k: &str| -> u128 {
        let s = v[k].as_str().unwrap_or_else(|| panic!("{k} missing"));
        u128::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex")
    };
    let r = recorded_base_receipt();
    assert_eq!(hex("status"), 1);
    assert_eq!(hex("gasUsed"), u128::from(r.gas_used));
    assert_eq!(hex("effectiveGasPrice"), r.effective_gas_price_wei);
    assert_eq!(hex("l1Fee"), r.l1_fee_wei);
    assert_eq!(hex("blockNumber"), u128::from(r.block_number));
    assert_eq!(
        v["transactionHash"].as_str().unwrap().trim_start_matches("0x"),
        format!("{:x}", r.tx)
    );
    // And it is a Base transaction, not a fork or a dry run.
    assert_eq!(v["_provenance"]["chain"].as_u64(), Some(8453));
}

/// **The receipt also pins the OP Stack L1 fee relation, exactly.**
///
/// `l1Fee = estimatedSizeScaled x (l1BaseFeeScalar x l1BaseFee x 16 +
/// l1BlobBaseFeeScalar x l1BlobBaseFee) / 1e12`, and for THIS transaction the
/// implied `estimatedSizeScaled` is exactly `100 x 1e6` -- the `MIN_TX_SIZE`
/// clamp. §23 predicted the clamp would be operative because the Fjord
/// intercept is negative; here is a real Base receipt where it is.
///
/// Checked across all twelve recorded Base receipts, the relation reproduces
/// every one to the wei, and **two of the twelve sit on the clamp** -- so the
/// clamp is operative sometimes, not always. That refines §23's wording and is
/// recorded in PLAN.md Task 7.5. This is evidence `apex-econ`'s estimator can
/// be validated against without egress.
#[test]
fn the_receipt_pins_the_l1_fee_relation_at_the_clamp() {
    let raw = include_str!("fixtures/base_receipt.json");
    let v: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
    let hex = |k: &str| -> u128 {
        u128::from_str_radix(v[k].as_str().expect(k).trim_start_matches("0x"), 16).expect("hex")
    };
    let weighted = hex("l1BaseFeeScalar") * hex("l1GasPrice") * 16
        + hex("l1BlobBaseFeeScalar") * hex("l1BlobBaseFee");

    const MIN_TX_SIZE_SCALED: u128 = 100 * 1_000_000;
    assert_eq!(
        MIN_TX_SIZE_SCALED * weighted / 1_000_000_000_000,
        hex("l1Fee"),
        "the clamp branch must reproduce this receipt exactly"
    );
}

/// A trade whose gross does not cover the chain fee reconciles to a negative
/// net rather than being refused. It happened, it lost money, and the ledger
/// needs the number.
#[test]
fn a_losing_trade_reconciles_to_a_negative_net() {
    let pnl =
        reconcile(&recorded_base_receipt(), &[BalanceDelta { token: WETH, delta: 1_000 }], &inputs())
            .expect("reconcilable");
    assert!(pnl.net_profit_token < 0);
    assert_eq!(pnl.gross_profit, 1_000);
}
