//! Task 2.4 — a pool is admitted on evidence, or it is not admitted.

use alloy_primitives::{Address, B256};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, PoolId, TokenId, VenueId};
use apex_types::route::Exactness;
use apex_venues::admission::{
    AdmissionError, BytecodeEvidence, DepthEstimate, DepthPolicy, FeeBehavior, GasProfile,
    PoolAdmissionRecord, ReconstructionMethod, RevertProfile, TransferSemantics, VenueRegistry,
    EMPTY_CODE_HASH, REQUIRED_FIELDS,
};

const UNIV3: VenueId = VenueId(1);
const PANCAKE: VenueId = VenueId(3);

fn univ3_factory() -> Address {
    Address::repeat_byte(0xfa)
}
fn pancake_factory() -> Address {
    Address::repeat_byte(0xfb)
}

fn registry() -> VenueRegistry {
    VenueRegistry::new(DepthPolicy::default())
        .with_factory(UNIV3, univ3_factory())
        .with_factory(PANCAKE, pancake_factory())
}

fn token(n: u8) -> TokenId {
    TokenId {
        chain: ChainId::BASE,
        address: Address::repeat_byte(n),
    }
}

fn complete() -> PoolAdmissionRecord {
    PoolAdmissionRecord {
        pool: Some(PoolId {
            chain: ChainId::BASE,
            address: Address::repeat_byte(0x11),
        }),
        venue: Some(UNIV3),
        bytecode: Some(BytecodeEvidence {
            extcodehash: B256::repeat_byte(0x99),
            observed_at_block: 20_000_000,
        }),
        deployed_by: Some(univ3_factory()),
        tokens: Some((token(1), token(2))),
        decimals: Some((18, 6)),
        fee_behavior: Some(FeeBehavior::Static { ppm: 500 }),
        reconstruction: Some(ReconstructionMethod::ConcentratedLiquidityLogs),
        depth: Some(DepthEstimate::MeasuredOnChain {
            usd_micros: 3_600_000 * 1_000_000,
            at_block: 20_000_000,
        }),
        gas_profile: Some(GasProfile {
            per_hop: GasLimit(140_000),
            measured: false,
        }),
        revert_profile: Some(RevertProfile::default()),
        transfer_semantics: Some((TransferSemantics::Standard, TransferSemantics::Standard)),
        update_mapping: Some(vec![B256::repeat_byte(0xc4)]),
    }
}

/// Null one field at a time; every one of them must be fatal.
#[test]
fn a_pool_missing_any_admissibility_field_is_rejected() {
    assert!(registry().admit(complete()).is_ok(), "the fixture must admit");

    for field in REQUIRED_FIELDS {
        let mut json = serde_json::to_value(complete()).expect("serialisable");
        json.as_object_mut().expect("object")[*field] = serde_json::Value::Null;
        let record: PoolAdmissionRecord = serde_json::from_value(json).expect("deserialisable");
        assert_eq!(
            registry().admit(record),
            Err(AdmissionError::MissingField(field)),
            "must reject when {field} is absent"
        );
    }
}

/// The list the test above iterates must cover the whole struct.
///
/// Without this, adding a field and forgetting to list it leaves that field
/// untested — the rejection test would still pass, having checked everything
/// except the new thing.
#[test]
fn the_required_field_list_covers_every_field() {
    let json = serde_json::to_value(complete()).expect("serialisable");
    let keys: Vec<&String> = json.as_object().expect("object").keys().collect();
    let mut listed: Vec<&str> = REQUIRED_FIELDS.to_vec();
    listed.sort_unstable();
    let mut actual: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    actual.sort_unstable();
    assert_eq!(
        listed, actual,
        "REQUIRED_FIELDS has drifted from PoolAdmissionRecord"
    );
}

/// An address with no code is never admitted, whichever way the chain says so.
#[test]
fn an_address_with_no_bytecode_is_never_admitted() {
    for hash in [EMPTY_CODE_HASH, B256::ZERO] {
        let mut record = complete();
        record.bytecode = Some(BytecodeEvidence {
            extcodehash: hash,
            observed_at_block: 20_000_000,
        });
        assert!(
            matches!(
                registry().admit(record),
                Err(AdmissionError::NoBytecode { .. })
            ),
            "extcodehash {hash} must be refused"
        );
    }
}

/// B-7's shape: an invented address. `generate_base_venues.py` produced router
/// and quoter addresses that were never deployed, and nothing downstream could
/// tell — a quote against an address with no code simply fails, and a
/// *filtering* step that tolerates failures records it as "no opportunity".
#[test]
fn a_fabricated_address_cannot_reach_the_scanner() {
    let mut record = complete();
    record.bytecode = Some(BytecodeEvidence {
        extcodehash: EMPTY_CODE_HASH,
        observed_at_block: 20_000_000,
    });
    assert!(registry().admit(record).is_err());
}

/// The 33-pool misfiling, as a test.
///
/// Those pools sat under Uniswap V3 while `factory()` said PancakeSwap. Quoting
/// a PancakeSwap pool through the Uniswap quoter does not error: the quoter
/// resolves its OWN pool for that pair and fee tier and prices a different pool
/// entirely. Two of the deepest cheap pools on Base were affected.
#[test]
fn a_pool_filed_under_the_wrong_venue_is_rejected() {
    let mut record = complete();
    record.deployed_by = Some(pancake_factory()); // claimed UNIV3
    assert_eq!(
        registry().admit(record),
        Err(AdmissionError::WrongFactory {
            claimed: UNIV3,
            deployed_by: pancake_factory(),
        })
    );
}

/// A venue with no registered factory admits nothing. Fail closed: an
/// unconfigured venue is one whose pools nothing can price correctly.
#[test]
fn an_unconfigured_venue_admits_nothing() {
    let mut record = complete();
    record.venue = Some(VenueId(999));
    assert_eq!(
        registry().admit(record),
        Err(AdmissionError::UnknownVenue(VenueId(999)))
    );
}

/// The depth floor is a measured threshold, and it carries its measurement.
#[test]
fn the_depth_floor_is_measured_not_chosen() {
    let policy = DepthPolicy::default();
    assert_eq!(policy.floor_usd_micros, 100_000 * 1_000_000);
    assert!(
        policy.provenance.contains("sweep_depth_floor.py"),
        "the floor must name the measurement that produced it"
    );

    let mut record = complete();
    record.depth = Some(DepthEstimate::MeasuredOnChain {
        usd_micros: 99_999 * 1_000_000,
        at_block: 20_000_000,
    });
    assert!(matches!(
        registry().admit(record),
        Err(AdmissionError::TooShallow { .. })
    ));
}

/// A third-party TVL number may decide what to look at. It may never decide
/// how much to risk (§18.5).
#[test]
fn external_depth_ranks_but_does_not_size() {
    let external = DepthEstimate::ExternalNonAuthoritative {
        usd_micros: 5_000_000 * 1_000_000,
    };
    assert!(!external.may_size_capital());
    assert!(DepthEstimate::MeasuredOnChain {
        usd_micros: 1,
        at_block: 1
    }
    .may_size_capital());

    // It still admits: filtering on an external number is allowed.
    let mut record = complete();
    record.depth = Some(external);
    let admitted = registry().admit(record).expect("external depth still admits");
    assert!(!admitted.depth.may_size_capital());
}

/// An admitted pool whose token semantics nobody has determined is shadow-only.
///
/// A fee-on-transfer token silently eats part of a hop and every downstream
/// quote assumes it does not. Ranking such a pool is fine; dispatching against
/// it is not.
#[test]
fn an_unclassified_token_makes_the_pool_shadow_only() {
    let mut record = complete();
    record.transfer_semantics = Some((TransferSemantics::Standard, TransferSemantics::Unknown));
    let admitted = registry().admit(record).expect("still admitted");
    assert_eq!(admitted.exactness(), Exactness::Approximate);
    assert!(!admitted.exactness().may_authorize_live_dispatch());

    let fully_known = registry().admit(complete()).expect("admitted");
    assert_eq!(fully_known.exactness(), Exactness::Proven);
    assert!(fully_known.exactness().may_authorize_live_dispatch());
}

/// A known fee-on-transfer token is a DETERMINATION, not an unknown: the
/// engine can price around it.
#[test]
fn a_known_fee_on_transfer_token_is_not_the_same_as_an_unclassified_one() {
    let mut record = complete();
    record.transfer_semantics = Some((
        TransferSemantics::Standard,
        TransferSemantics::FeeOnTransfer { bps: 500 },
    ));
    let admitted = registry().admit(record).expect("admitted");
    assert_eq!(admitted.exactness(), Exactness::Proven);
}
