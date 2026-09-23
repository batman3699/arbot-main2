//! Per-chain execution **behaviour** (Blueprint §4, §20, §21).
//!
//! `chain.rs` was a config struct. A config struct is not an adapter: chain
//! identity there is a `&str` matched in `main.rs`, and chain-specific
//! behaviour — fee model, inclusion probability, submission optimisation,
//! replacement policy, reconciliation — did not exist at all. This crate is
//! where it lives, and `scripts/ci/no_chain_string_matching.sh` keeps it here.

pub mod adapter;
pub mod base;
pub mod regime;

pub use adapter::{
    Ack, AdapterError, AdapterResult, ChainExecutionAdapter, PendingState, RejectReason,
    ReplacementPolicy, SignedPayload, StateFeedHandle, SubmissionDecision,
};
pub use base::adapter::{BaseAdapter, BaseRpc, FLASHBLOCK, GAS_HEADROOM_BPS};
pub use base::flashblock::{
    earliest_eligible, earliest_eligible_from, Capacity, FlashblockObservation,
    MeasuredCapacityModel, ModelError,
};
pub use base::observe::{Receipt, TransactionObservation};
pub use base::reconcile::{reconcile, BalanceDelta, ReconcileError, ReconcileInputs};
pub use base::submit::{
    blockpi_base_lane, choose_lane, send_redundantly, BaseTransactionStatus, EndpointKind, LaneRefusal,
    PrivacyEvidence, RedundancyOutcome, SubmissionLane,
};
pub use regime::{
    ChainRegime, FeeModel, NotDiscovered, OrderingMode, PriorityFeeSemantics, RegimeDiscovery,
    ReplacementRules,
};
