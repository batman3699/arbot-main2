//! Dispatch, and the shadow dispatcher Phase 6 runs on (§24.5).
//!
//! Phase 6 ships **no live dispatch**. [`NullDispatcher`] records what would
//! have been sent, which is what makes a shadow run a measurement rather than a
//! rehearsal: every ticket goes through the real ticket lifecycle, the real
//! revalidation and the real ack ladder, and the only thing that does not
//! happen is the transaction.

use crate::dispatch::ack::{AckLadder, LifecycleStage};
use crate::recover::DispatchPermit;
use crate::revalidate::SigningAuthorization;
use crate::sync::{recover, Mutex};
use apex_types::ids::{ChainId, SubmissionLaneId, TicketId};
use apex_types::ticket::SubmissionPolicy;
use apex_types::time::UnixNanos;

/// What a dispatcher is asked to send.
///
/// Holds a [`SigningAuthorization`], which cannot exist without a `Revalidated`
/// token -- so a dispatch that skipped last-mile revalidation is not a bug to
/// catch in review, it is a program that does not compile.
#[derive(Debug)]
pub struct DispatchRequest {
    authorization: SigningAuthorization,
    pub chain: ChainId,
    pub lane: SubmissionLaneId,
    pub policy: SubmissionPolicy,
    /// The exact signed bytes. INV-10: transport redundancy sends **these**,
    /// not a re-signed equivalent.
    pub signed: Vec<u8>,
}

impl DispatchRequest {
    pub const fn new(
        authorization: SigningAuthorization,
        chain: ChainId,
        lane: SubmissionLaneId,
        policy: SubmissionPolicy,
        signed: Vec<u8>,
    ) -> Self {
        Self { authorization, chain, lane, policy, signed }
    }

    pub const fn ticket(&self) -> TicketId {
        self.authorization.ticket()
    }

    pub const fn authorization(&self) -> &SigningAuthorization {
        &self.authorization
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// The endpoint refused it.
    Rejected { lane: SubmissionLaneId, detail: String },
    /// No answer within the transport timeout.
    NoResponse { lane: SubmissionLaneId },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { lane, detail } => write!(f, "lane {} rejected it: {detail}", lane.0),
            Self::NoResponse { lane } => write!(f, "lane {} did not answer", lane.0),
        }
    }
}

impl std::error::Error for DispatchError {}

pub trait Dispatcher {
    /// Send, and return a ladder carrying **only** what actually happened.
    ///
    /// The `DispatchPermit` argument is INV-39: dispatch is impossible until
    /// boot-time reconciliation produces its proof, and the type says so rather
    /// than a runtime check somewhere up the call stack.
    fn dispatch(
        &self,
        req: &DispatchRequest,
        permit: &DispatchPermit<'_>,
        now: UnixNanos,
    ) -> Result<AckLadder, DispatchError>;
}

/// What a dispatcher was asked to send, kept for the shadow run's ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WouldHaveSent {
    pub ticket: TicketId,
    pub chain: ChainId,
    pub lane: SubmissionLaneId,
    pub policy: SubmissionPolicy,
    pub nonce: u64,
    pub bytes: usize,
    pub at: UnixNanos,
}

/// §16.1's null dispatcher. Records; sends nothing.
#[derive(Debug, Default)]
pub struct NullDispatcher {
    sent: Mutex<Vec<WouldHaveSent>>,
}

impl NullDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn recorded(&self) -> Vec<WouldHaveSent> {
        recover(&self.sent).clone()
    }

    pub fn count(&self) -> usize {
        recover(&self.sent).len()
    }
}

impl Dispatcher for NullDispatcher {
    fn dispatch(
        &self,
        req: &DispatchRequest,
        _permit: &DispatchPermit<'_>,
        now: UnixNanos,
    ) -> Result<AckLadder, DispatchError> {
        recover(&self.sent).push(WouldHaveSent {
            ticket: req.ticket(),
            chain: req.chain,
            lane: req.lane,
            policy: req.policy,
            nonce: req.authorization().nonce().get(),
            bytes: req.signed.len(),
            at: now,
        });

        // `TransportAccepted`, and **only** that. A null dispatcher that
        // returned a ladder reaching `Included` would make every shadow run
        // report a 100% landing rate, which is the exact confusion INV-34
        // exists to prevent -- and the shadow numbers would then be used to
        // justify going live.
        let mut ladder = AckLadder::new(now);
        let _ = ladder.observe(LifecycleStage::TransportAccepted, now);
        Ok(ladder)
    }
}
