//! Resolving disagreement between redundant feeds (Blueprint §5.6, INV-13).
//!
//! "If two feeds disagree, neither is promoted by majority vote: the controller
//! resolves the disagreement by parentage, sequence continuity, explicit
//! reconciliation, or full state rebuild."
//!
//! There is deliberately no `Majority` outcome. Redundancy exists to repair
//! transport failure, not to merge contradictory states -- two feeds echoing one
//! bad upstream are not two witnesses, and counting them would promote exactly
//! the wrong answer.

use apex_types::ids::FeedSourceId;

/// One feed's claim about the current head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedClaim {
    pub source: FeedSourceId,
    /// Opaque parent identity -- a block hash in production. Compared, never
    /// interpreted, so this type stays free of chain primitives.
    pub parent_tag: String,
    pub block: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Every claim shares a parent: the chain is agreed and the feed is usable.
    ByParentage { parent_tag: String, block: u64 },
    /// Claims disagree on parentage. No amount of counting resolves that, so the
    /// only sound answer is to rebuild and re-verify.
    Rebuild { reason: &'static str, claims: usize },
}

pub struct FeedArbiter;

impl FeedArbiter {
    pub fn resolve(claims: &[FeedClaim]) -> Resolution {
        let Some(first) = claims.first() else {
            return Resolution::Rebuild { reason: "no feed reported a head", claims: 0 };
        };

        let agreed = claims
            .iter()
            .all(|c| c.parent_tag == first.parent_tag && c.block == first.block);

        if agreed {
            return Resolution::ByParentage {
                parent_tag: first.parent_tag.clone(),
                block: first.block,
            };
        }

        // Note what is NOT here: tallying parent_tags and taking the most
        // common. That is the majority vote INV-13 forbids.
        Resolution::Rebuild {
            reason: "feeds disagree on parentage; redundancy repairs transport, not truth",
            claims: claims.len(),
        }
    }
}
