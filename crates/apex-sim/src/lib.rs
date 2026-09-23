//! APEX-MEV v4 simulation hierarchy (§20, §15).
//!
//! Three tiers, escalating in cost and in fidelity:
//!
//! | Tier | What it is | Budget |
//! |---|---|---|
//! | 0 | Analytic screening. Pure arithmetic, no node. | 50 µs |
//! | 1 | Local EVM against a cached state. | 5 ms |
//! | 2 | Full node simulation — `eth_simulateV1` on Base. | 60 ms p99 |
//!
//! # The ladder is one-directional, and that is the whole safety argument
//!
//! A lower tier may only **reject**. It may never admit something a higher
//! tier would have caught, because nothing runs after it to catch anything.
//! So Tier 0's errors are allowed in one direction: a candidate it escalates
//! may still fail at Tier 2, but a candidate it rejects must be one no tier
//! could have saved. Every comparison it makes is against the *conservative*
//! total, and every rounding goes against the trade.
//!
//! §20 also states the boundary the other way: Tier 4 (canary) is *"never a
//! latency technique, and never a substitute for simulation"*. A tier is not a
//! faster way to get the same answer; it is a different answer with a
//! different confidence, and `SimulationResult::tier` records which one was
//! actually asked.

pub mod backends;
pub mod tier0;

/// Crate version, exposed so workspace wiring is testable before a backend
/// exists.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
