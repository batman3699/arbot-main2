//! The plane driven by a real `Commitments`, built from verified venue code.
//!
//! Phase 2b left this port unimplemented for a stated reason:
//! `ExecutionCommitment::venue_fingerprints` comes from the venue adapters, and
//! `apex-venues` had no such concept. Inventing one would have put a fabricated
//! value inside the hash the system deduplicates on — B-7's failure in a new
//! place, where `base_venues_complete.yaml` invented router and quoter addresses
//! and 33 pools were filed under a venue that had not deployed them.
//!
//! `apex_venues::fingerprint` is built over `PoolAdmission`, which only
//! `VenueRegistry::admit` produces. So the property under test is not "the
//! commitment is correct" but **"a route through a pool nobody verified cannot
//! be committed"** — and the first is a consequence of the second.

mod support;

use alloy_primitives::{Address, B256, U256};
use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_runtime::commit::{deadline_seconds, narrow_executor_version, VenueCommitments};
use apex_runtime::plane::{Commitments, Decline, Handled, Plane, Ports};
use apex_types::ids::{PoolId, SignerLaneId, TokenId};
use apex_types::miss::{ExplainsMiss, MissReason};
use apex_types::route::{RouteCommitment, RouteHop};
use apex_types::time::UnixNanos;
use apex_venues::admission::{
    BytecodeEvidence, DepthEstimate, DepthPolicy, FeeBehavior, GasProfile, PoolAdmission,
    PoolAdmissionRecord, ReconstructionMethod, RevertProfile, TransferSemantics, VenueRegistry,
};
use std::sync::Arc;
use support::*;

const BOOT: UnixNanos = UnixNanos(1_000_000_000);
const BLOCK: u64 = 47_079_437;
const FACTORY: u8 = 0xf1;

fn pool_at(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

fn registry() -> VenueRegistry {
    VenueRegistry::new(DepthPolicy::default()).with_factory(VENUE, Address::repeat_byte(FACTORY))
}

fn admit(pool: u8, codehash: u8, observed_at_block: u64) -> PoolAdmission {
    registry()
        .admit(PoolAdmissionRecord {
            pool: Some(pool_at(pool)),
            venue: Some(VENUE),
            bytecode: Some(BytecodeEvidence {
                extcodehash: B256::repeat_byte(codehash),
                observed_at_block,
            }),
            deployed_by: Some(Address::repeat_byte(FACTORY)),
            tokens: Some((
                TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
                TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
            )),
            decimals: Some((18, 6)),
            fee_behavior: Some(FeeBehavior::Static { ppm: 500 }),
            reconstruction: Some(ReconstructionMethod::ConcentratedLiquidityLogs),
            depth: Some(DepthEstimate::MeasuredOnChain {
                usd_micros: 500_000_000_000,
                at_block: observed_at_block,
            }),
            gas_profile: Some(GasProfile {
                per_hop: apex_types::cost::GasLimit(120_000),
                measured: false,
            }),
            revert_profile: Some(RevertProfile::default()),
            transfer_semantics: Some((TransferSemantics::Standard, TransferSemantics::Standard)),
            update_mapping: Some(vec![B256::repeat_byte(0xee)]),
        })
        .expect("a complete, consistent record")
}

/// A candidate whose route names real pools, so the commitment has something to
/// fingerprint. `support::candidate` has an empty hop list, which `for_route`
/// correctly refuses.
fn routed_candidate(pools: &[u8]) -> apex_types::candidate::Candidate {
    let mut c = candidate(1, BLOCK, 320_000_000_000);
    c.route = RouteCommitment {
        hops: pools
            .iter()
            .map(|p| RouteHop {
                venue: VENUE,
                pool: pool_at(*p),
                token_in: TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
                token_out: TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
                fee_ppm: 500,
            })
            .collect(),
        ..c.route
    };
    c.state_fingerprint = fingerprint(BLOCK, 1);
    c
}

fn auth() -> ExecutorAuth {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version }
}

fn signers() -> SignerPool {
    SignerPool::new(
        auth(),
        vec![LaneConfig {
            id: SignerLaneId(1),
            address: [0x22; 20],
            gas_reserve_wei: 1_000_000_000_000_000_000,
        }],
    )
}

/// **The property.** A route through a pool nobody admitted cannot be committed.
#[test]
fn a_route_through_an_unadmitted_pool_cannot_be_committed() {
    let commitments = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK)], 30, 1_800);

    let ok = commitments.commit(&routed_candidate(&[0x33]), &auth(), U256::from(1u64));
    assert!(ok.is_ok(), "the admitted pool commits: {ok:?}");

    let refused = commitments
        .commit(&routed_candidate(&[0x33, 0x99]), &auth(), U256::from(1u64))
        .expect_err("0x99 was never admitted");
    assert!(
        matches!(refused, Decline::VenueUnverified { .. }),
        "an unadmitted pool is a venue problem with an action attached: {refused:?}"
    );
    assert_eq!(
        refused.miss_reason(),
        MissReason::VenueDisabled,
        "the bucket that tells an operator where to look"
    );
}

/// **The fingerprint is in the hash**, so a venue whose code changed produces a
/// different commitment — which is what §17.4's deduplication then distinguishes
/// and §28's `contract code fingerprint change` trigger reads.
#[test]
fn a_changed_venue_codehash_changes_the_commitment() {
    let c = routed_candidate(&[0x33]);
    let before = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK)], 30, 1_800)
        .commit(&c, &auth(), U256::from(1u64))
        .expect("commits");
    let after = VenueCommitments::new(vec![admit(0x33, 0xab, BLOCK)], 30, 1_800)
        .commit(&c, &auth(), U256::from(1u64))
        .expect("commits");

    assert_ne!(before.venue_fingerprints, after.venue_fingerprints);
    assert_ne!(
        before.hash(),
        after.hash(),
        "the same trade against different venue code is a different commitment"
    );
}

/// Code evidence older than the bound refuses the commitment.
///
/// Measured against the block the candidate was **priced at**, not a wall clock:
/// the question is whether the code evidence is current relative to the state the
/// trade assumes, and those are the same clock.
#[test]
fn stale_code_evidence_refuses_the_commitment() {
    let c = routed_candidate(&[0x33]);

    let fresh = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK - 100)], 30, 1_800);
    assert!(fresh.commit(&c, &auth(), U256::from(1u64)).is_ok());

    let stale = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK - 5_000)], 30, 1_800);
    let err = stale.commit(&c, &auth(), U256::from(1u64)).expect_err("5,000 blocks is past 1,800");
    assert!(matches!(err, Decline::VenueUnverified { .. }), "{err:?}");
    assert_eq!(err.miss_reason(), MissReason::VenueDisabled);
}

/// **The executor version is refused rather than truncated.**
///
/// `ExecutorAuth` carries 32 bytes and the commitment holds a `u32`. Truncating
/// would let two deployments differing only in their high bytes share a
/// commitment — exactly the change a version field exists to notice.
#[test]
fn an_executor_version_that_does_not_fit_is_refused() {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&7u32.to_be_bytes());
    assert_eq!(narrow_executor_version(version).expect("fits"), 7);

    // A high byte set: the same low four bytes, a different version.
    version[0] = 1;
    let err = narrow_executor_version(version).expect_err("must not truncate");
    assert!(matches!(err, Decline::Uncommittable { .. }), "{err:?}");

    // And it reaches the commitment path rather than being lost in a helper.
    let mut wide = auth();
    wide.executor_version[0] = 1;
    let refused = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK)], 30, 1_800)
        .commit(&routed_candidate(&[0x33]), &wide, U256::from(1u64))
        .expect_err("must refuse");
    assert!(matches!(refused, Decline::Uncommittable { .. }), "{refused:?}");
}

/// The deadline truncates to seconds rather than rounding, because a deadline
/// that is too early costs an opportunity while one that is too late executes a
/// stale trade.
#[test]
fn the_deadline_truncates_rather_than_rounds() {
    assert_eq!(deadline_seconds(UnixNanos(1_781_049_614_999_999_999)), 1_781_049_614);
    assert_eq!(deadline_seconds(UnixNanos(1_781_049_615_000_000_000)), 1_781_049_615);
    assert_eq!(deadline_seconds(UnixNanos(0)), 0);
}

/// One slippage bound per hop, and a route with no hops is refused before this
/// can produce the empty vector that would say "no constraints".
#[test]
fn slippage_constraints_are_one_per_hop() {
    let commitments =
        VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK), admit(0x44, 0xbb, BLOCK)], 30, 1_800);

    let two = commitments
        .commit(&routed_candidate(&[0x33, 0x44]), &auth(), U256::from(1u64))
        .expect("commits");
    assert_eq!(two.slippage_constraints, vec![30, 30]);

    // An empty route is refused, not committed with an empty constraint list.
    let mut empty = routed_candidate(&[]);
    empty.route.hops.clear();
    let err = commitments.commit(&empty, &auth(), U256::from(1u64)).expect_err("no hops");
    assert!(matches!(err, Decline::Uncommittable { .. }), "{err:?}");
}

/// The state fingerprint is in the commitment, so the same route priced against
/// different state is a different opportunity — which is the distinction the
/// contract's own `planCommitment` deliberately does not make, and the reason
/// this commitment exists alongside it.
#[test]
fn the_state_the_trade_was_priced_against_is_committed() {
    let commitments = VenueCommitments::new(vec![admit(0x33, 0xaa, BLOCK)], 30, 1_800);

    let a = commitments
        .commit(&routed_candidate(&[0x33]), &auth(), U256::from(1u64))
        .expect("commits");

    let mut later = routed_candidate(&[0x33]);
    later.state_fingerprint = fingerprint(BLOCK, 2);
    let b = commitments.commit(&later, &auth(), U256::from(1u64)).expect("commits");

    assert_ne!(a.state_fingerprint_hash, b.state_fingerprint_hash);
    assert_ne!(a.hash(), b.hash(), "the same route against moved state is a new opportunity");
}

/// **End to end.** The plane, with a real search and a real `Commitments`,
/// drives a ticket to `Reconciled`.
#[tokio::test]
async fn the_plane_runs_on_a_real_commitments() {
    let c = routed_candidate(&[0x33]);
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(signers()),
        gate: Arc::new(DispatchGate::shut()),
        dispatcher: Arc::new(NullDispatcher::new()),
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![c.clone()])),
        econ: Arc::new(PassThroughEconomics(c)),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(VenueCommitments::new(
            vec![admit(0x33, 0xaa, BLOCK)],
            30,
            1_800,
        )),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");

    let event = recorded_stream().first().cloned().expect("an event");
    let handled = plane.on_event(&event).await;
    let Some(Handled::Closed { outcome, .. }) = handled.first() else {
        panic!("expected a reconciled ticket, got {handled:?}");
    };
    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(plane.registry().metrics().ticket_drop_count(), 0);
    assert_eq!(plane.commitments_in_flight(), 0, "the lock was released");
}

/// ...and the same plane with the pool unadmitted admits no ticket at all.
#[tokio::test]
async fn an_unverified_venue_stops_the_plane_before_a_ticket_exists() {
    let c = routed_candidate(&[0x33]);
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(signers()),
        gate: Arc::new(DispatchGate::shut()),
        dispatcher: Arc::new(NullDispatcher::new()),
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![c.clone()])),
        econ: Arc::new(PassThroughEconomics(c)),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        // Nothing admitted.
        commitments: Arc::new(VenueCommitments::new(Vec::new(), 30, 1_800)),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");

    let event = recorded_stream().first().cloned().expect("an event");
    let handled = plane.on_event(&event).await;

    assert!(
        matches!(handled.first(), Some(Handled::Declined(Decline::VenueUnverified { .. }))),
        "expected a venue refusal before any ticket existed, got {handled:?}"
    );
    assert_eq!(plane.registry().metrics().tickets_admitted, 0);
    assert_eq!(plane.misses().len(), 1, "and the refusal is a recorded miss");
    assert_eq!(
        plane.misses().by_reason().keys().copied().collect::<Vec<_>>(),
        vec!["VENUE_DISABLED"],
        "the actionable bucket, not a catch-all"
    );
}
