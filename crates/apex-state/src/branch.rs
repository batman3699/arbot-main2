//! Speculative state branches and rollback (Blueprint §5.1, §5.4).
//!
//! ```text
//! CanonicalState (branch 0)
//!    ├── ConfirmedState
//!    └── SpeculativeStateTree
//!           ├── Flashblock / pending branch 0..N
//!           ├── Target-event branch
//!           └── Candidate-local execution branch
//! ```
//!
//! The load-bearing part is not opening branches, it is closing them. §5.4
//! requires that when the final block differs from the speculation, the affected
//! branch is rolled back AND the impacted candidates are re-evaluated. A
//! rollback that does not name its candidates leaves them referencing state that
//! no longer exists -- a candidate priced on a branch that lost.

use apex_types::ids::CandidateId;
use apex_types::state::StateBranchId;

/// What should happen to one branch given the confirmed head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchOutcome {
    Promote,
    Rollback { expected: String, actual: String },
}

/// The result of closing out a confirmed block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitOutcome {
    pub promoted: Option<StateBranchId>,
    pub rolled_back: Vec<StateBranchId>,
    /// Every candidate that was priced against a branch that lost. §5.4's
    /// "re-evaluate impacted candidates" needs this list to exist.
    pub invalidated_candidates: Vec<CandidateId>,
}

#[derive(Debug)]
struct Branch {
    id: StateBranchId,
    /// Opaque head identity -- a block hash in production. Compared, never
    /// interpreted, so this crate stays free of chain primitives.
    head_tag: String,
    candidates: Vec<CandidateId>,
}

#[derive(Debug, Default)]
pub struct SpeculativeStateTree {
    branches: Vec<Branch>,
    next_id: u32,
    commits: u64,
    reorgs: u64,
}

impl SpeculativeStateTree {
    pub fn new() -> Self {
        Self { branches: Vec::new(), next_id: 1, commits: 0, reorgs: 0 }
    }

    pub fn open_branch(&mut self, head_tag: impl Into<String>) -> StateBranchId {
        let id = StateBranchId(self.next_id);
        self.next_id += 1;
        self.branches.push(Branch { id, head_tag: head_tag.into(), candidates: Vec::new() });
        id
    }

    /// Record that a candidate was priced against this branch.
    pub fn attach_candidate(&mut self, branch: StateBranchId, candidate: CandidateId) {
        if let Some(b) = self.branches.iter_mut().find(|b| b.id == branch) {
            b.candidates.push(candidate);
        }
    }

    pub fn classify(&self, branch: StateBranchId, confirmed_head: &str) -> BranchOutcome {
        match self.branches.iter().find(|b| b.id == branch) {
            Some(b) if b.head_tag == confirmed_head => BranchOutcome::Promote,
            Some(b) => BranchOutcome::Rollback {
                expected: b.head_tag.clone(),
                actual: confirmed_head.to_string(),
            },
            None => BranchOutcome::Rollback {
                expected: "<unknown branch>".to_string(),
                actual: confirmed_head.to_string(),
            },
        }
    }

    /// Close out a confirmed block: promote the branch that predicted it, roll
    /// back the rest, and report which candidates that invalidated.
    ///
    /// A commit with no open branches is NOT a reorg. Quiet blocks are the
    /// common case, and counting them would inflate `divergence_rate` and
    /// trigger §5.4's response for no reason.
    pub fn commit_or_rollback(&mut self, confirmed_head: &str) -> CommitOutcome {
        self.commits += 1;

        let mut out = CommitOutcome::default();
        for b in self.branches.drain(..) {
            if b.head_tag == confirmed_head && out.promoted.is_none() {
                out.promoted = Some(b.id);
            } else {
                out.rolled_back.push(b.id);
                out.invalidated_candidates.extend(b.candidates);
            }
        }
        if !out.rolled_back.is_empty() {
            self.reorgs += 1;
        }
        out
    }

    pub fn open_branches(&self) -> usize { self.branches.len() }
    pub const fn commits(&self) -> u64 { self.commits }
    pub const fn reorg_count(&self) -> u64 { self.reorgs }

    /// §5.4 `preconf_to_final_divergence_rate`.
    pub fn divergence_rate(&self) -> f64 {
        if self.commits == 0 { return 0.0; }
        self.reorgs as f64 / self.commits as f64
    }
}
