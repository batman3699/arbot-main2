//! Outcome observation (§21.5, §24.8). **INV-34.**
//!
//! `observe_outcome` must distinguish **preconfirmed**, **included** and
//! **finalized**, which are three different facts about money:
//!
//! - *Preconfirmed*: the sequencer has committed to including it. Good enough
//!   to stop competing for the opportunity; not good enough to book a profit.
//! - *Included*: it is in an L2 block. The trade happened.
//! - *Finalized*: the L1 batch containing that block is finalized. Only now is
//!   it beyond a reorg.
//!
//! Collapsing any pair of these is how a P&L ledger books a trade that later
//! un-happens.
//!
//! §20's trait returns `apex_types::miss::ObservedOutcome`, which is
//! `{ landed_by_competitor, realized_profit_estimate }` — the **miss ledger's**
//! question, "did somebody else take it?". That cannot answer "where is our
//! transaction?", so this module's [`TransactionObservation`] is what the
//! adapter returns and the deviation is recorded in PLAN.md Task 7.5.

use alloy_primitives::B256;
use apex_types::ack::LifecycleStage;
use apex_types::time::UnixNanos;
use serde::{Deserialize, Serialize};

/// What the chain reported about a transaction that exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub tx: B256,
    pub block_number: u64,
    /// `status == 1`.
    pub success: bool,
    pub gas_used: u64,
    pub effective_gas_price_wei: u128,
    /// OP Stack: the L1 data fee the chain actually charged. **Read, not
    /// re-derived** — reconciliation records what happened, and re-computing it
    /// here would report the estimator's opinion as the realized cost.
    pub l1_fee_wei: u128,
}

impl Receipt {
    /// What the L2 execution actually cost.
    pub const fn l2_execution_fee_wei(&self) -> u128 {
        (self.gas_used as u128).saturating_mul(self.effective_gas_price_wei)
    }

    /// Everything the chain charged.
    pub const fn total_chain_fee_wei(&self) -> u128 {
        self.l2_execution_fee_wei().saturating_add(self.l1_fee_wei)
    }
}

/// Where a transaction is, and what is known about it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionObservation {
    pub tx: B256,
    pub stage: LifecycleStage,
    pub observed_at: UnixNanos,
    /// Present from `Included` onward. A preconfirmation has no receipt, which
    /// is the mechanical reason the two cannot be conflated.
    pub receipt: Option<Receipt>,
}

impl TransactionObservation {
    pub const fn preconfirmed(tx: B256, at: UnixNanos) -> Self {
        Self { tx, stage: LifecycleStage::Preconfirmed, observed_at: at, receipt: None }
    }

    pub const fn included(receipt: Receipt, at: UnixNanos) -> Self {
        Self {
            tx: receipt.tx,
            stage: LifecycleStage::Included,
            observed_at: at,
            receipt: Some(receipt),
        }
    }

    /// Finalization is a fact about the **L1 batch**, not about the L2 block, so
    /// it is supplied rather than derived: an L2 receipt cannot tell you
    /// whether the batch containing it has finalized.
    pub const fn finalized(receipt: Receipt, at: UnixNanos) -> Self {
        Self {
            tx: receipt.tx,
            stage: LifecycleStage::Finalized,
            observed_at: at,
            receipt: Some(receipt),
        }
    }

    /// **Safe to book a profit against.** Only `Finalized` -- an included
    /// transaction can still be reorganized out, and a ledger that books at
    /// inclusion records trades that later un-happen.
    pub const fn is_settled(&self) -> bool {
        matches!(self.stage, LifecycleStage::Finalized)
    }

    /// The trade happened, whether or not it is beyond a reorg. Enough to stop
    /// competing; not enough to book.
    pub const fn has_executed(&self) -> bool {
        self.stage.implies_inclusion()
    }
}
