//! Task 8.5 R9 — pool admissions from the live book
//! (`apex_runtime::live::admission`).
//!
//! The properties that matter: every field comes from something the book read
//! or says it did not; a refusal is reported with its reason; and evidence the
//! commitments refuse as stale is made current again by replacing it, without
//! changing the commitment a route produces.

mod support;

use alloy_primitives::{address, Address, B256, U256};
use apex_capture::signer::ExecutorAuth;
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::admission::{self, LiveCommitments, GAS_PER_HOP};
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::feed::{BURN, MINT, SWAP};
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::plane::{Commitments, Decline};
use apex_types::candidate::Candidate;
use apex_types::ids::{ChainId, PoolId};
use apex_types::route::{Exactness, RouteHop};
use apex_types::state::ReconstructionStatus;
use apex_venues::admission::{
    AdmissionError, BytecodeEvidence, DepthEstimate, FeeBehavior, GasProfile, PoolAdmission, ReconstructionMethod,
    TransferSemantics,
};

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const BSDETH: Address = address!("cb327b99ff831bf8223cced12b1338ff3aa322ff");
/// WETH/USDC on Uniswap V3 (0.05%), WETH/USDC on Slipstream (CL100), and
/// WETH/bsdETH on Slipstream: the universe's two pairs.
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const BSD: Address = address!("2ae9df02539887d4ebce0230168a302d34784c82");
const CODE: B256 = B256::repeat_byte(0xc0);
const READ_AT: u64 = 100;

fn pool(addr: Address, venue: Venue, token1: Address, depth_usd: f64) -> PoolSnapshot {
    PoolSnapshot {
        // The inventory's fee is not the chain's: the record must take the chain's.
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1, fee_ppm: 3_000, depth_usd },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(-197_350).unwrap(),
            liquidity: 1_400_000_000_000_000_000,
            tick: -197_350,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(vec![(-199_000, 1), (-196_000, -1)], -200_000, -195_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: CODE,
        block: READ_AT,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
    }
}

fn universe() -> [PoolSnapshot; 3] {
    [
        pool(UNI, Venue::UniswapV3, USDC, 5e6),
        pool(SLIP, Venue::Slipstream, USDC, 4e6),
        pool(BSD, Venue::Slipstream, BSDETH, 1.25e6),
    ]
}

fn book(pools: impl IntoIterator<Item = PoolSnapshot>) -> PoolBook {
    PoolBook::from_snapshots(pools, ReconstructionStatus::Verified)
}

fn admitted(pools: impl IntoIterator<Item = PoolSnapshot>, read_block: u64) -> Vec<PoolAdmission> {
    let (admitted, refused) = admission::admit_book(&book(pools), ChainId::BASE, read_block);
    assert!(refused.is_empty(), "{refused:?}");
    admitted
}

fn find(admitted: &[PoolAdmission], pool: Address) -> &PoolAdmission {
    admitted.iter().find(|a| a.pool.address == pool).unwrap()
}

/// **Every field is evidence the book read, or says it is not.**
#[test]
fn every_pool_is_admitted_with_the_evidence_the_book_read() {
    let a = admitted(universe(), READ_AT);
    assert_eq!(a.len(), 3);

    let uni = find(&a, UNI);
    assert_eq!(uni.pool, PoolId { chain: ChainId::BASE, address: UNI });
    assert_eq!(uni.venue, Venue::UniswapV3.id());
    assert_eq!(uni.bytecode, BytecodeEvidence { extcodehash: CODE, observed_at_block: READ_AT });
    assert_eq!(uni.deployed_by, Venue::UniswapV3.factory());
    assert_eq!((uni.tokens.0.address, uni.tokens.1.address), (WETH, USDC));
    assert_eq!(uni.decimals, (18, 6));
    assert_eq!(uni.fee_behavior, FeeBehavior::Static { ppm: 500 }, "the chain's fee, not the inventory's");
    assert_eq!(uni.reconstruction, ReconstructionMethod::ConcentratedLiquidityLogs);
    assert_eq!(uni.depth, DepthEstimate::ExternalNonAuthoritative { usd_micros: 5_000_000_000_000 });
    assert_eq!(uni.gas_profile, GasProfile { per_hop: GAS_PER_HOP, measured: false });
    assert_eq!(uni.update_mapping, vec![SWAP, MINT, BURN]);

    // Slipstream's fee is its module's, and moves.
    assert_eq!(find(&a, SLIP).fee_behavior, FeeBehavior::Dynamic);
    assert_eq!(find(&a, SLIP).deployed_by, Venue::Slipstream.factory());
}

/// **Standard only where checked.** bsdETH is classified by nobody, so its
/// pool is admitted — it ranks, and runs in the shadow — but never proven.
#[test]
fn transfers_are_standard_only_for_the_tokens_checked() {
    let a = admitted(universe(), READ_AT);
    let standard = (TransferSemantics::Standard, TransferSemantics::Standard);
    assert_eq!(find(&a, UNI).transfer_semantics, standard);
    assert_eq!(find(&a, SLIP).transfer_semantics, standard);
    assert_eq!(find(&a, UNI).exactness(), Exactness::Proven);

    let bsd = find(&a, BSD);
    assert_eq!(bsd.transfer_semantics, (TransferSemantics::Standard, TransferSemantics::Unknown));
    assert_eq!(bsd.exactness(), Exactness::Approximate);
}

/// **A refusal is named, with its reason** — and the rest are still admitted.
#[test]
fn a_refused_pool_is_reported_with_its_reason() {
    let no_code = Address::repeat_byte(1);
    let misfiled = Address::repeat_byte(2);
    let shallow = Address::repeat_byte(3);
    let nan = Address::repeat_byte(4);
    let negative = Address::repeat_byte(5);
    let at_floor = Address::repeat_byte(6);
    let pools = [
        PoolSnapshot { code_hash: B256::ZERO, ..pool(no_code, Venue::UniswapV3, USDC, 5e6) },
        // Filed under Slipstream, deployed by Uniswap's factory.
        PoolSnapshot { factory: Venue::UniswapV3.factory(), ..pool(misfiled, Venue::Slipstream, USDC, 5e6) },
        pool(shallow, Venue::UniswapV3, USDC, 99_999.99),
        pool(nan, Venue::UniswapV3, USDC, f64::NAN),
        pool(negative, Venue::UniswapV3, USDC, -1e6),
        pool(at_floor, Venue::UniswapV3, USDC, 100_000.0),
    ];
    let (admitted, refused) = admission::admit_book(&book(pools), ChainId::BASE, READ_AT);

    assert_eq!(admitted.iter().map(|a| a.pool.address).collect::<Vec<_>>(), vec![at_floor]);
    let floor = 100_000 * 1_000_000;
    assert_eq!(
        refused,
        vec![
            (no_code, AdmissionError::NoBytecode { address: no_code }),
            (misfiled, AdmissionError::WrongFactory { claimed: Venue::Slipstream.id(), deployed_by: Venue::UniswapV3.factory() }),
            (shallow, AdmissionError::TooShallow { usd_micros: 99_999_000_000, floor_micros: floor }),
            (nan, AdmissionError::TooShallow { usd_micros: 0, floor_micros: floor }),
            (negative, AdmissionError::TooShallow { usd_micros: 0, floor_micros: floor }),
        ]
    );
}

fn through(pools: &[Address], block: u64) -> Candidate {
    let mut c = support::candidate(1, block, 1_000);
    let hop = c.route.hops[0].clone();
    c.route.hops = pools
        .iter()
        .map(|p| RouteHop { pool: PoolId { chain: c.chain_id, address: *p }, ..hop.clone() })
        .collect();
    c
}

fn auth() -> ExecutorAuth {
    let mut executor_version = [0u8; 32];
    executor_version[31] = 2;
    ExecutorAuth { chain: support::BASE, executor: support::EXECUTOR, executor_version }
}

/// **Evidence ages, and a reload makes it current** — without changing what a
/// route commits to, because the commitment drops the evidence's age.
#[test]
fn stale_evidence_is_refused_until_a_reload_replaces_it() {
    let commitments = LiveCommitments::new(admitted(universe(), READ_AT), 30, 10);
    assert_eq!(commitments.admitted(), 3);
    let route = [UNI, SLIP];

    let fresh = commitments.commit(&through(&route, READ_AT + 10), &auth(), U256::from(1)).unwrap();
    let stale = commitments.commit(&through(&route, READ_AT + 11), &auth(), U256::from(1));
    assert!(matches!(&stale, Err(Decline::VenueUnverified { detail }) if detail.contains("11 blocks ago")), "{stale:?}");

    commitments.set_admissions(admitted(universe(), READ_AT + 11));
    let reloaded = commitments.commit(&through(&route, READ_AT + 11), &auth(), U256::from(1)).unwrap();
    assert_eq!(reloaded.venue_fingerprints, fresh.venue_fingerprints);
    // The same policy over the new evidence: its slippage, and its bound.
    assert_eq!(reloaded.slippage_constraints, vec![30, 30]);
    assert!(commitments.commit(&through(&route, READ_AT + 22), &auth(), U256::from(1)).is_err());
}

/// A pool the reload did not admit — its code moved, say — is out of every
/// route through it, from that reload on.
#[test]
fn a_pool_the_last_reload_refused_is_unverified() {
    let commitments = LiveCommitments::new(admitted(universe(), READ_AT), 30, 10);
    let [uni, slip, bsd] = universe();
    let (a, refused) = admission::admit_book(
        &book([uni, PoolSnapshot { code_hash: B256::ZERO, ..slip }, bsd]),
        ChainId::BASE,
        READ_AT + 1,
    );
    assert_eq!(refused.len(), 1);
    commitments.set_admissions(a);
    assert_eq!(commitments.admitted(), 2);

    let got = commitments.commit(&through(&[UNI, SLIP], READ_AT + 1), &auth(), U256::from(1));
    assert!(matches!(&got, Err(Decline::VenueUnverified { .. })), "{got:?}");
    assert!(commitments.commit(&through(&[BSD, UNI], READ_AT + 1), &auth(), U256::from(1)).is_ok());
}
