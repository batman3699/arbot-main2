//! The canary's `Sender` (spec §6): check the hold, write the send record,
//! send, record the answer.

use super::ledger::{Outcome, Sent, Settled};
use super::limits::Canary;
use crate::plane::{SendContext, Sender};
use crate::risk::Hold;
use alloy_primitives::{keccak256, B256};
use apex_capture::dispatch::{AckLadder, DispatchError, DispatchRequest};
use apex_capture::recover::DispatchPermit;
use apex_chain::base::send::{RawTransport, SendAnswer};
use apex_types::ack::LifecycleStage;
use apex_types::ids::SubmissionLaneId;
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The canary's one submission lane.
pub const LANE: SubmissionLaneId = SubmissionLaneId(1);

/// A send in flight: its record, and the receipt once settlement finds it.
#[derive(Clone, Debug)]
pub struct Pending {
    pub sent: Sent,
    pub receipt: Option<serde_json::Value>,
}

/// What the sender knows and settlement reads, by transaction hash.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<BTreeMap<B256, Pending>>);

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<B256, Pending>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn insert(&self, sent: Sent) {
        self.lock().insert(sent.tx, Pending { sent, receipt: None });
    }
    pub fn get(&self, tx: &B256) -> Option<Pending> {
        self.lock().get(tx).cloned()
    }
    pub fn set_receipt(&self, tx: &B256, receipt: serde_json::Value) {
        if let Some(p) = self.lock().get_mut(tx) {
            p.receipt = Some(receipt);
        }
    }
    pub fn remove(&self, tx: &B256) {
        self.lock().remove(tx);
    }
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

pub struct CanaryDispatcher {
    canary: Arc<Canary>,
    transport: Arc<dyn RawTransport>,
    in_flight: Arc<InFlight>,
}

impl CanaryDispatcher {
    pub fn new(canary: Arc<Canary>, transport: Arc<dyn RawTransport>, in_flight: Arc<InFlight>) -> Self {
        Self { canary, transport, in_flight }
    }

    /// The send, without the plane's types: what the fork proof drives.
    pub async fn send_raw(&self, ticket: u64, nonce: u64, raw: &[u8], ctx: &SendContext) -> Result<B256, DispatchError> {
        let rejected = |detail: String| DispatchError::Rejected { lane: LANE, detail };
        // A hold that appeared after the gate: refused here, as a hold and not
        // as an endpoint's refusal, so nothing is recorded.
        if let Some(reason) = self.canary.reason() {
            return Err(rejected(reason));
        }
        let tx = keccak256(raw);
        let sent = Sent {
            ticket,
            nonce,
            tx,
            at_ms: now_ms(),
            chain: ctx.chain,
            strategy: ctx.strategy,
            route_hash: ctx.route_hash,
            venues: ctx.venues.clone(),
            gas_limit: ctx.gas_limit.0,
            max_fee_per_gas: ctx.fees.max_fee_per_gas,
            max_priority_fee_per_gas: ctx.fees.max_priority_fee_per_gas,
            expected_gross_wei: ctx.expected_gross_wei,
            expected_net_wei: ctx.expected_net_wei,
            waived: ctx.waived.clone(),
        };
        // Write-ahead: on disk before a byte leaves.
        self.canary.sent(sent.clone()).map_err(|e| rejected(format!("the canary ledger: {e}")))?;
        self.in_flight.insert(sent);
        let settled = |settled: Settled| Outcome { tx, at_ms: now_ms(), settled };
        match self.transport.send_raw(raw).await {
            SendAnswer::Accepted(h) if h == tx => Ok(tx),
            // A success naming another transaction is not ours being accepted:
            // something may have been sent, and it was not what we signed.
            SendAnswer::Accepted(h) => {
                let detail = format!("the endpoint named {h:#x}, and ours is {tx:#x}");
                let _ = self.canary.settle(settled(Settled::Silent { detail }));
                Err(DispatchError::NoResponse { lane: LANE })
            }
            SendAnswer::Refused(detail) => {
                self.in_flight.remove(&tx);
                let _ = self.canary.settle(settled(Settled::Refused { detail: detail.clone() }));
                Err(rejected(detail))
            }
            SendAnswer::Silent(detail) => {
                let _ = self.canary.settle(settled(Settled::Silent { detail }));
                Err(DispatchError::NoResponse { lane: LANE })
            }
        }
    }
}

#[async_trait::async_trait]
impl Sender for CanaryDispatcher {
    async fn send(
        &self,
        req: &DispatchRequest,
        _permit: &DispatchPermit<'_>,
        now: UnixNanos,
        context: &SendContext,
    ) -> Result<AckLadder, DispatchError> {
        self.send_raw(req.ticket().0, req.authorization().nonce().get(), &req.signed, context).await?;
        let mut ladder = AckLadder::new(now);
        let _ = ladder.observe(LifecycleStage::TransportAccepted, now);
        Ok(ladder)
    }
}
