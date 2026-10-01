//! The chain, as a shadow run's recovery and settlement see it: nothing was
//! ever sent to it.
//!
//! **Recovery.** A shadow run keeps its own journal, and every ticket in it was
//! dispatched — if it got that far — to the null dispatcher, which records and
//! sends nothing. So a ticket a crash left open, even one past signing that a
//! live run would have to look up on chain, is closed as abandoned with that as
//! its cause: there is no transaction to find. `ShadowConfig` refuses a journal
//! not named as a shadow's, because this is true of no other.
//!
//! **Settlement.** A shadow ticket ends at the null dispatcher; the plane
//! never asks where its transaction is. Asking would be a defect, so it is
//! refused loudly and counted — the report carries the count, and it should
//! stay zero.

use crate::plane::{Decline, SettlementFeed};
use alloy_primitives::B256;
use apex_capture::clock::{Clock, SystemClock};
use apex_capture::recover::ChainOutcomeSource;
use apex_chain::base::observe::TransactionObservation;
use apex_types::ticket::{OpportunityTicket, TerminalFailure, TicketOutcome};
use std::sync::atomic::{AtomicU64, Ordering};

pub const NEVER_SENT: &str = "shadow run: the null dispatcher sends nothing, so no transaction exists";

#[derive(Debug, Default)]
pub struct NothingSent {
    asked: AtomicU64,
}

impl NothingSent {
    /// How often settlement was asked about a transaction. Zero, or a defect.
    pub fn asked(&self) -> u64 {
        self.asked.load(Ordering::Relaxed)
    }
}

impl ChainOutcomeSource for NothingSent {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Ok(TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: t.status },
            at: SystemClock.now(),
            state: Box::new(t.state_fingerprint.clone()),
            cause: NEVER_SENT.to_string(),
        })
    }
}

#[async_trait::async_trait]
impl SettlementFeed for NothingSent {
    async fn observe(&self, _tx: B256) -> Result<Vec<TransactionObservation>, Decline> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        Err(Decline::ChainUnavailable { detail: NEVER_SENT.to_string() })
    }
}
