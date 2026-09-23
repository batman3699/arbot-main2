//! Task 6.4 — INV-35, §24.6.
//!
//! The plan's Step 1 sketch asked for `LastMile::CHECKS` and a helper
//! `passing_but_failing(check)`. That helper is the whole test: it takes a
//! context that passes everything and breaks **exactly one** input. If the
//! implementation's checks were entangled -- one comparison covering for
//! another, or a single early guard subsuming several -- the table would show
//! it as a check that cannot be made to fire on its own.

use apex_capture::revalidate::{
    doc_fixture, last_mile, LastMileCheck, LastMileContext, SigningAuthorization,
};
use apex_capture::signer::NonceLane;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SignerLaneId, VenueId};
use apex_types::time::UnixNanos;

fn passing() -> LastMileContext {
    doc_fixture()
}

/// Break exactly one input, leaving the other ten checks satisfied.
fn passing_but_failing(check: LastMileCheck) -> LastMileContext {
    let mut c = passing();
    match check {
        LastMileCheck::ChainId => c.signer_chain = ChainId(1),
        LastMileCheck::ExecutorFingerprint => c.live_executor_version = 3,
        LastMileCheck::NonceOwnership => {
            c.nonce = NonceLane::new(SignerLaneId(7)).reserve(5, UnixNanos(0));
        }
        LastMileCheck::Deadline => c.now = c.dispatch_deadline,
        LastMileCheck::MinProfit => c.expected_net_profit = c.min_profit - 1,
        LastMileCheck::FeeCeiling => c.observed_fee_wei = c.fee_ceiling_wei + 1,
        LastMileCheck::GasEligibility => c.required_gas = GasLimit(c.remaining_block_gas + 1),
        LastMileCheck::SignerBalance => c.signer_balance_wei = c.required_reserve_wei - 1,
        LastMileCheck::FlashAvailability => c.flash_available = c.flash_required - 1,
        LastMileCheck::HookFingerprint => c.live_hooks = Some([0xAA; 32]),
        LastMileCheck::CriticalPoolState => {
            c.live_venue_versions.insert(VenueId(2), 10);
        }
    }
    c
}

/// The plan's `each_revalidation_check_can_reject`, over all eleven.
#[test]
fn each_revalidation_check_can_reject() {
    for check in LastMileCheck::ALL {
        let ctx = passing_but_failing(check);
        match last_mile(&ctx) {
            Err(e) => assert_eq!(e.check, check, "{check} was masked by {}", e.check),
            Ok(_) => panic!("check {check} did not gate"),
        }
    }
}

/// And the baseline the table rests on: the unperturbed context passes. Without
/// this, a `last_mile` that rejected everything would make the table above
/// green while gating nothing usefully.
#[test]
fn a_context_that_satisfies_everything_passes() {
    assert!(last_mile(&passing()).is_ok());
}

/// `ALL` is hand-written, so a twelfth variant added to the enum and forgotten
/// there would leave a silent gap in the table test. This is the counterweight:
/// it fails to compile if the match is not exhaustive.
#[test]
fn the_check_list_is_complete() {
    fn every_variant(c: LastMileCheck) -> usize {
        match c {
            LastMileCheck::ChainId => 0,
            LastMileCheck::ExecutorFingerprint => 1,
            LastMileCheck::NonceOwnership => 2,
            LastMileCheck::Deadline => 3,
            LastMileCheck::MinProfit => 4,
            LastMileCheck::FeeCeiling => 5,
            LastMileCheck::GasEligibility => 6,
            LastMileCheck::SignerBalance => 7,
            LastMileCheck::FlashAvailability => 8,
            LastMileCheck::HookFingerprint => 9,
            LastMileCheck::CriticalPoolState => 10,
        }
    }
    let mut seen: Vec<usize> = LastMileCheck::ALL.iter().copied().map(every_variant).collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..11).collect::<Vec<_>>(), "ALL does not list every variant exactly once");
}

/// INV-05: `wrong_chain_submission == 0`. Named separately from the table
/// because §8 names this test, and because it is the one check whose failure is
/// a global halt rather than a re-simulate.
#[test]
fn chain_id_mismatch_refuses_to_sign() {
    let mut c = passing();
    c.signer_chain = ChainId(1);
    let e = last_mile(&c).unwrap_err();
    assert_eq!(e.check, LastMileCheck::ChainId);
    assert!(e.detail.contains("8453"), "the detail must name the chains: {}", e.detail);
}

/// A venue the live state no longer carries is a *change*, not an absence. §5.6
/// forbids converting a gap into "probably unchanged", and this is where that
/// conversion would be most expensive: immediately before signing.
#[test]
fn a_venue_missing_from_live_state_is_a_rejection_not_a_pass() {
    let mut c = passing();
    c.live_venue_versions.remove(&VenueId(2));
    let e = last_mile(&c).unwrap_err();
    assert_eq!(e.check, LastMileCheck::CriticalPoolState);
    assert!(e.detail.contains("no longer"), "got: {}", e.detail);
}

/// Only the venues the route touches. A venue that moved but was never
/// committed to is not this route's problem, and re-reading it would spend the
/// `T_sign` budget the ordering exists to protect.
#[test]
fn an_uncommitted_venue_moving_does_not_reject() {
    let mut c = passing();
    c.live_venue_versions.insert(VenueId(99), 12345);
    assert!(last_mile(&c).is_ok());
}

/// The deadline is exclusive: *at* the deadline the ticket is already late.
#[test]
fn the_deadline_check_is_exclusive() {
    let mut c = passing();
    c.now = UnixNanos(c.dispatch_deadline.0 - 1);
    assert!(last_mile(&c).is_ok());
    c.now = c.dispatch_deadline;
    assert_eq!(last_mile(&c).unwrap_err().check, LastMileCheck::Deadline);
}

/// Exactly at the floor is acceptable; below it is not. An off-by-one here
/// either refuses trades that clear or accepts trades that do not.
#[test]
fn the_profit_floor_is_inclusive() {
    let mut c = passing();
    c.expected_net_profit = c.min_profit;
    assert!(last_mile(&c).is_ok());
    c.expected_net_profit = c.min_profit - 1;
    assert_eq!(last_mile(&c).unwrap_err().check, LastMileCheck::MinProfit);
}

/// The token is what a signer accepts, and it only comes from a pass. The
/// compile-fail half -- forging the token, and calling `new` without one -- is
/// in `revalidate.rs`'s doctests, per the convention in `apex_types::candidate`.
#[test]
fn a_signing_authorization_can_only_be_built_from_a_pass() {
    let c = passing();
    let proof = last_mile(&c).expect("a passing context");
    let auth = SigningAuthorization::new(c.ticket, c.nonce, proof);
    assert_eq!(auth.ticket(), c.ticket);
    assert_eq!(auth.nonce(), c.nonce);
}

/// Every check reports itself, so the rejection histogram has eleven buckets
/// rather than one called "revalidation failed".
#[test]
fn a_rejection_names_its_check_and_its_ticket() {
    for check in LastMileCheck::ALL {
        let e = last_mile(&passing_but_failing(check)).unwrap_err();
        assert_eq!(e.ticket, passing().ticket);
        assert!(!e.detail.is_empty(), "{check} rejected without saying why");
        assert!(e.to_string().contains(check.name()));
    }
}
