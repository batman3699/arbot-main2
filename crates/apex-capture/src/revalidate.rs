//! Last-mile revalidation (§24.6, §16.2 step 4). **INV-35.**
//!
//! Step 4 of the mandatory capture protocol, and the step immediately before
//! signing. Its output is a [`Revalidated`] token, and a [`SigningAuthorization`]
//! cannot be built without one -- so "revalidate before you sign" is enforced by
//! the type system rather than by review.
//!
//! # Only critical mutable state
//!
//! §16.2 step 4 says "REVALIDATE **only critical** mutable state", and the
//! reason is the budget: `T_sign` p99 must stay under 3 ms *including* this
//! (§21's latency table). Re-reading everything would make revalidation the
//! thing that misses the block. So the pool-state check compares only the
//! venues the route actually touches, and the eleven checks are ordered
//! cheapest-first.
//!
//! # First failure, not all failures
//!
//! [`last_mile`] returns the first check that rejects. The purpose is to *not
//! sign*, not to produce a diagnosis, and running ten more comparisons after
//! the answer is known spends the budget the ordering exists to protect. The
//! rejection histogram therefore counts first-failures -- which is the right
//! denominator anyway, since it answers "why did we not sign".
//!
//! # What rejection means
//!
//! Re-simulate or invalidate. **Never blind dispatch** (INV-35's remediation
//! column). Nothing here returns a token on a failed check by any path.

use crate::signer::ReservedNonce;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SignerLaneId, TicketId, VenueId};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

/// §24.6's list, as INV-35 enumerates it. Ordered cheapest-first, which is the
/// order [`last_mile`] runs them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LastMileCheck {
    ChainId,
    ExecutorFingerprint,
    NonceOwnership,
    Deadline,
    MinProfit,
    FeeCeiling,
    GasEligibility,
    SignerBalance,
    FlashAvailability,
    HookFingerprint,
    CriticalPoolState,
}

impl LastMileCheck {
    /// All eleven. A `const` array rather than a derive, because the test that
    /// proves each one independently gates iterates this -- so a check added to
    /// the enum and forgotten here would leave a gap the table test cannot see.
    /// `the_check_list_is_complete` closes that by counting variants.
    pub const ALL: [Self; 11] = [
        Self::ChainId,
        Self::ExecutorFingerprint,
        Self::NonceOwnership,
        Self::Deadline,
        Self::MinProfit,
        Self::FeeCeiling,
        Self::GasEligibility,
        Self::SignerBalance,
        Self::FlashAvailability,
        Self::HookFingerprint,
        Self::CriticalPoolState,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::ChainId => "chain_id",
            Self::ExecutorFingerprint => "executor_fingerprint",
            Self::NonceOwnership => "nonce_ownership",
            Self::Deadline => "deadline",
            Self::MinProfit => "min_profit",
            Self::FeeCeiling => "fee_ceiling",
            Self::GasEligibility => "gas_eligibility",
            Self::SignerBalance => "signer_balance",
            Self::FlashAvailability => "flash_availability",
            Self::HookFingerprint => "hook_fingerprint",
            Self::CriticalPoolState => "critical_pool_state",
        }
    }
}

impl std::fmt::Display for LastMileCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Everything the eleven checks read. Both sides of every comparison are here
/// explicitly -- committed versus live -- because a check that reads one side
/// from ambient state is a check whose answer depends on when it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct LastMileContext {
    pub ticket: TicketId,

    /// 1. Chain id. INV-05: `wrong_chain_submission == 0`.
    pub committed_chain: ChainId,
    pub signer_chain: ChainId,

    /// 2. Executor fingerprint (§26.2): address and version together.
    pub committed_executor: [u8; 20],
    pub committed_executor_version: u32,
    pub live_executor: [u8; 20],
    pub live_executor_version: u32,

    /// 3. Nonce ownership: the reservation must belong to the assigned lane.
    pub nonce: ReservedNonce,
    pub assigned_lane: SignerLaneId,

    /// 4. Deadline (§17.3).
    pub now: UnixNanos,
    pub dispatch_deadline: UnixNanos,

    /// 5. Minimum profit, in units of the profit token (§2.10).
    pub expected_net_profit: i128,
    pub min_profit: i128,

    /// 6. Fee ceiling, wei.
    pub observed_fee_wei: u128,
    pub fee_ceiling_wei: u128,

    /// 7. Gas eligibility (§22.2): does the transaction still fit?
    pub required_gas: GasLimit,
    pub remaining_block_gas: u64,

    /// 8. Signer balance against the lane's required reserve.
    pub signer_balance_wei: u128,
    pub required_reserve_wei: u128,

    /// 9. Flash liquidity still available at the committed source.
    pub flash_required: u128,
    pub flash_available: u128,

    /// 10. Hook fingerprint (§21.5). `None` on both sides means no hooks.
    pub committed_hooks: Option<[u8; 32]>,
    pub live_hooks: Option<[u8; 32]>,

    /// 11. Critical pool state: only the venues the route touches.
    pub committed_venue_versions: BTreeMap<VenueId, u64>,
    pub live_venue_versions: BTreeMap<VenueId, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevalidationFailure {
    pub check: LastMileCheck,
    pub ticket: TicketId,
    pub detail: String,
}

impl std::fmt::Display for RevalidationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ticket {}: {} rejected: {}", self.ticket.0, self.check, self.detail)
    }
}

impl std::error::Error for RevalidationFailure {}

/// Proof that [`last_mile`] passed. **Unforgeable outside this module**, and
/// consumed by [`SigningAuthorization::new`], which is the only way to reach a
/// signer.
///
/// ```compile_fail
/// use apex_capture::revalidate::Revalidated;
/// let forged = Revalidated { _sealed: () };
/// ```
///
/// The twin, differing only in going through the check:
///
/// ```
/// use apex_capture::revalidate::{last_mile, SigningAuthorization};
/// let ctx = apex_capture::revalidate::doc_fixture();
/// let proof = last_mile(&ctx).expect("a passing context");
/// let auth = SigningAuthorization::new(ctx.ticket, ctx.nonce, proof);
/// assert_eq!(auth.ticket(), ctx.ticket);
/// ```
///
/// And it cannot be defaulted into existence either:
///
/// ```compile_fail
/// use apex_capture::revalidate::Revalidated;
/// let forged = Revalidated::default();
/// ```
#[derive(Debug)]
pub struct Revalidated {
    _sealed: (),
}

/// The only thing a signer accepts. Holding one means revalidation passed,
/// because there is no other way to build it.
///
/// Step 5 of §16.2 requires a token produced only by step 4; this is that
/// requirement, spelled as a type rather than as a comment on a function.
///
/// ```compile_fail
/// use apex_capture::revalidate::SigningAuthorization;
/// use apex_types::ids::TicketId;
/// let ctx = apex_capture::revalidate::doc_fixture();
/// // No `Revalidated` to pass: signing without revalidating does not compile.
/// let auth = SigningAuthorization::new(TicketId(1), ctx.nonce);
/// ```
#[derive(Debug)]
pub struct SigningAuthorization {
    ticket: TicketId,
    nonce: ReservedNonce,
}

impl SigningAuthorization {
    pub const fn new(ticket: TicketId, nonce: ReservedNonce, _proof: Revalidated) -> Self {
        Self { ticket, nonce }
    }
    pub const fn ticket(&self) -> TicketId {
        self.ticket
    }
    pub const fn nonce(&self) -> ReservedNonce {
        self.nonce
    }
}

/// §24.6, in cheapest-first order. The first rejection wins.
pub fn last_mile(ctx: &LastMileContext) -> Result<Revalidated, RevalidationFailure> {
    let fail = |check: LastMileCheck, detail: String| RevalidationFailure {
        check,
        ticket: ctx.ticket,
        detail,
    };

    if ctx.committed_chain != ctx.signer_chain {
        return Err(fail(
            LastMileCheck::ChainId,
            format!("committed to {} but signing on {}", ctx.committed_chain.0, ctx.signer_chain.0),
        ));
    }
    if ctx.committed_executor != ctx.live_executor
        || ctx.committed_executor_version != ctx.live_executor_version
    {
        return Err(fail(
            LastMileCheck::ExecutorFingerprint,
            format!(
                "executor moved from v{} to v{}",
                ctx.committed_executor_version, ctx.live_executor_version
            ),
        ));
    }
    if ctx.nonce.lane() != ctx.assigned_lane {
        return Err(fail(
            LastMileCheck::NonceOwnership,
            format!(
                "nonce belongs to lane {} but lane {} is assigned",
                ctx.nonce.lane().0,
                ctx.assigned_lane.0
            ),
        ));
    }
    // `>=`, not `>`: at the deadline the ticket is already late. §17.3 wants
    // expiry explained before it happens, not observed after.
    if ctx.now.0 >= ctx.dispatch_deadline.0 {
        return Err(fail(
            LastMileCheck::Deadline,
            format!("{} ns past the dispatch deadline", ctx.now.0 - ctx.dispatch_deadline.0),
        ));
    }
    if ctx.expected_net_profit < ctx.min_profit {
        return Err(fail(
            LastMileCheck::MinProfit,
            format!("{} below the floor of {}", ctx.expected_net_profit, ctx.min_profit),
        ));
    }
    if ctx.observed_fee_wei > ctx.fee_ceiling_wei {
        return Err(fail(
            LastMileCheck::FeeCeiling,
            format!("fee {} above the ceiling {}", ctx.observed_fee_wei, ctx.fee_ceiling_wei),
        ));
    }
    if ctx.required_gas.0 > ctx.remaining_block_gas {
        return Err(fail(
            LastMileCheck::GasEligibility,
            format!(
                "needs {} gas, {} remains in the target block",
                ctx.required_gas.0, ctx.remaining_block_gas
            ),
        ));
    }
    if ctx.signer_balance_wei < ctx.required_reserve_wei {
        return Err(fail(
            LastMileCheck::SignerBalance,
            format!(
                "balance {} below the required reserve {}",
                ctx.signer_balance_wei, ctx.required_reserve_wei
            ),
        ));
    }
    if ctx.flash_available < ctx.flash_required {
        return Err(fail(
            LastMileCheck::FlashAvailability,
            format!("needs {}, source holds {}", ctx.flash_required, ctx.flash_available),
        ));
    }
    if ctx.committed_hooks != ctx.live_hooks {
        return Err(fail(
            LastMileCheck::HookFingerprint,
            "a hook changed between commitment and signing".to_string(),
        ));
    }
    // Only the venues the route touches -- §16.2 step 4's "only critical
    // mutable state". A venue absent from the live map is a change too: it
    // means the state source no longer carries it, which is not the same as
    // "unchanged" (§5.6 forbids that conversion).
    for (venue, committed) in &ctx.committed_venue_versions {
        match ctx.live_venue_versions.get(venue) {
            Some(live) if live == committed => {}
            Some(live) => {
                return Err(fail(
                    LastMileCheck::CriticalPoolState,
                    format!("venue {} moved from version {} to {}", venue.0, committed, live),
                ))
            }
            None => {
                return Err(fail(
                    LastMileCheck::CriticalPoolState,
                    format!("venue {} is no longer in the live state", venue.0),
                ))
            }
        }
    }

    Ok(Revalidated { _sealed: () })
}

/// A passing context, for the doctests above. Public because a doctest is
/// compiled as an external crate and cannot reach a test module.
#[doc(hidden)]
pub fn doc_fixture() -> LastMileContext {
    LastMileContext {
        ticket: TicketId(1),
        committed_chain: ChainId::BASE,
        signer_chain: ChainId::BASE,
        committed_executor: [0xEE; 20],
        committed_executor_version: 2,
        live_executor: [0xEE; 20],
        live_executor_version: 2,
        nonce: crate::signer::NonceLane::new(SignerLaneId(0)).reserve(5, UnixNanos(0)),
        assigned_lane: SignerLaneId(0),
        now: UnixNanos(1_000),
        dispatch_deadline: UnixNanos(2_000),
        expected_net_profit: 5_000,
        min_profit: 1_000,
        observed_fee_wei: 10,
        fee_ceiling_wei: 100,
        required_gas: GasLimit(500_000),
        remaining_block_gas: 30_000_000,
        signer_balance_wei: 1_000_000_000_000_000_000,
        required_reserve_wei: 100_000_000_000_000_000,
        flash_required: 10,
        flash_available: 1_000,
        committed_hooks: None,
        live_hooks: None,
        committed_venue_versions: BTreeMap::from([(VenueId(1), 7), (VenueId(2), 9)]),
        live_venue_versions: BTreeMap::from([(VenueId(1), 7), (VenueId(2), 9)]),
    }
}
