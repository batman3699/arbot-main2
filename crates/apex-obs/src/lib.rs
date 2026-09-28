//! Observability (Blueprint §33, §27, §2.7). The measurement loop.

pub mod coverage;
pub mod miss;

pub use coverage::{
    AuditWindow, CoverageAuditor, CoverageReport, CoverageResponse, Discovery, OpportunityKey,
    Recall, RouteClass,
};
pub use miss::{Miss, MissContext, MissLedger};
