//! Dispatch (§24.5, §24.8). INV-34, INV-10.

pub mod ack;
pub mod router;

pub use ack::{AckError, AckLadder, Escalation, LifecycleStage};
pub use router::{
    DispatchError, DispatchRequest, Dispatcher, NullDispatcher, WouldHaveSent,
};
