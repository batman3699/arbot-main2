//! The one call a ticket makes (§25, INV-06).
//!
//! # Built once, handed to two consumers, unforgeable
//!
//! The plane builds an [`ExecutorCall`] once per ticket and hands the **same
//! value** to the simulator and the signer. So what is simulated and what is
//! signed cannot differ: there is one set of bytes, and both read it.
//!
//! It can only be made from a [`SignedPlan`] — a plan whose commitment was
//! recomputed and matched — and it encodes its own calldata from that plan. So
//! neither consumer can be handed bytes that are not the encoding of a checked
//! plan, and the target is the deployment the commitment was checked for rather
//! than an argument a caller could get wrong: the commitment covers
//! `address(this)` and `block.chainid`, so a plan sent anywhere else reverts.
//!
//! The fields are private, and the struct literal does not compile:
//!
//! ```compile_fail
//! use apex_exec::call::ExecutorCall;
//! let forged = ExecutorCall {
//!     to: todo!(),
//!     chain_id: todo!(),
//!     plan: todo!(),
//!     commitment: todo!(),
//!     data: todo!(),
//! };
//! ```
//!
//! The twin, differing only in going through the check:
//!
//! ```
//! use apex_exec::call::ExecutorCall;
//! use apex_exec::commitment::{plan_commitment, PlanV2};
//! use apex_exec::sign::SignedPlan;
//! use alloy_primitives::{Address, U256};
//! let plan = PlanV2 {
//!     loans: Vec::new(),
//!     cycle_slippage_bps: 30,
//!     steps: Vec::new(),
//!     min_profit: U256::from(1u64),
//!     declared_residue: U256::ZERO,
//!     chain_id: 8453,
//!     deadline: 1_781_049_614,
//! };
//! let declared = plan_commitment(&plan, 8453, Address::ZERO);
//! let call = ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, Address::ZERO).unwrap());
//! assert_eq!(call.commitment(), declared);
//! ```

use crate::commitment::PlanV2;
use crate::encode::start_v2_calldata;
use crate::sign::SignedPlan;
use alloy_primitives::{Address, B256};

/// `startV2(plan)` against one executor on one chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorCall {
    to: Address,
    chain_id: u64,
    plan: PlanV2,
    commitment: B256,
    data: Vec<u8>,
}

impl ExecutorCall {
    /// The only constructor. The calldata is encoded here, from the checked
    /// plan, and nowhere else.
    pub fn from_checked(checked: SignedPlan) -> Self {
        let data = start_v2_calldata(checked.plan(), checked.commitment());
        Self {
            to: checked.executor(),
            chain_id: checked.block_chain_id(),
            commitment: checked.commitment(),
            plan: checked.plan().clone(),
            data,
        }
    }

    /// The executor: the deployment the commitment was recomputed for.
    pub const fn to(&self) -> Address {
        self.to
    }

    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// `startV2`'s calldata. What is simulated and what is signed.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub const fn plan(&self) -> &PlanV2 {
        &self.plan
    }

    /// The value in `p.commitment`, which the contract recomputes and reverts on.
    pub const fn commitment(&self) -> B256 {
        self.commitment
    }
}
