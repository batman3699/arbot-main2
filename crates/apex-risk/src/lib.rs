//! Risk (Blueprint §28, §44). **Risk is a hard execution gate**, not advice.

pub mod breaker;
pub mod loss;
pub mod posture;

pub use breaker::{BreakerStatus, CircuitBreaker};
pub use loss::{ClassBudget, Containment, LossLedger};
pub use posture::{PostureLadder, RiskTrigger, StepDownAuthority, StepDownRefused};
