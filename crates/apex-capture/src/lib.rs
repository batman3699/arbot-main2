//! Capture Assurance Controller (Blueprint §2.4-2.9, §29.4-29.6, §46.1, §57).
//!
//! §16.1: this is a core dependency, not a later enhancement. It owns the
//! invariant that no admitted ticket can disappear silently (INV-01), and it is
//! built in shadow, before any live dispatch exists in the v4 path.

pub mod clock;
pub mod dispatch;
pub mod journal;
pub mod recover;
pub mod registry;
pub mod revalidate;
pub mod scheduler;
pub mod signer;
pub(crate) mod sync;

pub use clock::{Clock, ManualClock, SystemClock};
pub use journal::{FileJournal, InMemoryJournal, Journal, JournalEntry};
pub use dispatch::{
    AckError, AckLadder, DispatchError, DispatchRequest, Dispatcher, Escalation, LifecycleStage,
    NullDispatcher, WouldHaveSent,
};
pub use recover::{
    scan, ChainOutcomeSource, Disposition, DispatchGate, DispatchPermit, JournalScan,
    ReconciliationComplete, RecoveryError,
};
pub use revalidate::{
    last_mile, LastMileCheck, LastMileContext, Revalidated, RevalidationFailure, SigningAuthorization,
};
pub use registry::{RegistryError, RegistryMetrics, TicketGuard, TicketRegistry};
pub use scheduler::{Scheduler, Work, WorkClass};
pub use signer::{
    ExecutorAuth, LaneAssignment, LaneConfig, LaneHealth, LaneRequirements, NoLane, NonceError,
    NonceLane, ReservedNonce, SignerPool,
};
