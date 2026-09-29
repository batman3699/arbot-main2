//! Venue code fingerprints (§25, §28's `contract code fingerprint change`).
//!
//! # What a venue fingerprint is a fingerprint *of*
//!
//! §25's commitment covers "chain_id, executor_version, **venue/version
//! fingerprints**, flash_source, …", and §28 lists **contract code fingerprint
//! change** among the risk triggers that tighten the gate. Together those say
//! what this is: a fingerprint of the **code the venue will execute**, so that a
//! trade committed against one version of a venue cannot be confused with the
//! same trade against a different one.
//!
//! The material is already collected and already verified. `PoolAdmission`
//! carries `BytecodeEvidence { extcodehash, observed_at_block }` and the factory
//! `factory()` reported, and `VenueRegistry::admit` refuses a pool whose factory
//! disagrees with its claimed venue. So a fingerprint is a fold over facts that
//! were checked against the chain rather than read from a file — which is the
//! whole of B-7's lesson, carried to the commitment layer.
//!
//! # Only an admitted pool can contribute, and that is the point
//!
//! [`fingerprints`] takes `&[PoolAdmission]`, the sealed type. A
//! `PoolAdmissionRecord` — the thing a file produces — cannot reach it. So the
//! fabricated router and quoter addresses in `base_venues_complete.yaml`, and
//! the 33 pools filed under Uniswap V3 that PancakeSwap had actually deployed,
//! are not "caught" here: they are **unable to arrive**.
//!
//! # `observed_at_block` is carried and NOT hashed
//!
//! Hashing the observation block would give unchanged code a different
//! fingerprint every block, so every commitment would differ, and §17.4's
//! deduplication — which is what the commitment hash is for — would never fire.
//! The block is carried alongside instead, as
//! [`VenueFingerprint::oldest_evidence_block`], because the age of the evidence
//! is a real question and a different one from what the evidence says.
//!
//! **"How old is too old" is a policy, not a constant**, so it is asked rather
//! than assumed: [`VenueFingerprint::is_fresh_at`] takes both the head and the
//! bound.
//!
//! # A venue with no admitted pool has no fingerprint
//!
//! Not `B256::ZERO`. Two venues with nothing admitted would share that hash, and
//! a commitment over an empty venue set would be indistinguishable from one over
//! a venue whose code happened to hash to zero. The same lesson as
//! `Recall::Undefined` and `CaptureAssurance::Undefined`: an absent measurement
//! is not a value.

use crate::admission::PoolAdmission;
use alloy_primitives::{keccak256, Address, B256};
use apex_types::ids::{PoolId, VenueId};
use std::collections::{BTreeMap, BTreeSet};

/// Domain separator, so a venue fingerprint cannot collide with any other
/// keccak in this system. Same discipline as `COMMITMENT_DOMAIN`.
pub const VENUE_FINGERPRINT_DOMAIN: &[u8] = b"apex.venue.fingerprint.v1";

/// A venue's code, as observed.
///
/// Construction is private: the only way to hold one is [`fingerprints`], which
/// only accepts admitted pools. Same witness pattern as `PoolAdmission`,
/// `DiscreteSize` and `VerifiedState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VenueFingerprint {
    hash: B256,
    oldest_evidence_block: u64,
    pools: usize,
    _sealed: (),
}

impl VenueFingerprint {
    /// What goes in the commitment.
    pub const fn hash(&self) -> B256 {
        self.hash
    }

    /// The **oldest** block any evidence in this fingerprint was read at.
    ///
    /// The minimum rather than the maximum, and rather than an average: a
    /// fingerprint is only as current as its stalest input, and reporting the
    /// newest would let one freshly-read pool vouch for nine nobody has looked
    /// at since deployment.
    pub const fn oldest_evidence_block(&self) -> u64 {
        self.oldest_evidence_block
    }

    /// How many admitted pools contributed.
    pub const fn pools(&self) -> usize {
        self.pools
    }

    /// Whether every piece of evidence is within `max_age_blocks` of `head`.
    ///
    /// Both arguments, because neither is this type's to decide. On Base a block
    /// is ~2 seconds, so a bound of 1,800 is an hour — but whether an hour is
    /// acceptable depends on the venue, and a constant here would be a policy
    /// nobody could change.
    ///
    /// Evidence from **after** `head` is fresh, not an error: a caller can hold a
    /// head that lags the reader.
    pub const fn is_fresh_at(&self, head: u64, max_age_blocks: u64) -> bool {
        head.saturating_sub(self.oldest_evidence_block) <= max_age_blocks
    }

    /// How far behind `head` the stalest evidence is.
    pub const fn age_at(&self, head: u64) -> u64 {
        head.saturating_sub(self.oldest_evidence_block)
    }
}

/// Why a route could not be fingerprinted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoFingerprint {
    /// The route names a pool nothing has admitted.
    ///
    /// **The refusal that matters.** A commitment over an unadmitted pool is a
    /// commitment to code nobody verified, and B-7 is what happens when an
    /// address is believed rather than checked.
    UnadmittedPool { pool: PoolId },
    /// The route names no pools at all, so there is nothing to fingerprint and
    /// no honest empty answer — an empty map would say "this route touches no
    /// venue", which no executable route does.
    EmptyRoute,
}

impl std::fmt::Display for NoFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnadmittedPool { pool } => {
                write!(f, "pool {} was never admitted", pool.address)
            }
            Self::EmptyRoute => f.write_str("a route with no pools touches no venue"),
        }
    }
}

impl std::error::Error for NoFingerprint {}

/// INV-40. A route that cannot be fingerprinted is an opportunity that goes
/// untaken, and it says which bucket.
///
/// `VenueDisabled` rather than `RiskFail`: an unadmitted pool means this venue
/// is not usable for this route, which is exactly what that bucket is for. It is
/// also the actionable reading — the response is to admit the pool, not to
/// loosen a gate.
impl apex_types::miss::ExplainsMiss for NoFingerprint {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        apex_types::miss::MissReason::VenueDisabled
    }
}

/// One fingerprint per venue represented in `admitted`.
///
/// # What is hashed, and in what order
///
/// ```text
/// DOMAIN || venue_id || factory || (pool_address || extcodehash)*
/// ```
///
/// Pools are sorted by address, and deduplicated, so the *set* decides the hash
/// rather than the order they arrived in.
///
/// **There is no length prefix, and the first draft had one.** A mutation
/// deleting it changed nothing, and the reason is that every element here is
/// fixed-width: the domain, the venue id and the factory are fixed, and each
/// pool contributes exactly 52 bytes (a 20-byte address and a 32-byte hash). So
/// the preimage parses uniquely and two different pool sets cannot produce the
/// same bytes.
///
/// `ExecutionCommitment::hash` does need its prefixes, and the difference is
/// worth naming rather than assuming: its `exact_inputs` and
/// `slippage_constraints` are *variable-length sequences*, where `[1, 2] ++ [3]`
/// and `[1] ++ [2, 3]` genuinely serialise identically. **Adding any
/// variable-length field per pool here — a hook payload, a tick list — makes the
/// prefix necessary again**, and that is the condition to check before doing it.
///
/// The **factory** is in because a venue whose factory changed is a different
/// venue — that is the fact `admit`'s `WrongFactory` check is about, and a
/// fingerprint that omitted it would call two of them the same.
///
/// `observed_at_block` is **not** in: see the module header.
pub fn fingerprints(admitted: &[PoolAdmission]) -> BTreeMap<VenueId, VenueFingerprint> {
    let mut by_venue: BTreeMap<VenueId, Vec<&PoolAdmission>> = BTreeMap::new();
    for a in admitted {
        by_venue.entry(a.venue).or_default().push(a);
    }

    by_venue
        .into_iter()
        .filter_map(|(venue, mut pools)| {
            // Sorted by address, and deduplicated: the same pool admitted twice
            // must not fingerprint differently from the same pool admitted once.
            pools.sort_by_key(|p| p.pool.address);
            let mut seen: BTreeSet<Address> = BTreeSet::new();
            pools.retain(|p| seen.insert(p.pool.address));

            let factory = pools.first()?.deployed_by;
            let oldest = pools.iter().map(|p| p.bytecode.observed_at_block).min()?;

            let mut buf = Vec::with_capacity(64 + pools.len() * 52);
            buf.extend_from_slice(VENUE_FINGERPRINT_DOMAIN);
            buf.extend_from_slice(&venue.0.to_be_bytes());
            buf.extend_from_slice(factory.as_slice());
            // No length prefix: every element below is fixed-width. See the
            // doc comment for what would make one necessary again.
            for p in &pools {
                buf.extend_from_slice(p.pool.address.as_slice());
                buf.extend_from_slice(p.bytecode.extcodehash.as_slice());
            }

            Some((
                venue,
                VenueFingerprint {
                    hash: keccak256(&buf),
                    oldest_evidence_block: oldest,
                    pools: pools.len(),
                    _sealed: (),
                },
            ))
        })
        .collect()
}

/// Fingerprints for exactly the venues a route touches.
///
/// **Refuses a route naming a pool nothing admitted.** That refusal is the
/// reason this function exists rather than callers filtering [`fingerprints`]
/// themselves: filtering silently drops an unknown pool, and a commitment that
/// silently omitted a venue would bind a trade to less code than it executes.
///
/// The fingerprint for a venue is computed over **only the route's pools on that
/// venue**, not over every pool the venue has. Two routes through different
/// pools of one venue are different trades and must not share a fingerprint —
/// and a venue-wide fingerprint would also change whenever any unrelated pool
/// was re-read, invalidating commitments for routes that never touched it.
pub fn for_route(
    admitted: &[PoolAdmission],
    route_pools: &[PoolId],
) -> Result<BTreeMap<VenueId, VenueFingerprint>, NoFingerprint> {
    if route_pools.is_empty() {
        return Err(NoFingerprint::EmptyRoute);
    }

    let index: BTreeMap<PoolId, &PoolAdmission> =
        admitted.iter().map(|a| (a.pool, a)).collect();

    let mut selected: Vec<PoolAdmission> = Vec::with_capacity(route_pools.len());
    for pool in route_pools {
        let Some(a) = index.get(pool) else {
            return Err(NoFingerprint::UnadmittedPool { pool: *pool });
        };
        selected.push((*a).clone());
    }

    Ok(fingerprints(&selected))
}

/// The commitment's shape: venue → hash, dropping the evidence age.
///
/// Separate from [`for_route`] so the age is *available* and taking it requires
/// saying so. A caller that wants only the hashes has usually already decided
/// staleness is somebody else's problem, and this is where that decision is
/// visible.
pub fn hashes(fingerprints: &BTreeMap<VenueId, VenueFingerprint>) -> BTreeMap<VenueId, B256> {
    fingerprints.iter().map(|(v, f)| (*v, f.hash())).collect()
}

/// The stalest venue in a set, and how stale.
///
/// Returns `None` for an empty set rather than a zero age: "nothing is stale"
/// and "there is nothing" are different facts, and a caller gating on age needs
/// to tell them apart.
pub fn stalest(
    fingerprints: &BTreeMap<VenueId, VenueFingerprint>,
    head: u64,
) -> Option<(VenueId, u64)> {
    fingerprints
        .iter()
        .map(|(v, f)| (*v, f.age_at(head)))
        .max_by_key(|(_, age)| *age)
}
