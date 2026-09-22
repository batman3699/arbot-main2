pub mod base_fast;
pub mod backrun_state;
pub mod cl_sim;
pub mod cl_parity_gate;
pub mod cl_ticks;
pub mod sim_revm;
pub mod abi;
pub mod abi_fixture;
pub mod capital;
pub mod chain;
pub mod continuity;
pub mod convex;
pub mod cycle_index;
pub mod discovery;
pub mod graph;
pub mod hot_path;
pub mod ingestion;
pub mod live_state;
pub mod log_decode;
pub mod metrics;
pub mod ops_inputs;
pub mod plan;
pub mod pool_store;
pub mod quote_balancer;
pub mod quote_cl;
pub mod quote_curve;
pub mod quote_univ2;
pub mod quote_slipstream;
pub mod quote_univ3;
pub mod quote_univ4;
pub mod reconcile;
pub mod registry;
pub mod state_gate;
pub mod state_validation;
pub mod util;
pub mod validation_select;
pub mod venues;

// Moved to `apex-math` in Phase 2 (PLAN.md §33 Phase 2, scope correction).
// Re-exported at the crate root so every `crate::math::...` / `crate::cl_swap::...`
// path in this crate keeps resolving: the move is a relocation, not a rename,
// and rewriting ~200 call sites would bury it in noise.
pub use apex_math::cl_math;
pub use apex_math::cl_swap;
pub use apex_math::math;
pub use apex_math::quote_common;
pub use apex_math::quote_solidly;
