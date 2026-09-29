//! `Commitments`, wired to verified venue code (Task 2b.7, continued).
//!
//! # The port the plane could not have
//!
//! Phase 2b delivered `apex-exec`'s commitment — the one the **contract**
//! recomputes — and left `Commitments` unimplemented for a stated reason:
//! `ExecutionCommitment::venue_fingerprints` comes from the venue adapters, and
//! `apex-venues` had no such concept. Inventing one would have put a fabricated
//! value inside the hash the system deduplicates on, which is B-7's failure in a
//! new place.
//!
//! `apex_venues::fingerprint` is that concept now, built over `PoolAdmission` —
//! the sealed type that only `VenueRegistry::admit` produces. So the material in
//! this commitment was checked against the chain rather than read from a file,
//! and a route through an unadmitted pool **cannot be committed**.
//!
//! # Two commitments, and this is the off-chain one
//!
//! `apex_exec::plan_commitment` is what the executor recomputes and reverts on.
//! This is §17.4's **deduplication key**, and it covers things the chain has no
//! way to check — the state the trade was priced against, and the code the venues
//! were carrying when it was priced. Whether the two should be one commitment is
//! recorded in PLAN.md as an open decision; they are not the same today and
//! pretending otherwise would put the wrong value in one of the two places.
//!
//! # Every narrowing is checked, not assumed
//!
//! Three fields arrive wider than the commitment holds them, and each is a place
//! a silent conversion would lose the distinction the field exists for. They are
//! refused rather than truncated — see [`narrow_executor_version`] and
//! [`deadline_seconds`].

use crate::plane::{Commitments, Decline};
use alloy_primitives::{Address, U256};
use apex_capture::signer::ExecutorAuth;
use apex_types::candidate::Candidate;
use apex_types::commitment::ExecutionCommitment;
use apex_types::ids::FlashProviderId;
use apex_venues::admission::PoolAdmission;
use apex_venues::fingerprint::{self, NoFingerprint};

/// `FlashProviderId(0)` is the sentinel for "this route borrows nothing".
///
/// `ExecutionCommitment::flash_source` is not an `Option`, so some value has to
/// mean absent. Zero is the convention the executor's own `PlanV2` uses for an
/// unstated field, and a route with no flash source is a real shape — the census
/// priced them at $300–$1,000 against inventory, not against a loan.
pub const NO_FLASH_SOURCE: FlashProviderId = FlashProviderId(0);

/// Builds §25's commitment from admitted venue code.
pub struct VenueCommitments {
    admitted: Vec<PoolAdmission>,
    slippage_bps_per_hop: u32,
    max_evidence_age_blocks: u64,
}

impl VenueCommitments {
    /// `slippage_bps_per_hop` is **policy and cannot be derived from a
    /// candidate**: the candidate carries its expected output, not the bound a
    /// caller is willing to accept on it. §25 wants a constraint per hop, so it
    /// is stated here where somebody owns it.
    ///
    /// `max_evidence_age_blocks` is the reason the fingerprint carries an age at
    /// all. A commitment built on code nobody has looked at in an hour is a
    /// commitment to code that may have changed — and "contract code fingerprint
    /// change" is on §28's risk-trigger list precisely because it happens.
    pub fn new(
        admitted: Vec<PoolAdmission>,
        slippage_bps_per_hop: u32,
        max_evidence_age_blocks: u64,
    ) -> Self {
        Self { admitted, slippage_bps_per_hop, max_evidence_age_blocks }
    }

    pub fn admitted(&self) -> &[PoolAdmission] {
        &self.admitted
    }
}

/// `ExecutorAuth` carries a 32-byte version; the commitment and
/// `LastMileContext` both hold a `u32`.
///
/// **Refused rather than truncated**, and the difference is the whole point of
/// having a version field: two executor versions differing only in their high
/// bytes would produce the same commitment, so a deployment change the field
/// exists to notice would be invisible. The repository's convention is a `u32`
/// stored in the low four bytes — this checks that rather than assuming it, so
/// the convention is enforced at the one place it is relied on.
///
/// Widening the commitment's field to 32 bytes would be the other repair, and it
/// is the better one; it changes `ExecutionCommitment` and `LastMileContext` and
/// is recorded in PLAN.md rather than done in passing.
pub fn narrow_executor_version(version: [u8; 32]) -> Result<u32, Decline> {
    if version[..28].iter().any(|b| *b != 0) {
        return Err(Decline::Uncommittable {
            detail: format!(
                "executor version 0x{} does not fit the commitment's u32; truncating it \
                 would make two deployments share a commitment",
                version.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ),
        });
    }
    let mut low = [0u8; 4];
    low.copy_from_slice(&version[28..]);
    Ok(u32::from_be_bytes(low))
}

/// Nanoseconds to the unix **seconds** the commitment holds, "matching the
/// on-chain `block.timestamp` comparison".
///
/// Truncating rather than rounding, and the direction is deliberate: rounding up
/// would give the trade a deadline later than the one it was priced against, and
/// a deadline that is too early costs an opportunity while one that is too late
/// executes a stale trade.
pub const fn deadline_seconds(deadline: apex_types::time::UnixNanos) -> u64 {
    deadline.0 / 1_000_000_000
}

impl Commitments for VenueCommitments {
    fn commit(
        &self,
        c: &Candidate,
        auth: &ExecutorAuth,
        min_profit: U256,
    ) -> Result<ExecutionCommitment, Decline> {
        let pools: Vec<apex_types::ids::PoolId> =
            c.route.hops.iter().map(|h| h.pool).collect();

        let fingerprints = fingerprint::for_route(&self.admitted, &pools).map_err(|e| {
            match e {
                // An unadmitted pool is a venue problem with an action attached:
                // verify the pool. Reported as such rather than as a generic
                // failure, because `VenueDisabled` is what tells an operator
                // where to look.
                NoFingerprint::UnadmittedPool { .. } => {
                    Decline::VenueUnverified { detail: e.to_string() }
                }
                NoFingerprint::EmptyRoute => {
                    Decline::Uncommittable { detail: e.to_string() }
                }
            }
        })?;

        // Staleness, against the block this candidate was priced at. Not against
        // a wall clock: the question is whether the code evidence is current
        // relative to the state the trade assumes, and those are the same clock.
        let head = c.state_fingerprint.confirmed_block_number;
        if let Some((venue, age)) = fingerprint::stalest(&fingerprints, head) {
            if age > self.max_evidence_age_blocks {
                return Err(Decline::VenueUnverified {
                    detail: format!(
                        "venue {} last had its code read {age} blocks ago, past the {} bound",
                        venue.0, self.max_evidence_age_blocks
                    ),
                });
            }
        }

        Ok(ExecutionCommitment {
            chain_id: c.chain_id,
            executor_address: Address::from(auth.executor),
            executor_version: narrow_executor_version(auth.executor_version)?,
            venue_fingerprints: fingerprint::hashes(&fingerprints),
            flash_source: c.flash_source.as_ref().map_or(NO_FLASH_SOURCE, |f| f.provider),
            state_fingerprint_hash: c.state_fingerprint.hash(),
            route_hash: c.route.route_hash,
            exact_inputs: vec![c.input_amount.get()],
            min_profit,
            // One bound per hop, from policy. A route with no hops was already
            // refused by `for_route`, so this is never the empty vector that
            // would say "no constraints" instead of "no route".
            slippage_constraints: vec![self.slippage_bps_per_hop; c.route.hops.len()],
            deadline: deadline_seconds(c.deadline),
            submission_policy: c.submission_policy,
        })
    }
}
