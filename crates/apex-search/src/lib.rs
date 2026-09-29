//! Candidate generation (Blueprint §12, §46.3).
//!
//! # What this crate may and may not produce
//!
//! It produces [`RouteProposal`](frontier::RouteProposal)s, **not
//! `Candidate`s**, and that is a consequence of §6.1 rather than a preference.
//! The dependency graph runs `apex-search → apex-econ`, and `Candidate` carries
//! `input_amount: DiscreteSize`, `total_execution_cost`, `robust_ev`,
//! `capture_probability`, `certificate_status` and `simulation_tier` — every one
//! of which is an `apex-econ` or `apex-sim` output. A search crate that could
//! fill them would be sizing and pricing, which is the god-object shape one
//! layer up.
//!
//! **This corrects Task 8.4.** Its `CandidateSource` port took a `StateEvent` and
//! returned `Vec<Candidate>`, and named `apex-search` as the crate that would
//! implement it — which `apex-search` could not, because only `apex-econ` can
//! mint a `DiscreteSize` (INV-18) and a `Candidate` cannot exist without one. The
//! port was named for a crate that could not satisfy it. Task 2b.5 renames it.
//!
//! A proposal may carry a `size_hint: Option<U256>` — Engine C searches *for* a
//! finite size, so it has one in mind. It is a `U256` and not a `DiscreteSize`
//! precisely because INV-18 says the executed size must come from
//! `apex-econ::sizing::discrete::refine`. The hint is an input to that search,
//! never a substitute for it, and the type is what says so.
//!
//! # What the measurements say this crate is for
//!
//! Five findings this repository recorded before v4 began:
//!
//! - **Opportunity density is not the constraint.** 4× the cycles produced the
//!   same 96% `no_profitable_size`.
//! - **More hops is strictly worse.** ~1,200 samples across 2/3/4-hop, none
//!   profitable, 2-hop dominant at every percentile.
//! - **The tradeable set is small and fee-selected.** Fee-aware pool selection
//!   cut the arbitrage hurdle from −247 bps to −10 bps median, a ~24× reduction,
//!   with no change to the search at all.
//! - **Event-triggered sampling found the first net-positive samples**, 88% of
//!   them following a swap.
//! - **The sweet spot is $300–$1,000 notional**: below it gas dominates, above it
//!   price impact does.
//!
//! So the frontier is a small resident set ranked **fee-first**, the engine that
//! matters is the event-driven one, and broad graph search is the fallback rather
//! than the product. §52 reaches the same conclusion from the other direction.

pub mod engine_c;
pub mod engine_d;
pub mod frontier;

pub use engine_c::{Declined, FiniteSizeEngine, Proposals, TemplatePricer};
pub use engine_d::{EventEngine, Response, Skipped, StaleAttribute, TemplateAction};
pub use frontier::{
    FeeVariant, Frontier, GasClass, InconsistentTemplate, ProposalOrigin, Revalued, RouteId,
    RouteProposal, RouteTemplate, TickNeighborhood,
};

/// Crate version, exposed so workspace wiring is testable.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
