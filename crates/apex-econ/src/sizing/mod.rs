//! Trade sizing (§14.3, INV-18).
//!
//! Two stages, and the separation is the point:
//!
//! * [`continuous`] finds an approximate maximiser in real arithmetic. It is a
//!   **warm start** and nothing else — fast, differentiable, and wrong by
//!   construction, because no AMM accepts a fractional wei.
//! * [`discrete`] evaluates integer sizes exactly and returns the one that will
//!   actually be executed. It is the only thing that can mint a `DiscreteSize`.
//!
//! The order matters and the direction of trust matters more: the continuous
//! stage may suggest where to look, and may never decide.

pub mod continuous;
pub mod discrete;
