//! **Engine C: finite-size search (§12.3, BP-062).**
//!
//! # The evaluator already existed; this is the search over it
//!
//! Phase 2 Task 2.5 delivered `apex_math::finite_size` — `SizedRoute`,
//! [`best_size`](apex_math::finite_size::best_size), `SearchBudget`, `NoSize` —
//! and listed what it did not deliver: *"k-shortest simple routes, k-shortest
//! cycles, same-pair split, event-targeted, backrun and liquidation templates.
//! Pairwise cross-venue mismatch is the one the cheap frontier needs, and the
//! rest are search **topologies** over the same finite-size evaluator."* This
//! module is a topology: which resident templates to price, in what order, and
//! within what budget.
//!
//! # Engine C earns its place by refusing, not by finding
//!
//! The intuition it is usually sold on — "infinitesimal rates say no, finite size
//! says yes" — is **false for every venue this repository prices**. Task 2.5
//! proved it: constant product, the Solidly curves and concentrated liquidity all
//! have an output concave in its input and zero at zero, so the average rate over
//! `[0, x]` is at most the marginal rate at 0, and a cycle's finite-size gross
//! can never exceed its marginal gross. That intuition describes a *convex*
//! market.
//!
//! What is true is the other direction, and it is worth more. `graph.rs`'s edge
//! weights are rate-only — nothing adds a gas term — so Engine A calls any cycle
//! with a marginal gross above 1 a negative cycle **regardless of whether any
//! size pays for the transaction**. On this repository's own two-pool fixture at
//! ~1 cent of gas, three of the six measured spreads in the 60–65 bps band cannot
//! be traded at any size. That band is not a corner case: it is where the cheap
//! frontier sits, and it is consistent with the census finding 0 of 750
//! candidates net-positive.
//!
//! So this engine's output is as much the **declines** as the proposals, and the
//! declines carry `NoSize`, which answers with a `MissReason` (INV-40).
//!
//! # The budget is a refusal, not a queue
//!
//! §29.3: "no strategy may create an unbounded queue." The frontier hands back
//! templates already ranked fee-first, so pricing them in order means a budget cut
//! falls on the most expensive routes — the ones least likely to clear anyway.
//! What is left unpriced is **reported**, because a budget nobody can see the cost
//! of is a budget nobody can set.

use crate::frontier::{Frontier, ProposalOrigin, Revalued, RouteId, RouteProposal};
use apex_math::finite_size::{NoSize, SearchBudget, SizedOpportunity, Surplus};
use apex_state::feed::event::StateEvent;
use apex_types::compat::u256_to_alloy;
use apex_types::ids::{ChainId, FlashProviderId, VenueId};
use apex_types::route::RouteCommitment;
use apex_types::state::StateFingerprint;

/// Prices one resident template against live state, as a finite-size curve.
///
/// Implemented by `apex-venues` (pool state → a `SizedRoute`) and by test
/// doubles. Engine C does not price; it decides **what** to price and in what
/// order, which is what a search topology is.
///
/// It returns `apex-math`'s own verdict rather than an `Option`, because
/// `NoSize` distinguishes "no size pays" from "a venue would not price it" and
/// those are different failures with different owners.
pub trait TemplatePricer {
    fn best_size(
        &self,
        id: RouteId,
        at: &StateFingerprint,
        budget: SearchBudget,
    ) -> Result<SizedOpportunity, NoSize>;

    /// The route this template commits to, for the proposal.
    fn commitment(&self, id: RouteId) -> Option<RouteCommitment>;

    /// The fingerprint a proposal for this template commits to. Engine C asks
    /// **before** pricing the template.
    ///
    /// The default is the event's own. A pricer over live state narrows the venue
    /// versions to the template's own pools: last-mile revalidation compares them
    /// by equality before signing, and an event's describe only the pool that
    /// moved. Read before pricing, they are no newer than any state the proposal
    /// is priced against, so a write at any point after is one last-mile sees —
    /// where a reading taken after pricing would absorb a write that landed
    /// during it, and pass a ticket priced against state that had moved.
    fn route_fingerprint(&self, _id: RouteId, at: &StateFingerprint) -> StateFingerprint {
        at.clone()
    }
}

/// One template that could not be traded, and `apex-math`'s reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declined {
    pub route: RouteId,
    pub why: NoSize,
}

/// What Engine C did with one event's revalued set.
#[derive(Debug)]
pub struct Proposals {
    pub proposals: Vec<RouteProposal>,
    /// Priced and refused. Each carries a `NoSize`, which answers with a
    /// `MissReason` — so the 96% bucket is filed rather than inferred.
    pub declined: Vec<Declined>,
    /// Templates the budget did not reach.
    ///
    /// **Not declines.** Nothing was evaluated, so calling them `LOW_EV` would be
    /// a claim nobody measured — the same distinction Engine D draws between a
    /// skip and a miss. Reported so the cost of the budget is visible.
    pub unpriced: Vec<RouteId>,
    pub evaluated: usize,
}

impl Proposals {
    pub fn is_empty(&self) -> bool {
        self.proposals.is_empty()
    }
}

/// §12.3's engine, as a topology over `apex-math`'s evaluator.
#[derive(Debug, Clone, Copy)]
pub struct FiniteSizeEngine {
    /// How many templates may be priced for one event. Distinct from
    /// `SearchBudget::max_evaluations`, which bounds the size search *within* one
    /// template: this bounds how many templates are looked at, that bounds how
    /// hard each is looked at.
    max_templates: usize,
    per_route: SearchBudget,
}

impl FiniteSizeEngine {
    /// Eight templates per event.
    ///
    /// The measured tradeable set on Base is 8–11 cross-venue pairs at 1.6–7 bps
    /// and ≥ $100k depth, so pricing more than that per event is spending the
    /// §29 compute budget on routes the inventory measurement already excluded.
    pub const MEASURED_TRADEABLE_SET: usize = 8;

    pub const fn new(max_templates: usize, per_route: SearchBudget) -> Self {
        Self { max_templates, per_route }
    }

    pub fn measured() -> Self {
        Self::new(Self::MEASURED_TRADEABLE_SET, SearchBudget::default())
    }

    pub const fn max_templates(&self) -> usize {
        self.max_templates
    }

    /// Price the revalued set, in the frontier's order, and propose what pays.
    ///
    /// Takes a [`Revalued`] rather than producing one: §12.1's ordering is that
    /// the frontier is consulted first, and this engine works on what it returned.
    pub fn propose(
        &self,
        hits: &[RouteId],
        _after: &Revalued,
        event: &StateEvent,
        frontier: &Frontier,
        pricer: &dyn TemplatePricer,
    ) -> Proposals {
        let (mut proposals, mut declined) = (Vec::new(), Vec::new());
        let mut evaluated = 0usize;

        // The frontier ranked these fee-first, so taking a prefix means a budget
        // cut falls on the most expensive routes -- the ones least likely to
        // clear. That is the ranking earning its keep rather than decorating a
        // log line.
        let (priced, unpriced) = hits.split_at(hits.len().min(self.max_templates));

        for id in priced {
            evaluated += 1;
            // `Ok` **is** a gain. `best_size`'s own tail is
            // `Some(b) if b.net.is_gain() => Ok(b)` / `Some(b) => Err(NoProfitableSize)`,
            // so a guard here would be dead and a loss arm unreachable. The first
            // draft had both; a mutation replacing the guard with `if true`
            // changed nothing, which is how dead code announces itself. Same
            // shape as the frontier's chain filter and the two guards Task 7.2
            // removed. `apex-math`'s `best_size_returns_ok_only_for_a_gain` pins
            // the contract at its source, which is the honest place for it.
            //
            // The fingerprint first: see `TemplatePricer::route_fingerprint`.
            let fingerprint = pricer.route_fingerprint(*id, &event.fingerprint);
            match pricer.best_size(*id, &event.fingerprint, self.per_route) {
                Ok(best) => {
                    let Some(template) = frontier.get(*id) else { continue };
                    let Some(route) = pricer.commitment(*id) else { continue };
                    proposals.push(RouteProposal {
                        chain: template.chain,
                        route,
                        venue_set: venues_of(template),
                        state_fingerprint: fingerprint,
                        found_at: event.observed_at,
                        origin: ProposalOrigin::FiniteSize,
                        flash_source: flash_source_of(template),
                        // The size the search found -- a `U256`, never a
                        // `DiscreteSize`. INV-18: the executed size comes from
                        // `apex-econ::sizing::discrete::refine` and nowhere else,
                        // so this is an input to that refinement and the type is
                        // what says so.
                        size_hint: Some(u256_to_alloy(best.amount_in)),
                        // `Ok` is a gain (see above), so the loss arm cannot
                        // happen; `None` keeps the type honest if it ever did.
                        net_hint: match best.net {
                            Surplus::Gain(g) => Some(u256_to_alloy(g)),
                            Surplus::Loss(_) => None,
                        },
                    });
                }
                Err(why) => declined.push(Declined { route: *id, why }),
            }
        }

        Proposals { proposals, declined, unpriced: unpriced.to_vec(), evaluated }
    }
}

fn venues_of(template: &crate::frontier::RouteTemplate) -> Vec<VenueId> {
    let mut v = template.venue_sequence.clone();
    v.sort_unstable();
    v.dedup();
    v
}

/// `FlashProviderId(0)` is the sentinel for "no flash source", matching
/// `ExecutionCommitment`'s own encoding of the same fact.
fn flash_source_of(template: &crate::frontier::RouteTemplate) -> Option<FlashProviderId> {
    (template.flash_source.0 != 0).then_some(template.flash_source)
}

/// Exposed so a caller can name a chain without importing `apex-types`.
pub const fn chain_of(p: &RouteProposal) -> ChainId {
    p.chain
}
