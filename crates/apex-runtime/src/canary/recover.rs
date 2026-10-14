//! Boot recovery for a journal that sent (spec §9, INV-39). Built before the
//! plane's boot from the ledger and the chain, and read synchronously by it.

use super::chain::{CanaryChain, Resolved};
use super::ledger::Halt;
use super::limits::Canary;
use apex_capture::recover::ChainOutcomeSource;
use apex_types::ids::TicketId;
use apex_types::sim::RevertClass;
use apex_types::ticket::{OpportunityTicket, TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

enum Found {
    Landed(Box<apex_types::pnl::PnlAttribution>),
    Reverted(RevertClass),
    Unresolved(String),
}

pub struct CanaryRecovery {
    found: BTreeMap<TicketId, Found>,
}

fn now() -> UnixNanos {
    UnixNanos(u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())).unwrap_or(u64::MAX))
}

impl CanaryRecovery {
    pub async fn build(canary: &Canary, chain: &CanaryChain) -> Self {
        let mut found = BTreeMap::new();
        for sent in canary.open_sends() {
            let f = match chain.resolve_open(&sent).await {
                Resolved::Landed(pnl) => Found::Landed(pnl),
                Resolved::Reverted(class) => Found::Reverted(class),
                Resolved::NotFound { nonce_consumed } => {
                    let _ = canary.halt(Halt::Unresolved { tx: sent.tx });
                    Found::Unresolved(if nonce_consumed {
                        format!("sent as {:#x}; its nonce was consumed by a transaction we cannot find", sent.tx)
                    } else {
                        format!("sent as {:#x}, unresolved at boot", sent.tx)
                    })
                }
            };
            found.insert(TicketId(sent.ticket), f);
        }
        Self { found }
    }
}

impl ChainOutcomeSource for CanaryRecovery {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        let failure = |code, cause: String| TicketOutcome::ExplicitFailure {
            code,
            at: now(),
            state: Box::new(t.state_fingerprint.clone()),
            cause,
        };
        Ok(match self.found.get(&t.ticket_id) {
            None => failure(
                TerminalFailure::Abandoned { at_status: t.status },
                "no send record: it never left the process".to_string(),
            ),
            Some(Found::Landed(pnl)) => TicketOutcome::Success { stage: TicketStatus::Reconciled, realized: pnl.clone() },
            Some(Found::Reverted(class)) => failure(
                TerminalFailure::Reverted { revert_class: *class, data: Vec::new() },
                "reverted on chain before the restart".to_string(),
            ),
            Some(Found::Unresolved(cause)) => failure(TerminalFailure::Abandoned { at_status: t.status }, cause.clone()),
        })
    }
}
