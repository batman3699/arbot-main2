//! What the signer is handed (§25, INV-06).
//!
//! # This crate does not sign
//!
//! §7's responsibility list for `apex-exec` is "§25 deterministic
//! `ExecutionCommitment`, executor calldata encoding, §26.2 route-validator
//! input assembly, calldata-size optimisation". **Signing is not on it**, and
//! that is right: producing a signature needs key material, and §43 plus INV-46
//! govern how key material may be held. `apex-capture::signer` owns lanes,
//! nonces and per-lane health; the key itself is an operational input.
//!
//! So what lives here is the thing a signer is handed: a plan, the commitment
//! the executor will recompute, and the check that the two agree. Task 8.4's
//! port table said "`apex-exec` + key management" for `Signer`, and the second
//! half is the part still missing — recorded in PLAN.md rather than filled with a
//! fabricated signature, which would produce a process that looks able to trade
//! and cannot.

use crate::commitment::{plan_commitment, PlanV2};
use alloy_primitives::{Address, B256};

/// **INV-06's off-chain half.** The plan does not hash to the commitment it
/// carries.
///
/// The contract's own note is worth keeping in view: its check *"does not
/// constrain a malicious executor"*, because the plan and its commitment arrive
/// in the same calldata. This one does — a signer that refuses a payload whose
/// recomputed commitment differs from the ticket's is what stops a plan being
/// altered between the planner and the signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitmentMismatch {
    pub declared: B256,
    pub recomputed: B256,
}

impl std::fmt::Display for CommitmentMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plan declares {} but recomputes to {}", self.declared, self.recomputed)
    }
}

impl std::error::Error for CommitmentMismatch {}

/// A plan whose commitment has been verified against its contents.
///
/// **Unforgeable outside this module**, so a payload that skipped the check is
/// not a review finding — it does not exist. Same idiom as `Revalidated` in
/// `apex-capture`, and for the same reason.
///
/// ```compile_fail
/// use apex_exec::sign::SignedPlan;
/// let forged = SignedPlan { plan: todo!(), commitment: todo!(), _sealed: () };
/// ```
///
/// The twin, differing only in going through the check:
///
/// ```
/// use apex_exec::sign::SignedPlan;
/// use apex_exec::commitment::{plan_commitment, PlanV2};
/// use alloy_primitives::{Address, U256};
/// let plan = PlanV2 {
///     loans: Vec::new(),
///     cycle_slippage_bps: 30,
///     steps: Vec::new(),
///     min_profit: U256::from(1u64),
///     declared_residue: U256::ZERO,
///     chain_id: 8453,
///     deadline: 1_781_049_614,
/// };
/// let declared = plan_commitment(&plan, 8453, Address::ZERO);
/// let checked = SignedPlan::check(plan, declared, 8453, Address::ZERO)
///     .expect("the commitment matches");
/// assert_eq!(checked.commitment(), declared);
/// ```
#[derive(Debug)]
pub struct SignedPlan {
    plan: PlanV2,
    commitment: B256,
    _sealed: (),
}

impl SignedPlan {
    /// Recompute, compare, and refuse on a mismatch.
    pub fn check(
        plan: PlanV2,
        declared: B256,
        block_chain_id: u64,
        executor: Address,
    ) -> Result<Self, CommitmentMismatch> {
        let recomputed = plan_commitment(&plan, block_chain_id, executor);
        if recomputed != declared {
            return Err(CommitmentMismatch { declared, recomputed });
        }
        Ok(Self { plan, commitment: declared, _sealed: () })
    }

    pub const fn plan(&self) -> &PlanV2 {
        &self.plan
    }

    pub const fn commitment(&self) -> B256 {
        self.commitment
    }
}
