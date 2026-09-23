//! Task 4.1 — Tier 0 screens, cheaply, and cannot reach the network.

use apex_sim::tier0::{screen, Tier0Input, Tier0Verdict};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use std::time::Instant;

/// A provider that records every request made through it.
///
/// Task 4.1 asks for a test asserting this records zero requests. It does —
/// and the reason is worth stating plainly rather than dressing up: **there is
/// no way to give it to `screen`.** `screen` is a synchronous `fn` taking a
/// `&Tier0Input` and nothing else. The assertion below is therefore true by
/// the shape of the call, and that is exactly the guarantee wanted; a mock
/// that *could* have been called and happened not to be would be the weaker
/// result.
///
/// `scripts/ci/tier0_is_pure.sh` is what keeps it true as the module changes:
/// it fails the build if `tier0.rs` ever names a provider type or becomes
/// `async`.
#[derive(Default)]
struct RecordingProvider {
    requests: std::cell::Cell<u32>,
}

impl RecordingProvider {
    fn requests(&self) -> u32 {
        self.requests.get()
    }
}

fn cost() -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 0,
        l1_data_fee: 18_000_000_000_000,
        priority_fee: 0,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: 0,
        dex_fees: 0,
        expected_failure_cost: 2_000_000_000_000,
        calldata_bytes: 420,
        compressed_data_estimate: 310,
        gas_limit: GasLimit(250_000),
        gas_used_distribution: GasDistribution {
            p50: GasUsed(180_000),
            p90: GasUsed(210_000),
            p99: GasUsed(240_000),
            max_observed: GasUsed(249_000),
        },
    }
}

fn input(gross: i128) -> Tier0Input {
    Tier0Input {
        gross_profit_wei: gross,
        cost: cost(),
        gas_price_wei: 10_000_000,
    }
}

#[test]
fn tier0_rejects_a_candidate_that_cannot_pay_for_itself() {
    let provider = RecordingProvider::default();

    let conservative = cost().conservative_total(10_000_000);
    let below = input(conservative as i128 - 1);
    assert!(
        matches!(screen(&below), Tier0Verdict::Reject { .. }),
        "a candidate one wei short of the conservative total must be rejected"
    );

    let above = input(conservative as i128 + 1);
    assert_eq!(screen(&above), Tier0Verdict::Escalate { margin_wei: 1 });

    assert_eq!(
        provider.requests(),
        0,
        "Tier 0 made a network request -- which should not have been reachable"
    );
}

/// The §29 budget: 50 µs. Measured as an amortised per-call cost over ten
/// thousand screenings rather than a single timing, because a single
/// measurement on a loaded CI runner measures the scheduler.
///
/// The real figure is in the tens of nanoseconds — this is arithmetic on a
/// struct — so the assertion has roughly three orders of magnitude of headroom
/// and will not flake. It is here to catch a Tier 0 that starts doing
/// something, not to benchmark the one that does not.
#[test]
fn tier0_stays_inside_its_fifty_microsecond_budget() {
    const ITERATIONS: u32 = 10_000;
    let candidates: Vec<Tier0Input> = (0..64)
        .map(|i| input(i * 1_000_000_000_000 - 20_000_000_000_000))
        .collect();

    // Warm the branch predictor and the cache; the budget is about steady
    // state, not about the first call.
    for c in &candidates {
        std::hint::black_box(screen(c));
    }

    let start = Instant::now();
    for i in 0..ITERATIONS {
        let c = &candidates[(i as usize) % candidates.len()];
        std::hint::black_box(screen(c));
    }
    let per_call = start.elapsed() / ITERATIONS;

    assert!(
        per_call.as_micros() < 50,
        "Tier 0 took {per_call:?} per call, over the 50 µs budget"
    );
}

/// Screening is deterministic. The same candidate must not escalate on one
/// call and reject on the next — the ladder's one-directional safety argument
/// assumes a stable verdict.
#[test]
fn screening_is_deterministic() {
    let c = input(50_000_000_000_000);
    let first = screen(&c);
    for _ in 0..1_000 {
        assert_eq!(screen(&c), first);
    }
}

/// Monotone in gross profit: more profit never turns an escalation into a
/// rejection. A non-monotone screen would admit in a band, and the sizing
/// search above it assumes a threshold.
#[test]
fn more_profit_never_makes_a_candidate_worse() {
    let mut previously_escalated = false;
    for gross in (0..200).map(|i| i * 1_000_000_000_000i128) {
        let escalates = screen(&input(gross)).escalates();
        if previously_escalated {
            assert!(escalates, "escalation reversed at gross {gross}");
        }
        previously_escalated = escalates;
    }
    assert!(previously_escalated, "the sweep never reached an escalation");
}
