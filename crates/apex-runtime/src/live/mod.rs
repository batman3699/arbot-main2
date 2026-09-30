//! The live chain, as the plane reads it (Task 8.5, R2 onward).
//!
//! Everything here is wiring over crates that already exist: `apex-chain`'s
//! read-only transport and feed, `apex-venues`' ladder builder, `apex-math`'s
//! concentrated-liquidity maths and `apex-state`'s versioned snapshots. What this
//! module adds is the assembly — which pools, read how, kept current by what —
//! and the one rule each piece enforces, stated where it is enforced.

pub mod abi;
pub mod adapter;
pub mod admission;
pub mod book;
pub mod calls;
pub mod feed;
pub mod frontier;
pub mod inventory;
pub mod pricing;
pub mod reader;
pub mod reads;
pub mod sim;
