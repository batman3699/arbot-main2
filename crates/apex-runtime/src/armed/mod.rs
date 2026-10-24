//! Armed backruns (Task 8.5 R26): transactions that wait in Base's builder
//! for a gap's condition (validity transactions). In armed-shadow they are
//! built and recorded, never sent; `apex armed-eval` judges them against the
//! chain.

pub mod bid;
pub mod engine;
pub mod eval;
pub mod keys;
pub mod lines;
pub mod live;
pub mod live_chain;
pub mod live_ledger;
pub mod moves;
pub mod plan;
pub mod predicate;
pub mod run;
pub mod targets;
pub mod threshold;
