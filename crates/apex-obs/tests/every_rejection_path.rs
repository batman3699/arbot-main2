//! Task 8.1 — **INV-40**, `obs::every_rejection_path_records_a_miss`.
//!
//! §27 asks for "an exhaustive test: every `return Reject` / `None` path in the
//! candidate pipeline is enumerated and asserted to produce a `MissRecord`".
//!
//! **Enumerated how** is the whole question. Counting `return` statements is a
//! textual exercise that a refactor silently invalidates, and it cannot tell a
//! rejection from an early return. This enumerates the *types*: every way the
//! system declines a candidate is a value of some rejection enum, each enum
//! implements `ExplainsMiss` with an exhaustive `match`, and this test walks
//! every variant of every one.
//!
//! Three things then hold that a textual enumeration could not give:
//!
//! - A new **variant** of an existing rejection breaks the `match` at compile
//!   time.
//! - A new **rejection type** with no implementation fails
//!   `scripts/ci/every_rejection_explains.sh`.
//! - A rejection that produces no record cannot compile, because
//!   `MissLedger::record` takes the rejection and derives the bucket itself.

use alloy_primitives::{Address, B256};
use apex_capture::revalidate::LastMileCheck;
use apex_capture::signer::NoLane;
use apex_chain::adapter::RejectReason;
use apex_econ::eligibility::Clause;
use apex_obs::{MissContext, MissLedger};
use apex_sim::tier0::Tier0Verdict;
use apex_types::cost::GasLimit;
use apex_types::ids::{CandidateId, ChainId, VenueId};
use apex_types::miss::{ExplainsMiss, MissReason, ObservedOutcome, SearchPath};
use apex_types::state::StateFingerprint;
use apex_types::ticket::SubmissionPolicy;
use apex_types::time::DurationNanos;
use apex_venues::admission::AdmissionError;
use std::collections::BTreeMap;

fn fingerprint() -> StateFingerprint {
    StateFingerprint {
        chain_id: ChainId::BASE,
        parent_block_hash: B256::repeat_byte(0x11),
        confirmed_block_number: 50_684_845,
        preconf_sequence: Some(7),
        flashblock_index: Some(3),
        state_root_or_equivalent: None,
        block_hash_if_available: Some(B256::repeat_byte(0x22)),
        state_delta_hash: B256::repeat_byte(0x33),
        venue_state_version: BTreeMap::new(),
        external_dependency_fingerprint: None,
    }
}

fn ctx(id: u64, ev: i128) -> MissContext {
    MissContext {
        candidate_id: CandidateId(id),
        state_fingerprint: fingerprint(),
        simulated_ev: ev,
        estimated_capture_probability: 0.4,
        path: SearchPath::Fast,
        submission_policy: SubmissionPolicy::Private,
    }
}

/// Every rejection variant this system can produce, one of each.
fn every_rejection() -> Vec<Box<dyn RejectionCase>> {
    let mut out: Vec<Box<dyn RejectionCase>> = Vec::new();
    for c in Clause::ALL {
        out.push(Box::new(c));
    }
    for c in LastMileCheck::ALL {
        out.push(Box::new(c));
    }
    for n in [NoLane::AllBusy, NoLane::NoneFunded, NoLane::NotAuthorized, NoLane::AllOutOfTheHotPool] {
        out.push(Box::new(n));
    }
    for r in [
        RejectReason::NoSafeGasLimit { needed: GasLimit(1), largest_window: 0 },
        RejectReason::StateExpiresFirst {
            valid_for: DurationNanos(1),
            earliest_landing: DurationNanos(2),
        },
        RejectReason::GasHeadroomExceedsEdge { extra_wei: 2, edge_wei: 1 },
        RejectReason::NoLaneForPolicy,
    ] {
        out.push(Box::new(r));
    }
    for a in [
        AdmissionError::MissingField("fee"),
        AdmissionError::NoBytecode { address: Address::repeat_byte(0x01) },
        AdmissionError::WrongFactory { claimed: VenueId(1), deployed_by: Address::repeat_byte(0x02) },
        AdmissionError::UnknownVenue(VenueId(9)),
        AdmissionError::TooShallow { usd_micros: 1, floor_micros: 2 },
    ] {
        out.push(Box::new(a));
    }
    out.push(Box::new(Tier0Verdict::Reject { shortfall_wei: 1 }));
    out
}

/// Type-erased so one loop can walk rejections from six different crates.
trait RejectionCase {
    fn reason(&self) -> MissReason;
    fn file(&self, ledger: &mut MissLedger, ctx: &MissContext) -> MissReason;
    fn name(&self) -> String;
}

impl<T: ExplainsMiss + std::fmt::Debug> RejectionCase for T {
    fn reason(&self) -> MissReason {
        self.miss_reason()
    }
    fn file(&self, ledger: &mut MissLedger, ctx: &MissContext) -> MissReason {
        ledger.record(ctx, self)
    }
    fn name(&self) -> String {
        format!("{self:?}")
    }
}

/// **INV-40.** Every rejection the system can produce files a `MissRecord`, and
/// the record's reason is the one the rejection itself answers with.
#[test]
fn every_rejection_path_records_a_miss() {
    let cases = every_rejection();
    // 9 eligibility clauses + 11 last-mile checks + 4 lane refusals
    // + 4 submission rejections + 5 admission errors + 1 tier-0 verdict.
    assert_eq!(cases.len(), 34, "the rejection surface changed; update the enumeration");

    let mut ledger = MissLedger::new();
    for (i, case) in cases.iter().enumerate() {
        let c = ctx(i as u64, 1_000);
        let filed = case.file(&mut ledger, &c);
        assert_eq!(filed, case.reason(), "{} filed under a different bucket", case.name());
    }

    assert_eq!(ledger.len(), cases.len(), "a rejection produced no record");
    for (i, m) in ledger.misses().iter().enumerate() {
        assert_eq!(m.record.candidate_id, CandidateId(i as u64));
        assert!(!m.detail.is_empty(), "a record lost the rejection that caused it");
    }
}

/// The ledger cannot be handed a reason, only a rejection. A
/// `record(ctx, MissReason::LowEv)` signature would let a caller under deadline
/// pressure file anything as low-EV, which is how the largest bucket becomes
/// the least informative one.
///
/// ```compile_fail
/// use apex_obs::MissLedger;
/// use apex_types::miss::MissReason;
/// let mut l = MissLedger::new();
/// // `MissReason` does not implement `ExplainsMiss`, so it is not a rejection
/// // and cannot be filed as one.
/// l.record(&unimplemented!(), &MissReason::LowEv);
/// ```
#[test]
fn a_reason_cannot_be_chosen_by_the_caller() {
    // The runtime half: two different rejections that a careless caller might
    // file identically land in different buckets, because the rejection decides.
    let mut ledger = MissLedger::new();
    let c = ctx(1, 100);
    assert_eq!(ledger.record(&c.clone(), &Clause::StateFreshness), MissReason::StaleState);
    assert_eq!(ledger.record(&c, &Clause::CostEstimateConfidence), MissReason::GasFail);
}

/// Every one of §33's seventeen buckets is reachable, or it is a bucket that
/// can never be filed and the taxonomy is describing a system other than this
/// one. The ones that are not yet reachable are named, so the list is a
/// statement rather than an omission.
#[test]
fn the_reachable_buckets_are_known() {
    let reached: std::collections::BTreeSet<MissReason> =
        every_rejection().iter().map(|c| c.reason()).collect();

    // Not reachable from a rejection *type* today, and each for a reason:
    let expected_unreachable = [
        // Phase 9: the competitor model is what observes this, and it is an
        // observation about the outcome rather than a rejection.
        MissReason::CompetitorWon,
        // Phase 12: there is no packing decision to decline yet.
        MissReason::PackingNotWorthwhile,
        // Phase 11: no conflict graph yet.
        MissReason::ConflictRejected,
        // Phase 3's L1 cost model rejects inside the cost estimate, which
        // surfaces as a Clause failure rather than its own type.
        MissReason::L1DataCostFail,
        // Phase 7's ack ladder produces these from a submission response, not
        // from a candidate rejection.
        MissReason::BuilderRejected,
        MissReason::SequencerRejected,
    ];
    for r in MissReason::ALL {
        let unreachable = expected_unreachable.contains(&r);
        assert_eq!(
            reached.contains(&r),
            !unreachable,
            "{} is {} but the list says otherwise",
            r.label(),
            if reached.contains(&r) { "reachable" } else { "unreachable" }
        );
    }
    // `SimFail` is deliberately absent from the list above: it IS reachable,
    // through `Clause::SimulationFidelity`. Writing it there was the first
    // draft's mistake, and this test caught it -- which is the point of
    // asserting the list in both directions rather than only checking that the
    // reachable ones are reachable.
    assert_eq!(Clause::SimulationFidelity.miss_reason(), MissReason::SimFail);
}

/// §33's calibration signal: a gate that keeps rejecting trades somebody else
/// then executes is a gate that is wrong, not a market that was unprofitable.
#[test]
fn a_miss_a_competitor_took_is_findable() {
    let mut ledger = MissLedger::new();
    ledger.record(&ctx(1, 5_000), &Clause::ExpectedNetEvPositive);
    ledger.record(&ctx(2, 9_000), &Clause::StateFreshness);

    assert!(ledger.taken_by_competitors().is_empty());
    assert!(ledger.observe(
        CandidateId(2),
        ObservedOutcome {
            landed_by_competitor: Some(B256::repeat_byte(0x7)),
            realized_profit_estimate: Some(8_500),
        }
    ));
    let taken = ledger.taken_by_competitors();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].record.reason, MissReason::StaleState);

    // And observing a candidate that was never missed says so rather than
    // silently doing nothing.
    assert!(!ledger.observe(CandidateId(99), ObservedOutcome {
        landed_by_competitor: None,
        realized_profit_estimate: None
    }));
}

/// **Counting misses ranks the wrong thing.** A thousand dust rejections matter
/// less than three large ones, and a count-only histogram says the opposite --
/// which is how an engineering quarter goes into the wrong gate.
#[test]
fn the_ledger_ranks_by_declined_ev_not_by_count() {
    let mut ledger = MissLedger::new();
    for i in 0..50u64 {
        ledger.record(&ctx(i, 10), &Clause::ExpectedNetEvPositive); // dust
    }
    ledger.record(&ctx(100, 5_000_000), &Clause::StateFreshness); // one big one

    let counts = ledger.by_reason();
    assert_eq!(counts["LOW_EV"], 50);
    assert_eq!(counts["STALE_STATE"], 1);

    let ev = ledger.declined_ev_by_reason();
    assert_eq!(ev["LOW_EV"], 500);
    assert_eq!(ev["STALE_STATE"], 5_000_000);
    assert!(ev["STALE_STATE"] > ev["LOW_EV"], "the ranking must invert the count");
}

/// The plane is recorded directly, so nobody has to remember to filter on
/// `edges_scanned == 0` to isolate fast-path rejections.
#[test]
fn the_search_plane_is_recorded_rather_than_inferred() {
    let mut ledger = MissLedger::new();
    for (i, path) in [SearchPath::Fast, SearchPath::Slow, SearchPath::CoverageAudit]
        .into_iter()
        .enumerate()
    {
        let mut c = ctx(i as u64, 1);
        c.path = path;
        ledger.record(&c, &Clause::ExpectedNetEvPositive);
    }
    let by_path = ledger.by_path();
    assert_eq!(by_path[&SearchPath::Fast], 1);
    assert_eq!(by_path[&SearchPath::Slow], 1);
    assert_eq!(by_path[&SearchPath::CoverageAudit], 1);
}
