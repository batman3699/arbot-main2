//! Venue code fingerprints: what they hash, what they refuse, and what they
//! deliberately leave out.

use alloy_primitives::{Address, B256};
use apex_types::ids::{PoolId, TokenId, VenueId};
use apex_venues::admission::{
    BytecodeEvidence, DepthEstimate, DepthPolicy, FeeBehavior, GasProfile, PoolAdmission,
    PoolAdmissionRecord, ReconstructionMethod, RevertProfile, TransferSemantics, VenueRegistry,
};
use apex_venues::fingerprint::{
    for_route, fingerprints, hashes, stalest, NoFingerprint, VenueFingerprint,
};
use apex_types::miss::{ExplainsMiss, MissReason};
use std::collections::BTreeMap;

const BASE: apex_types::ids::ChainId = apex_types::ids::ChainId(8453);
const UNIV3: VenueId = VenueId(1);
const AERO: VenueId = VenueId(2);

fn factory(n: u8) -> Address {
    Address::repeat_byte(n)
}

fn pool_id(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

fn registry() -> VenueRegistry {
    VenueRegistry::new(DepthPolicy::default())
        .with_factory(UNIV3, factory(0xf1))
        .with_factory(AERO, factory(0xf2))
}

/// An admitted pool. Built through `VenueRegistry::admit`, because that is the
/// only way one exists — and that is what keeps a fabricated address out of a
/// fingerprint.
fn admit(venue: VenueId, pool: u8, codehash: u8, block: u64) -> PoolAdmission {
    let deployed_by = if venue == UNIV3 { factory(0xf1) } else { factory(0xf2) };
    registry()
        .admit(PoolAdmissionRecord {
            pool: Some(pool_id(pool)),
            venue: Some(venue),
            bytecode: Some(BytecodeEvidence {
                extcodehash: B256::repeat_byte(codehash),
                observed_at_block: block,
            }),
            deployed_by: Some(deployed_by),
            tokens: Some((
                TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
                TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
            )),
            decimals: Some((18, 6)),
            fee_behavior: Some(FeeBehavior::Static { ppm: 500 }),
            reconstruction: Some(ReconstructionMethod::ConcentratedLiquidityLogs),
            depth: Some(DepthEstimate::MeasuredOnChain { usd_micros: 500_000_000_000, at_block: 100 }),
            gas_profile: Some(GasProfile { per_hop: apex_types::cost::GasLimit(120_000), measured: false }),
            revert_profile: Some(RevertProfile::default()),
            transfer_semantics: Some((TransferSemantics::Standard, TransferSemantics::Standard)),
            update_mapping: Some(vec![B256::repeat_byte(0xee)]),
        })
        .expect("a complete, consistent record")
}

/// The basic shape: one entry per venue, over the pools admitted for it.
#[test]
fn one_fingerprint_per_venue() {
    let admitted = vec![
        admit(UNIV3, 0x33, 0xaa, 100),
        admit(UNIV3, 0x34, 0xaa, 110),
        admit(AERO, 0x44, 0xbb, 120),
    ];
    let f = fingerprints(&admitted);

    assert_eq!(f.len(), 2);
    assert_eq!(f[&UNIV3].pools(), 2);
    assert_eq!(f[&AERO].pools(), 1);
    assert_ne!(f[&UNIV3].hash(), f[&AERO].hash(), "two venues, two fingerprints");
}

/// **A venue with no admitted pool has no fingerprint — not a zero hash.**
///
/// Two venues with nothing admitted would share `B256::ZERO`, and a commitment
/// over an empty venue set would be indistinguishable from one over a venue
/// whose code happened to hash to zero. Same lesson as `Recall::Undefined` and
/// `CaptureAssurance::Undefined`: an absent measurement is not a value.
#[test]
fn a_venue_with_nothing_admitted_has_no_fingerprint() {
    let f = fingerprints(&[]);
    assert!(f.is_empty(), "no entries at all, rather than one mapping to zero");

    let f = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    assert!(f.contains_key(&UNIV3));
    assert!(!f.contains_key(&AERO), "a venue nobody admitted is absent, not zero");
    assert_ne!(f[&UNIV3].hash(), B256::ZERO, "and a real one is never the zero hash either");
}

/// **The code is what is fingerprinted.** Change any pool's `extcodehash` and
/// the venue's fingerprint moves — which is the whole purpose, and what §28's
/// `contract code fingerprint change` trigger reads.
#[test]
fn a_changed_extcodehash_moves_the_fingerprint() {
    let before = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    let after = fingerprints(&[admit(UNIV3, 0x33, 0xab, 100)]);
    assert_ne!(before[&UNIV3].hash(), after[&UNIV3].hash());
}

/// The factory is in the fingerprint, because a venue whose factory changed is a
/// different venue — the fact `admit`'s `WrongFactory` check is about.
#[test]
fn the_factory_is_part_of_the_fingerprint() {
    let a = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    // Same pool, same code, registered under a venue with a different factory.
    let other = VenueRegistry::new(DepthPolicy::default()).with_factory(UNIV3, factory(0xf9));
    let b_pool = other
        .admit(PoolAdmissionRecord {
            deployed_by: Some(factory(0xf9)),
            ..record_like(UNIV3, 0x33, 0xaa, 100)
        })
        .expect("admitted under the other registry");
    let b = fingerprints(&[b_pool]);

    assert_ne!(
        a[&UNIV3].hash(),
        b[&UNIV3].hash(),
        "the same pool deployed by a different factory is a different venue"
    );
}

fn record_like(venue: VenueId, pool: u8, codehash: u8, block: u64) -> PoolAdmissionRecord {
    PoolAdmissionRecord {
        pool: Some(pool_id(pool)),
        venue: Some(venue),
        bytecode: Some(BytecodeEvidence {
            extcodehash: B256::repeat_byte(codehash),
            observed_at_block: block,
        }),
        deployed_by: Some(factory(0xf1)),
        tokens: Some((
            TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
            TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
        )),
        decimals: Some((18, 6)),
        fee_behavior: Some(FeeBehavior::Static { ppm: 500 }),
        reconstruction: Some(ReconstructionMethod::ConcentratedLiquidityLogs),
        depth: Some(DepthEstimate::MeasuredOnChain { usd_micros: 500_000_000_000, at_block: 100 }),
        gas_profile: Some(GasProfile { per_hop: apex_types::cost::GasLimit(120_000), measured: false }),
        revert_profile: Some(RevertProfile::default()),
        transfer_semantics: Some((TransferSemantics::Standard, TransferSemantics::Standard)),
        update_mapping: Some(vec![B256::repeat_byte(0xee)]),
    }
}

/// **`observed_at_block` is carried and not hashed.**
///
/// Hashing it would give unchanged code a different fingerprint every block, so
/// every commitment would differ and §17.4's deduplication — which is what the
/// commitment hash is for — would never fire.
#[test]
fn the_observation_block_does_not_change_the_hash() {
    let early = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    let late = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 999_999)]);

    assert_eq!(
        early[&UNIV3].hash(),
        late[&UNIV3].hash(),
        "the same code read at two blocks is the same code"
    );
    assert_ne!(
        early[&UNIV3].oldest_evidence_block(),
        late[&UNIV3].oldest_evidence_block(),
        "...and the age is still available, which is the point of carrying it"
    );
}

/// The age reported is the **oldest**, because a fingerprint is only as current
/// as its stalest input. Reporting the newest would let one freshly-read pool
/// vouch for nine nobody has looked at since deployment.
#[test]
fn the_age_is_the_oldest_evidence_not_the_newest() {
    let f = fingerprints(&[
        admit(UNIV3, 0x33, 0xaa, 100),
        admit(UNIV3, 0x34, 0xaa, 900),
        admit(UNIV3, 0x35, 0xaa, 500),
    ]);
    assert_eq!(f[&UNIV3].oldest_evidence_block(), 100);
    assert_eq!(f[&UNIV3].age_at(1_000), 900);
}

/// Freshness is asked with a bound, because "how old is too old" is a policy.
#[test]
fn freshness_takes_the_bound_from_the_caller() {
    let f = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 1_000)]);
    let fp = f[&UNIV3];

    assert!(fp.is_fresh_at(1_100, 200), "100 blocks old against a 200 bound");
    assert!(!fp.is_fresh_at(1_300, 200), "300 blocks old against a 200 bound");
    assert!(fp.is_fresh_at(1_200, 200), "exactly at the bound is fresh");

    // Evidence from after the head is fresh, not an error: a caller can hold a
    // head that lags the reader.
    assert!(fp.is_fresh_at(900, 0));
    assert_eq!(fp.age_at(900), 0);
}

/// Pool order in the input does not change the hash; the *set* does.
#[test]
fn the_hash_is_over_the_set_not_the_input_order() {
    let a = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100), admit(UNIV3, 0x34, 0xbb, 100)]);
    let b = fingerprints(&[admit(UNIV3, 0x34, 0xbb, 100), admit(UNIV3, 0x33, 0xaa, 100)]);
    assert_eq!(a[&UNIV3].hash(), b[&UNIV3].hash(), "sorted before hashing");

    let c = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    assert_ne!(a[&UNIV3].hash(), c[&UNIV3].hash(), "a different set is a different hash");
}

/// The same pool admitted twice must not fingerprint differently from once.
#[test]
fn a_duplicate_admission_does_not_change_the_fingerprint() {
    let once = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    let twice = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100), admit(UNIV3, 0x33, 0xaa, 100)]);
    assert_eq!(once[&UNIV3].hash(), twice[&UNIV3].hash());
    assert_eq!(twice[&UNIV3].pools(), 1, "deduplicated by address");
}

/// **The refusal that matters.** A route naming a pool nothing admitted cannot
/// be fingerprinted, so it cannot be committed.
///
/// B-7 is what happens when an address is believed rather than checked, and this
/// is that check at the commitment layer: a commitment over an unadmitted pool
/// is a commitment to code nobody verified.
#[test]
fn a_route_through_an_unadmitted_pool_is_refused() {
    let admitted = vec![admit(UNIV3, 0x33, 0xaa, 100)];

    let err = for_route(&admitted, &[pool_id(0x33), pool_id(0x99)])
        .expect_err("the second pool was never admitted");
    assert_eq!(err, NoFingerprint::UnadmittedPool { pool: pool_id(0x99) });
    assert_eq!(err.miss_reason(), MissReason::VenueDisabled, "the actionable bucket");

    // The complement, so the pair discriminates: an admitted route succeeds.
    assert!(for_route(&admitted, &[pool_id(0x33)]).is_ok());
}

/// An empty route is refused rather than returning an empty map. An empty map
/// says "this route touches no venue", which no executable route does.
#[test]
fn an_empty_route_is_refused_rather_than_answered() {
    let admitted = vec![admit(UNIV3, 0x33, 0xaa, 100)];
    assert_eq!(for_route(&admitted, &[]).expect_err("nothing to fingerprint"), NoFingerprint::EmptyRoute);
}

/// **A route's fingerprint covers only the route's pools**, not every pool the
/// venue has.
///
/// Two routes through different pools of one venue are different trades and must
/// not share a fingerprint. And a venue-wide fingerprint would change whenever
/// any unrelated pool was re-read, invalidating commitments for routes that
/// never touched it.
#[test]
fn a_routes_fingerprint_covers_only_the_pools_it_touches() {
    let admitted = vec![
        admit(UNIV3, 0x33, 0xaa, 100),
        admit(UNIV3, 0x34, 0xbb, 100),
        admit(UNIV3, 0x35, 0xcc, 100),
    ];

    let one = for_route(&admitted, &[pool_id(0x33)]).expect("admitted");
    let two = for_route(&admitted, &[pool_id(0x34)]).expect("admitted");
    assert_ne!(
        one[&UNIV3].hash(),
        two[&UNIV3].hash(),
        "two routes through different pools of one venue are different trades"
    );

    // And neither equals the venue-wide fingerprint.
    let all = fingerprints(&admitted);
    assert_ne!(one[&UNIV3].hash(), all[&UNIV3].hash());
    assert_eq!(one[&UNIV3].pools(), 1);
    assert_eq!(all[&UNIV3].pools(), 3);
}

/// A cross-venue route produces one entry per venue it touches, and no others.
#[test]
fn a_cross_venue_route_fingerprints_each_venue_it_touches() {
    let admitted = vec![
        admit(UNIV3, 0x33, 0xaa, 100),
        admit(AERO, 0x44, 0xbb, 100),
        admit(AERO, 0x45, 0xcc, 100),
    ];
    let f = for_route(&admitted, &[pool_id(0x33), pool_id(0x44)]).expect("admitted");

    assert_eq!(f.len(), 2);
    assert_eq!(f[&AERO].pools(), 1, "only the pool the route touches, not both of the venue's");
}

/// `hashes` drops the age, and taking it requires saying so.
#[test]
fn taking_only_the_hashes_is_an_explicit_step() {
    let admitted = vec![admit(UNIV3, 0x33, 0xaa, 100), admit(AERO, 0x44, 0xbb, 500)];
    let f = fingerprints(&admitted);
    let h: BTreeMap<VenueId, B256> = hashes(&f);

    assert_eq!(h.len(), 2);
    assert_eq!(h[&UNIV3], f[&UNIV3].hash());
    // The stalest venue is still answerable from the fingerprints, which is why
    // they are what a caller holds.
    assert_eq!(stalest(&f, 1_000), Some((UNIV3, 900)));
}

/// `stalest` of nothing is `None`, not a zero age. "Nothing is stale" and "there
/// is nothing" are different facts, and a caller gating on age needs both.
#[test]
fn the_stalest_of_an_empty_set_is_not_a_zero_age() {
    let empty: BTreeMap<VenueId, VenueFingerprint> = BTreeMap::new();
    assert_eq!(stalest(&empty, 1_000), None);
}

/// The preimage's elements are fixed-width, which is what makes the absent
/// length prefix safe.
///
/// A mutation deleting the prefix from the first draft changed nothing, because
/// each pool contributes exactly a 20-byte address and a 32-byte hash and the
/// preimage therefore parses uniquely. This pins the invariant that argument
/// rests on: **adding a variable-length per-pool field makes a prefix necessary
/// again**, and this test is where that shows up as a decision rather than a
/// silent collision.
#[test]
fn every_pool_contributes_the_same_number_of_bytes() {
    // `PoolId::address` is 20 bytes and `extcodehash` is 32. Asserted through
    // the types rather than as a comment, so a widened `PoolId` breaks here.
    assert_eq!(pool_id(0x33).address.as_slice().len(), 20);
    assert_eq!(B256::repeat_byte(0xaa).as_slice().len(), 32);

    // And the consequence: N pools and N+1 pools cannot collide, because the
    // preimage lengths differ by exactly 52.
    let one = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100)]);
    let two = fingerprints(&[admit(UNIV3, 0x33, 0xaa, 100), admit(UNIV3, 0x34, 0xaa, 100)]);
    assert_ne!(one[&UNIV3].hash(), two[&UNIV3].hash());
    assert_eq!(one[&UNIV3].pools() + 1, two[&UNIV3].pools());
}
