//! Task 5.3, Rust half — the commitment survives a round trip and moves when
//! the trade does.

use alloy_primitives::{Address, B256, U256};
use apex_types::commitment::ExecutionCommitment;
use apex_types::ids::{ChainId, FlashProviderId, VenueId};
use apex_types::ticket::SubmissionPolicy;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use std::collections::BTreeMap;

fn commitment() -> ExecutionCommitment {
    ExecutionCommitment {
        chain_id: ChainId::BASE,
        executor_address: Address::repeat_byte(0x11),
        executor_version: 2,
        venue_fingerprints: BTreeMap::from([
            (VenueId(1), B256::repeat_byte(0xa1)),
            (VenueId(2), B256::repeat_byte(0xa2)),
        ]),
        flash_source: FlashProviderId(3),
        state_fingerprint_hash: B256::repeat_byte(0xbb),
        route_hash: B256::repeat_byte(0xcc),
        exact_inputs: vec![U256::from(1_000u64), U256::from(2_000u64)],
        min_profit: U256::from(42u64),
        slippage_constraints: vec![30, 50],
        deadline: 1_800_000_000,
        submission_policy: SubmissionPolicy::Private,
    }
}

/// `exec::commitment_is_stable_under_reencode`: encode → decode → re-encode
/// yields the same hash.
///
/// This is what catches an encoder that loses a field, reorders one, or widens
/// a type — all of which leave a plan that still executes and is no longer the
/// plan that was simulated.
#[test]
fn commitment_is_stable_under_reencode() {
    let original = commitment();
    let json = serde_json::to_string(&original).expect("serialisable");
    let decoded: ExecutionCommitment = serde_json::from_str(&json).expect("deserialisable");
    let reencoded = serde_json::to_string(&decoded).expect("serialisable");

    assert_eq!(decoded, original);
    assert_eq!(reencoded, json, "the encoding is not canonical");
    assert_eq!(decoded.hash(), original.hash());
}

/// Every field participates. A field that can change without moving the hash
/// is a field the commitment does not protect, and finding that out later is
/// the whole failure mode.
#[test]
fn every_field_moves_the_hash() {
    let base = commitment().hash();

    /// A named change to one committed field.
    type Mutation = (&'static str, Box<dyn Fn(&mut ExecutionCommitment)>);

    let mutations: Vec<Mutation> = vec![
        ("chain_id", Box::new(|c: &mut ExecutionCommitment| c.chain_id = ChainId::ETHEREUM)),
        (
            "executor_address",
            Box::new(|c: &mut ExecutionCommitment| c.executor_address = Address::repeat_byte(0x22)),
        ),
        ("executor_version", Box::new(|c: &mut ExecutionCommitment| c.executor_version += 1)),
        (
            "venue_fingerprints",
            Box::new(|c: &mut ExecutionCommitment| {
                c.venue_fingerprints.insert(VenueId(1), B256::repeat_byte(0xff));
            }),
        ),
        ("flash_source", Box::new(|c: &mut ExecutionCommitment| c.flash_source = FlashProviderId(9))),
        (
            "state_fingerprint_hash",
            Box::new(|c: &mut ExecutionCommitment| c.state_fingerprint_hash = B256::ZERO),
        ),
        ("route_hash", Box::new(|c: &mut ExecutionCommitment| c.route_hash = B256::ZERO)),
        (
            "exact_inputs",
            Box::new(|c: &mut ExecutionCommitment| c.exact_inputs[0] += U256::from(1u64)),
        ),
        ("min_profit", Box::new(|c: &mut ExecutionCommitment| c.min_profit += U256::from(1u64))),
        (
            "slippage_constraints",
            Box::new(|c: &mut ExecutionCommitment| c.slippage_constraints[1] += 1),
        ),
        ("deadline", Box::new(|c: &mut ExecutionCommitment| c.deadline += 1)),
        (
            "submission_policy",
            Box::new(|c: &mut ExecutionCommitment| c.submission_policy = SubmissionPolicy::Public),
        ),
    ];

    for (field, mutate) in mutations {
        let mut c = commitment();
        mutate(&mut c);
        assert_ne!(c.hash(), base, "{field} does not reach the commitment");
    }
}

/// Order is content. Two routes with the same hop amounts in a different order
/// are different routes.
#[test]
fn reordering_the_hops_changes_the_commitment() {
    let mut swapped = commitment();
    swapped.exact_inputs.reverse();
    assert_ne!(swapped.hash(), commitment().hash());

    let mut slippage = commitment();
    slippage.slippage_constraints.reverse();
    assert_ne!(slippage.hash(), commitment().hash());
}

/// Length prefixes, tested at the boundary they exist for.
///
/// Without them `[1, 2] ++ [3]` and `[1] ++ [2, 3]` serialise identically, so
/// two different routes hash the same and the commitment stops distinguishing
/// them. Same problem the Solidity side solves by hashing each step's payload
/// rather than concatenating.
#[test]
fn a_sequence_boundary_cannot_be_moved_without_moving_the_hash() {
    let mut a = commitment();
    a.exact_inputs = vec![U256::from(1u64), U256::from(2u64)];
    a.slippage_constraints = vec![3];

    let mut b = commitment();
    b.exact_inputs = vec![U256::from(1u64)];
    b.slippage_constraints = vec![2, 3];

    assert_ne!(a.hash(), b.hash(), "a sequence boundary moved without moving the hash");
}

/// Two processes holding the same commitment compute the same hash, whatever
/// order they inserted the venue fingerprints in. `BTreeMap` is what makes
/// that true; a `HashMap` would make `commitment_mismatch` fire at random.
#[test]
fn insertion_order_of_venue_fingerprints_is_not_content() {
    let mut forwards = commitment();
    forwards.venue_fingerprints = BTreeMap::new();
    forwards.venue_fingerprints.insert(VenueId(1), B256::repeat_byte(0xa1));
    forwards.venue_fingerprints.insert(VenueId(2), B256::repeat_byte(0xa2));

    let mut backwards = commitment();
    backwards.venue_fingerprints = BTreeMap::new();
    backwards.venue_fingerprints.insert(VenueId(2), B256::repeat_byte(0xa2));
    backwards.venue_fingerprints.insert(VenueId(1), B256::repeat_byte(0xa1));

    assert_eq!(forwards.hash(), backwards.hash());
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/commitment.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// The round trip over arbitrary commitments, not one fixture.
    #[test]
    fn any_commitment_survives_a_round_trip(
        chain in 1u64..100_000,
        version in 0u32..1_000,
        deadline in 0u64..u64::MAX,
        inputs in prop::collection::vec(0u64..u64::MAX, 0..8),
        slippage in prop::collection::vec(0u32..10_000, 0..8),
    ) {
        let mut c = commitment();
        c.chain_id = ChainId(chain);
        c.executor_version = version;
        c.deadline = deadline;
        c.exact_inputs = inputs.into_iter().map(U256::from).collect();
        c.slippage_constraints = slippage;

        let json = serde_json::to_string(&c).expect("serialisable");
        let decoded: ExecutionCommitment = serde_json::from_str(&json).expect("deserialisable");
        prop_assert_eq!(decoded.hash(), c.hash());
        prop_assert_eq!(serde_json::to_string(&decoded).expect("serialisable"), json);
    }
}
