//! Pool admissions from the live book, and the commitments built on them (Task
//! 8.5 R9).
//!
//! # Evidence from the book, and from nothing it did not read
//!
//! `VenueCommitments` fingerprints a route by `PoolAdmission`s, the sealed
//! evidence only `VenueRegistry::admit` produces. Each field here says where it
//! came from, and a field the book cannot vouch for says so rather than
//! borrowing a better-sounding value:
//!
//! | Field | From |
//! |---|---|
//! | bytecode | the code hash the book read, at the read's block |
//! | factory, tokens, decimals | the book's reads, already held to the inventory and the venue's factory |
//! | fee | the chain's fee for Uniswap (static); **dynamic** for Slipstream, whose module sets it |
//! | reconstruction | concentrated-liquidity logs — what the book does |
//! | depth | the inventory's figure, GeckoTerminal's: **non-authoritative**, it filters and never sizes |
//! | gas | a share of the R6 fork run's two-hop cycle, **not measured** per hop |
//! | reverts | none observed yet |
//! | transfers | `Standard` only for the tokens checked below; **`Unknown`** otherwise, which keeps a pool out of live dispatch while leaving it in the shadow run |
//! | update mapping | `Swap`, `Mint`, `Burn` |
//!
//! # Refreshed with the book
//!
//! `VenueCommitments` refuses evidence older than its bound, and a 14-day run
//! outlives any bound worth having. So [`LiveCommitments`] holds its admissions
//! behind a version the reload task replaces after each full reload, when the
//! book has just re-read every pool's code.

use crate::commit::VenueCommitments;
use crate::live::book::{PoolBook, PoolSnapshot};
use crate::live::feed::{BURN, MINT, SWAP};
use crate::live::frontier::WETH;
use crate::live::inventory::Venue;
use crate::plane::{Commitments, Decline};
use alloy_primitives::{address, Address, U256};
use apex_capture::signer::ExecutorAuth;
use apex_state::Versioned;
use apex_types::candidate::Candidate;
use apex_types::commitment::ExecutionCommitment;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, PoolId, TokenId};
use apex_types::state::ReconstructionStatus;
use apex_venues::admission::{
    AdmissionError, BytecodeEvidence, DepthEstimate, DepthPolicy, FeeBehavior, GasProfile, PoolAdmission,
    PoolAdmissionRecord, ReconstructionMethod, RevertProfile, TransferSemantics, VenueRegistry,
};

/// Circle's USDC on Base.
pub const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

/// Tokens whose transfers move exactly the amount stated: WETH9 (the
/// predeploy) and Circle's USDC, checked on Base 2026-10-01. The universe's
/// other WETH partner, bsdETH, is an upgradeable proxy nothing here has
/// classified, so it is `Unknown`.
pub const STANDARD_TOKENS: [Address; 2] = [WETH, USDC];

/// A share of the R6 fork run's 534,100 gas for a two-hop cycle with its flash
/// loan — half of it, per hop. A share, not a per-hop measurement, which is why
/// the profile says `measured: false`.
pub const GAS_PER_HOP: GasLimit = GasLimit(267_050);

/// The two venues the live book holds, each with the factory it deploys from.
pub fn registry() -> VenueRegistry {
    Venue::ALL
        .into_iter()
        .fold(VenueRegistry::new(DepthPolicy::default()), |r, v| r.with_factory(v.id(), v.factory()))
}

fn semantics(token: Address) -> TransferSemantics {
    if STANDARD_TOKENS.contains(&token) {
        TransferSemantics::Standard
    } else {
        TransferSemantics::Unknown
    }
}

/// One pool's record, as the book read it at `read_block`.
pub fn record(p: &PoolSnapshot, chain: ChainId, read_block: u64) -> PoolAdmissionRecord {
    let token = |address| TokenId { chain, address };
    PoolAdmissionRecord {
        pool: Some(PoolId { chain, address: p.spec.pool }),
        venue: Some(p.spec.venue.id()),
        bytecode: Some(BytecodeEvidence { extcodehash: p.code_hash, observed_at_block: read_block }),
        deployed_by: Some(p.factory),
        tokens: Some((token(p.spec.token0), token(p.spec.token1))),
        decimals: Some(p.decimals),
        fee_behavior: Some(match p.spec.venue {
            Venue::UniswapV3 => FeeBehavior::Static { ppm: p.state.fee_ppm },
            Venue::Slipstream => FeeBehavior::Dynamic,
        }),
        reconstruction: Some(ReconstructionMethod::ConcentratedLiquidityLogs),
        depth: Some(DepthEstimate::ExternalNonAuthoritative {
            // Whole dollars, in micros. `as` takes a NaN or a negative figure to
            // zero, which the registry's floor refuses.
            usd_micros: (p.spec.depth_usd as u128).saturating_mul(1_000_000),
        }),
        gas_profile: Some(GasProfile { per_hop: GAS_PER_HOP, measured: false }),
        revert_profile: Some(RevertProfile::default()),
        transfer_semantics: Some((semantics(p.spec.token0), semantics(p.spec.token1))),
        update_mapping: Some(vec![SWAP, MINT, BURN]),
    }
}

/// Admit every pool the book holds, as read at `read_block` — the block of a
/// full reload that has just succeeded, when every pool the book holds had its
/// code read at that block. Not a snapshot's own `block`, which a newer swap
/// moves. A refusal is reported with its reason, never dropped silently.
pub fn admit_book(
    book: &PoolBook,
    chain: ChainId,
    read_block: u64,
) -> (Vec<PoolAdmission>, Vec<(Address, AdmissionError)>) {
    let registry = registry();
    let (mut admitted, mut refused) = (Vec::new(), Vec::new());
    for p in book.snapshot().values() {
        match registry.admit(record(p, chain, read_block)) {
            Ok(a) => admitted.push(a),
            Err(e) => refused.push((p.spec.pool, e)),
        }
    }
    (admitted, refused)
}

/// `VenueCommitments`, over admissions the reload task replaces.
pub struct LiveCommitments {
    slippage_bps_per_hop: u32,
    max_evidence_age_blocks: u64,
    inner: Versioned<VenueCommitments>,
}

impl LiveCommitments {
    pub fn new(admitted: Vec<PoolAdmission>, slippage_bps_per_hop: u32, max_evidence_age_blocks: u64) -> Self {
        let inner = VenueCommitments::new(admitted, slippage_bps_per_hop, max_evidence_age_blocks);
        Self { slippage_bps_per_hop, max_evidence_age_blocks, inner: Versioned::new(inner, ReconstructionStatus::Verified) }
    }

    /// Replace the admissions, after a full reload has re-read every pool.
    pub fn set_admissions(&self, admitted: Vec<PoolAdmission>) {
        let next = VenueCommitments::new(admitted, self.slippage_bps_per_hop, self.max_evidence_age_blocks);
        self.inner.store(next, ReconstructionStatus::Verified);
    }

    pub fn admitted(&self) -> usize {
        self.inner.load().value.admitted().len()
    }
}

impl Commitments for LiveCommitments {
    fn commit(&self, c: &Candidate, auth: &ExecutorAuth, min_profit: U256) -> Result<ExecutionCommitment, Decline> {
        self.inner.load().value.commit(c, auth, min_profit)
    }
}
