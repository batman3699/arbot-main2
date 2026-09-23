//! Tasks 7.3 and 7.3a — §21.1, §21.5, §24.2, §24.4. INV-10, INV-34, B-14.

use apex_chain::base::submit::{
    blockpi_base_lane, send_redundantly, BaseTransactionStatus, EndpointKind, LaneRefusal,
    PrivacyEvidence, RedundancyOutcome, SubmissionLane, BLOCKPI_ATTESTATION_TTL,
};
use apex_types::ack::LifecycleStage;
use apex_types::ids::SubmissionLaneId;
use apex_types::ticket::SubmissionPolicy;
use apex_types::time::{DurationNanos, UnixNanos};
use std::cell::RefCell;

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);

fn at(ns: u64) -> UnixNanos {
    UnixNanos(T0.0 + ns)
}

/// What Base runs today: one attested private lane, one public fallback.
fn configured_lanes() -> Vec<SubmissionLane> {
    vec![
        blockpi_base_lane(SubmissionLaneId(1), T0),
        SubmissionLane {
            id: SubmissionLaneId(2),
            endpoint: EndpointKind::PublicRpc,
            policy: SubmissionPolicy::Public,
            privacy_evidence: None,
        },
    ]
}

// ------------------------------------------------------------------- 7.3a

/// **The plan's `a_protected_policy_requires_recorded_evidence`.**
#[test]
fn a_protected_policy_requires_recorded_evidence() {
    for lane in configured_lanes() {
        if lane.policy != SubmissionPolicy::Public {
            let ev = lane.privacy_evidence.unwrap_or_else(|| {
                panic!("lane {} claims {:?} with no evidence", lane.id.0, lane.policy)
            });
            assert!(ev.is_fresh(at(1)), "evidence for {} is stale", lane.id.0);
        }
        assert!(lane.may_dispatch(at(1)).is_ok(), "lane {} refused", lane.id.0);
    }
}

/// **The plan's `evidence_distinguishes_provider_attestation_from_measurement`.**
#[test]
fn evidence_distinguishes_provider_attestation_from_measurement() {
    let attested = PrivacyEvidence::ProviderAttested {
        source: "vendor doc",
        checked_at: T0,
        ttl: DurationNanos(1_000),
    };
    let measured = PrivacyEvidence::Measured {
        probe: "mempool-absence",
        checked_at: T0,
        ttl: DurationNanos(1_000),
    };
    assert!(attested.confidence() < measured.confidence());
    // Both admissible: neither is zero, because "we have not measured it" is
    // not the same as "we have no reason to believe it".
    assert!(attested.confidence() > 0.0);
}

/// A lane claiming protection with nothing behind it is refused, named. This is
/// B-14 exactly: §14.1 prices lanes by policy, so an unevidenced claim is a
/// lane priced above what is known about it.
#[test]
fn an_unevidenced_private_lane_is_refused() {
    let bare = SubmissionLane {
        id: SubmissionLaneId(9),
        endpoint: EndpointKind::FlashblocksAwareRpc,
        policy: SubmissionPolicy::Private,
        privacy_evidence: None,
    };
    assert_eq!(
        bare.may_dispatch(T0),
        Err(LaneRefusal::UnevidencedPolicy { policy: SubmissionPolicy::Private })
    );
    assert_eq!(bare.privacy_confidence(T0), 0.0);
}

/// **Evidence past its TTL is none, not less.** The guarantee is a dashboard
/// setting somebody can change without a deploy, so an old attestation says
/// what was true then and nothing about now.
#[test]
fn stale_evidence_is_no_evidence() {
    let lane = blockpi_base_lane(SubmissionLaneId(1), T0);
    let inside = at(BLOCKPI_ATTESTATION_TTL.0 - 1);
    let outside = at(BLOCKPI_ATTESTATION_TTL.0 + 1);

    assert!(lane.may_dispatch(inside).is_ok());
    assert!(lane.privacy_confidence(inside) > 0.0);

    assert!(matches!(lane.may_dispatch(outside), Err(LaneRefusal::StaleEvidence { .. })));
    assert_eq!(lane.privacy_confidence(outside), 0.0, "a stale lane must not be priced as private");
}

/// A public lane needs no evidence — there is nothing to attest to — and is
/// never credited with privacy confidence either.
#[test]
fn a_public_lane_needs_no_evidence_and_gets_no_credit() {
    let public = SubmissionLane {
        id: SubmissionLaneId(2),
        endpoint: EndpointKind::PublicRpc,
        policy: SubmissionPolicy::Public,
        privacy_evidence: None,
    };
    assert!(public.may_dispatch(T0).is_ok());
    assert_eq!(public.privacy_confidence(T0), 0.0);
}

/// Base's lane is seeded as attested, not measured. Recording it as measured
/// would overstate what is known — nothing has observed a transaction's absence
/// from the public mempool — and §14.1 would price the lane above its evidence.
#[test]
fn the_base_lane_is_seeded_as_attested_not_measured() {
    let lane = blockpi_base_lane(SubmissionLaneId(1), T0);
    let Some(PrivacyEvidence::ProviderAttested { source, ttl, .. }) = lane.privacy_evidence else {
        panic!("Base's lane must be ProviderAttested until a probe measures it");
    };
    assert!(source.contains("blockpi"));
    assert_eq!(ttl, BLOCKPI_ATTESTATION_TTL);
    assert!(
        ttl.0 <= 24 * 3_600_000_000_000,
        "an attestation about a dashboard setting must expire within a day"
    );
}

// ------------------------------------------------------------------- 7.3

/// **§21.1's feed policy.** The raw infrastructure stream is a read source, not
/// a dispatch target.
#[test]
fn dispatch_never_goes_to_the_raw_infrastructure_stream() {
    let raw = SubmissionLane {
        id: SubmissionLaneId(3),
        endpoint: EndpointKind::RawInfrastructureStream,
        policy: SubmissionPolicy::Private,
        privacy_evidence: Some(PrivacyEvidence::Measured {
            probe: "even with perfect evidence",
            checked_at: T0,
            ttl: DurationNanos(u64::MAX),
        }),
    };
    assert_eq!(
        raw.may_dispatch(T0),
        Err(LaneRefusal::NotADispatchTarget(EndpointKind::RawInfrastructureStream)),
        "no amount of evidence makes the stream a lane"
    );
    assert!(!EndpointKind::RawInfrastructureStream.is_dispatchable());
    assert!(EndpointKind::FlashblocksAwareRpc.is_dispatchable());
    assert!(EndpointKind::PublicRpc.is_dispatchable());
}

/// **INV-34 at the rung where it is most tempting.** `Known` sounds like the
/// transaction is safe. §21.5: it is evidence the preconfirmation node
/// *received* it, and nothing about a Flashblock.
#[test]
fn base_transaction_status_known_maps_to_node_known_not_included() {
    assert_eq!(BaseTransactionStatus::Known.stage(), Some(LifecycleStage::NodeKnown));
    assert_ne!(BaseTransactionStatus::Known.stage(), Some(LifecycleStage::Included));
    assert!(!BaseTransactionStatus::Known
        .stage()
        .is_some_and(LifecycleStage::implies_inclusion));

    assert_eq!(BaseTransactionStatus::Preconfirmed.stage(), Some(LifecycleStage::Preconfirmed));
    assert!(!BaseTransactionStatus::Preconfirmed
        .stage()
        .is_some_and(LifecycleStage::implies_inclusion));

    assert_eq!(BaseTransactionStatus::Included.stage(), Some(LifecycleStage::Included));
    assert!(BaseTransactionStatus::Included.stage().is_some_and(LifecycleStage::implies_inclusion));

    // Neither an absence nor a rejection is a rung on the ladder.
    assert_eq!(BaseTransactionStatus::Unknown.stage(), None);
    assert_eq!(BaseTransactionStatus::Rejected.stage(), None);
}

/// **INV-10.** Every lane gets the same bytes, and the function has no way to
/// give them anything else.
#[test]
fn redundant_transport_sends_identical_signed_bytes() {
    let signed = vec![0xAB, 0xCD, 0xEF, 0x01];
    let seen: RefCell<Vec<(SubmissionLaneId, Vec<u8>)>> = RefCell::new(Vec::new());

    let outcome = send_redundantly(
        &signed,
        &configured_lanes(),
        at(1),
        || true,
        |lane, bytes| -> Result<(), ()> {
            seen.borrow_mut().push((lane.id, bytes.to_vec()));
            Ok(())
        },
    );

    assert_eq!(
        outcome,
        RedundancyOutcome::SentToAll { lanes: vec![SubmissionLaneId(1), SubmissionLaneId(2)] }
    );
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2);
    for (lane, bytes) in seen.iter() {
        assert_eq!(bytes, &signed, "lane {} got different bytes", lane.0);
    }
}

/// **A stale opportunity cancels the fallback before dispatch.**
///
/// The freshness check runs before *each* send, not once at the start: the
/// whole point is that a fallback must not go out after the opportunity has
/// already died on the primary.
#[test]
fn a_stale_opportunity_cancels_the_fallback_before_dispatch() {
    let signed = vec![0x11; 8];
    let sends = RefCell::new(0usize);

    let outcome = send_redundantly(
        &signed,
        &configured_lanes(),
        at(1),
        // Fresh for the first lane, stale by the second.
        || *sends.borrow() < 1,
        |_lane, _bytes| -> Result<(), ()> {
            *sends.borrow_mut() += 1;
            Ok(())
        },
    );

    assert_eq!(
        outcome,
        RedundancyOutcome::CancelledAsStale {
            sent: vec![SubmissionLaneId(1)],
            cancelled: vec![SubmissionLaneId(2)],
        }
    );
    assert_eq!(*sends.borrow(), 1, "the fallback was dispatched after the opportunity died");
}

/// An ineligible lane is skipped rather than failing the whole send: the
/// primary landing while a misconfigured fallback is skipped is a good outcome,
/// and refusing everything would turn one bad lane into a total outage.
#[test]
fn an_ineligible_lane_is_skipped_not_fatal() {
    let mut lanes = configured_lanes();
    lanes.push(SubmissionLane {
        id: SubmissionLaneId(3),
        endpoint: EndpointKind::RawInfrastructureStream,
        policy: SubmissionPolicy::Private,
        privacy_evidence: None,
    });
    let seen = RefCell::new(Vec::new());
    let outcome = send_redundantly(&[0x01], &lanes, at(1), || true, |l, _| -> Result<(), ()> {
        seen.borrow_mut().push(l.id);
        Ok(())
    });
    assert_eq!(
        outcome,
        RedundancyOutcome::SentToAll { lanes: vec![SubmissionLaneId(1), SubmissionLaneId(2)] }
    );
    assert!(!seen.borrow().contains(&SubmissionLaneId(3)));
}

/// A lane whose evidence has gone stale drops out of the redundant set without
/// taking the send down — which is what makes a short TTL safe to run with.
#[test]
fn a_lane_that_goes_stale_drops_out_of_the_redundant_set() {
    let lanes = configured_lanes();
    let seen = RefCell::new(Vec::new());
    let outcome = send_redundantly(
        &[0x01],
        &lanes,
        at(BLOCKPI_ATTESTATION_TTL.0 + 1),
        || true,
        |l, _| -> Result<(), ()> {
            seen.borrow_mut().push(l.id);
            Ok(())
        },
    );
    assert_eq!(outcome, RedundancyOutcome::SentToAll { lanes: vec![SubmissionLaneId(2)] });
    assert_eq!(*seen.borrow(), vec![SubmissionLaneId(2)], "only the public fallback remains");
}
