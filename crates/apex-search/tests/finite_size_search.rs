//! **BP-062.** Engine C as a search topology over `apex-math`'s finite-size
//! evaluator.
//!
//! The engine's value runs the opposite way to the intuition it is usually sold
//! on. Phase 2 Task 2.5 proved that no fixture exists where infinitesimal rates
//! say no and a finite size says yes — every venue this repository prices has an
//! output concave in its input and zero at zero, so finite-size gross can never
//! exceed marginal gross. What Engine C does is **refuse what Engine A proposes**,
//! because `graph.rs`'s edge weights are rate-only and carry no gas term.

use alloy_primitives::{Address, B256};
use apex_math::finite_size::{best_size, NoSize, SearchBudget, SizedOpportunity, SizedRoute};
use apex_search::engine_c::{FiniteSizeEngine, TemplatePricer};
use apex_search::frontier::{
    FeeVariant, Frontier, GasClass, ProposalOrigin, RouteId, RouteTemplate,
};
use apex_state::feed::event::{EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{ChainId, FlashProviderId, PoolId, TokenId, VenueId};
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EU256;
use std::collections::BTreeMap;

const BASE: ChainId = ChainId(8453);
const NOW: UnixNanos = UnixNanos(1_781_049_614_240_000_000);

/// ~1 cent of gas at this repository's own measured figure.
const GAS: u64 = 20_000_000_000_000;

fn pool(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

/// A real two-pool round trip: buy on one constant-product pool, sell on
/// another whose price is `spread_bps` higher. Output is concave in input and
/// zero at zero, which is what makes the finite-size claim testable at all.
#[derive(Clone, Copy)]
struct TwoPoolRoundTrip {
    reserve_in: u128,
    reserve_out: u128,
    spread_bps: u64,
    fee_bps: u64,
    fixed_cost: u64,
}

impl TwoPoolRoundTrip {
    fn cp(&self, amount_in: EU256, r_in: u128, r_out: u128) -> Option<EU256> {
        let fee_num = EU256::from(10_000u64 - self.fee_bps);
        let with_fee = amount_in.checked_mul(fee_num)? / EU256::from(10_000u64);
        let num = with_fee.checked_mul(EU256::from(r_out))?;
        let den = EU256::from(r_in).checked_add(with_fee)?;
        (!den.is_zero()).then(|| num / den)
    }
}

impl SizedRoute for TwoPoolRoundTrip {
    fn output(&self, amount_in: EU256) -> Option<EU256> {
        // Leg 1: token A -> token B on the cheap pool.
        let mid = self.cp(amount_in, self.reserve_in, self.reserve_out)?;
        // Leg 2: token B -> token A on the pool whose price is `spread` higher,
        // which is the same curve with the reserves tilted.
        let tilted_out = self
            .reserve_in
            .checked_mul(10_000 + u128::from(self.spread_bps))?
            / 10_000;
        self.cp(mid, self.reserve_out, tilted_out)
    }

    fn fixed_cost(&self) -> EU256 {
        EU256::from(self.fixed_cost)
    }

    fn max_input(&self) -> EU256 {
        EU256::from(self.reserve_in / 10)
    }
}

fn route_at(spread_bps: u64, fixed_cost: u64) -> TwoPoolRoundTrip {
    TwoPoolRoundTrip {
        reserve_in: 1_000_000_000_000_000_000_000,
        reserve_out: 1_000_000_000_000_000_000_000,
        spread_bps,
        fee_bps: 30,
        fixed_cost,
    }
}

/// A pricer holding one curve per template.
struct Fixture {
    routes: BTreeMap<RouteId, TwoPoolRoundTrip>,
}

impl TemplatePricer for Fixture {
    fn best_size(
        &self,
        id: RouteId,
        _at: &StateFingerprint,
        budget: SearchBudget,
    ) -> Result<SizedOpportunity, NoSize> {
        match self.routes.get(&id) {
            Some(route) => best_size(route, budget),
            None => Err(NoSize::Unpriceable),
        }
    }

    fn commitment(&self, _id: RouteId) -> Option<RouteCommitment> {
        Some(RouteCommitment {
            hops: Vec::new(),
            complexity_cost: ComplexityCost {
                hops: 2,
                external_calls: 2,
                calldata_bytes: 420,
                state_deps: 2,
                tick_crossings: 0,
                hooks: 0,
                gas_estimate: 240_000,
                failure_surface: 0.01,
            },
            route_hash: B256::repeat_byte(0x44),
        })
    }
}

fn template(id: u64, fee_ppm: u32) -> RouteTemplate {
    RouteTemplate {
        id: RouteId(id),
        chain: BASE,
        topology: vec![
            TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
            TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
            TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
        ],
        venue_sequence: vec![VenueId(1), VenueId(2)],
        fee_variants: vec![
            FeeVariant { venue: VenueId(1), pool: pool(0x33), fee_ppm },
            FeeVariant { venue: VenueId(2), pool: pool(0x44), fee_ppm },
        ],
        tick_neighborhood: BTreeMap::new(),
        hook_fingerprint: None,
        flash_source: FlashProviderId(1),
        expected_gas_class: GasClass::TwoHopConstantProduct,
        last_profitable: None,
    }
}

fn swap() -> StateEvent {
    StateEvent {
        chain: BASE,
        at: Ordinal::confirmed(47_079_437, 12, 0),
        observed_at: NOW,
        fingerprint: StateFingerprint {
            chain_id: BASE,
            parent_block_hash: B256::repeat_byte(0xaa),
            confirmed_block_number: 47_079_437,
            preconf_sequence: None,
            flashblock_index: None,
            state_root_or_equivalent: None,
            block_hash_if_available: None,
            state_delta_hash: B256::repeat_byte(0xbb),
            venue_state_version: BTreeMap::new(),
            external_dependency_fingerprint: None,
        },
        kind: EventKind::PendingSwap {
            target: B256::repeat_byte(0x7f),
            pools: vec![pool(0x33)],
            notional_usd: Some(7_500.0),
        },
    }
}

/// **BP-062's test, in the direction that is actually true.**
///
/// Engine A's edge weights are rate-only, so it proposes any cycle whose marginal
/// gross exceeds 1. This route's does — and no size clears the fixed cost. Engine
/// C must produce no proposal and file a `NoProfitableSize`.
#[test]
fn engine_c_refuses_what_engine_a_proposes() {
    // 61 bps of spread against 60 bps of round-trip fee: marginally positive, and
    // 300x short of ~1 cent of gas. Measured in Phase 2 Task 2.5.
    let route = route_at(61, GAS);

    // Engine A's question, asked exactly: is the marginal gross above parity?
    // A basis-point report rounds this to zero, which is why the test asks
    // `output > input` rather than a rounded figure.
    let probe = EU256::from(1_000_000_000_000_000u64);
    let marginal = route.output(probe).expect("priceable");
    assert!(marginal > probe, "Engine A proposes this cycle: its marginal gross exceeds parity");

    // Engine C's answer.
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());
    let pricer = Fixture { routes: BTreeMap::from([(RouteId(1), route)]) };

    let out = FiniteSizeEngine::measured()
        .propose(&hits, &permit, &swap(), &frontier, &pricer);

    assert!(out.is_empty(), "no size pays for the transaction: {out:?}");
    assert_eq!(out.declined.len(), 1);
    assert!(
        matches!(out.declined[0].why, NoSize::NoProfitableSize { .. }),
        "the decline must be the Engine C verdict, not a pricing failure: {:?}",
        out.declined[0].why
    );

    // And the decline answers with a bucket, so the 96% is filed rather than
    // inferred (INV-40).
    use apex_types::miss::{ExplainsMiss, MissReason};
    assert_eq!(out.declined[0].why.miss_reason(), MissReason::LowEv);
}

/// The complement, so the pair discriminates: widen the spread and the same
/// engine proposes, with a size. Without this, "refuses" is satisfied by an
/// engine that refuses everything.
#[test]
fn a_spread_that_clears_the_fixed_cost_is_proposed_with_a_size() {
    let route = route_at(200, GAS);
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());
    let pricer = Fixture { routes: BTreeMap::from([(RouteId(1), route)]) };

    let out = FiniteSizeEngine::measured()
        .propose(&hits, &permit, &swap(), &frontier, &pricer);

    assert_eq!(out.proposals.len(), 1, "{out:?}");
    assert!(out.declined.is_empty());
    let p = &out.proposals[0];
    assert_eq!(p.origin, ProposalOrigin::FiniteSize);
    assert!(p.size_hint.is_some(), "a rate-only search cannot express a quantity at all");
    assert!(
        p.size_hint.unwrap_or_default() > alloy_primitives::U256::ZERO,
        "the hint is the size the search found"
    );
}

/// The hint is a `U256` and **not** a `DiscreteSize`, and that is INV-18.
///
/// Engine C searches *for* a size, so it has one in mind — but the executed size
/// must come from `apex-econ::sizing::discrete::refine`, which is the only thing
/// that can mint the witness. The hint is an input to that refinement. This test
/// exists because the distinction is invisible at a glance and load-bearing.
#[test]
fn the_size_hint_is_a_hint_and_not_a_size() {
    let route = route_at(200, GAS);
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());
    let pricer = Fixture { routes: BTreeMap::from([(RouteId(1), route)]) };
    let out = FiniteSizeEngine::measured()
        .propose(&hits, &permit, &swap(), &frontier, &pricer);

    // The field's type is the assertion: this compiles only because it is a
    // plain U256. A `DiscreteSize` has no such constructor outside apex-econ.
    let hint: Option<alloy_primitives::U256> = out.proposals[0].size_hint;
    assert!(hint.is_some());
}

/// §29.3: no unbounded queue. The budget bounds how many templates are priced,
/// and what it did not reach is **reported** rather than silently dropped.
#[test]
fn the_template_budget_is_bounded_and_the_remainder_is_reported() {
    let mut frontier = Frontier::new();
    let mut routes = BTreeMap::new();
    for i in 1..=5u64 {
        #[allow(clippy::cast_possible_truncation)]
        frontier.insert(template(i, 100 * i as u32)).expect("consistent");
        routes.insert(RouteId(i), route_at(200, GAS));
    }
    let (hits, permit) = frontier.revalue(&swap());
    assert_eq!(hits.len(), 5);

    let engine = FiniteSizeEngine::new(2, SearchBudget::default());
    let out = engine.propose(&hits, &permit, &swap(), &frontier, &Fixture { routes });

    assert_eq!(out.evaluated, 2, "the budget bounds what is priced");
    assert_eq!(out.unpriced.len(), 3, "and the remainder is visible");
    assert_eq!(
        out.unpriced,
        vec![RouteId(3), RouteId(4), RouteId(5)],
        "the frontier ranked fee-first, so the budget cut falls on the most expensive"
    );
    // Unpriced templates are NOT declines: nothing was evaluated, so calling them
    // LOW_EV would be a claim nobody measured.
    assert!(out.declined.is_empty());
}

/// A venue that will not price is `Unpriceable`, not `NoProfitableSize`. The two
/// have different owners: one is a broken adapter, the other is an economic
/// verdict, and folding them together hides the first inside the largest bucket.
#[test]
fn a_venue_that_will_not_price_is_distinguished_from_one_that_does_not_pay() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());

    // The pricer holds no curve for this template.
    let out = FiniteSizeEngine::measured().propose(
        &hits,
        &permit,
        &swap(),
        &frontier,
        &Fixture { routes: BTreeMap::new() },
    );

    assert_eq!(out.declined.len(), 1);
    assert_eq!(out.declined[0].why, NoSize::Unpriceable);

    use apex_types::miss::{ExplainsMiss, MissReason};
    assert_eq!(
        out.declined[0].why.miss_reason(),
        MissReason::SimFail,
        "a failure to evaluate is not an economic verdict"
    );
}

/// The claim Task 2.5 proved, re-asserted here because this engine is built on
/// it: average rate over `[0, x]` never exceeds the marginal rate at 0. If it
/// ever did, Engine C would have a second job and this module's argument would
/// be wrong.
#[test]
fn finite_size_gross_never_exceeds_marginal_gross() {
    for spread in [10u64, 61, 65, 200, 1_000] {
        let route = route_at(spread, 0);
        let tiny = EU256::from(1_000_000_000_000u64);
        let Some(marginal_out) = route.output(tiny) else { continue };

        for mult in [10u64, 100, 1_000, 10_000] {
            let x = tiny * EU256::from(mult);
            let Some(out) = route.output(x) else { continue };
            // out/x <= marginal_out/tiny, compared by cross-multiplication so
            // nothing is lost to integer division.
            assert!(
                out * tiny <= marginal_out * x,
                "spread {spread} bps at {mult}x: average rate exceeded the marginal rate, \
                 which would mean a convex market"
            );
        }
    }
}

/// **The proposal commits to venue versions read before pricing.**
///
/// A pricer over live state narrows the fingerprint to the template's own
/// pools, and Engine C must ask for it *before* `best_size`: a write that lands
/// while the template is priced has to be one last-mile sees, and a reading
/// taken after pricing would absorb it. This pricer's state moves during every
/// `best_size`, as a swap landing mid-search would.
#[test]
fn the_proposal_commits_to_versions_read_before_pricing() {
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Moving {
        inner: Fixture,
        version: AtomicU64,
    }
    impl TemplatePricer for Moving {
        fn best_size(
            &self,
            id: RouteId,
            at: &StateFingerprint,
            budget: SearchBudget,
        ) -> Result<SizedOpportunity, NoSize> {
            self.version.fetch_add(1, Ordering::SeqCst);
            self.inner.best_size(id, at, budget)
        }
        fn commitment(&self, id: RouteId) -> Option<RouteCommitment> {
            self.inner.commitment(id)
        }
        fn route_fingerprint(&self, _id: RouteId, at: &StateFingerprint) -> StateFingerprint {
            StateFingerprint {
                venue_state_version: BTreeMap::from([(VenueId(1), self.version.load(Ordering::SeqCst))]),
                ..at.clone()
            }
        }
    }
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());
    let routes = || BTreeMap::from([(RouteId(1), route_at(200, GAS))]);

    let moving = Moving { inner: Fixture { routes: routes() }, version: AtomicU64::new(7) };
    let out = FiniteSizeEngine::measured().propose(&hits, &permit, &swap(), &frontier, &moving);
    let fp = &out.proposals[0].state_fingerprint;
    assert_eq!(fp.venue_state_version, BTreeMap::from([(VenueId(1), 7)]), "read after pricing");
    assert_eq!(fp.state_delta_hash, swap().fingerprint.state_delta_hash, "the event's other fields are kept");

    // A pricer that does not narrow commits to the event's own fingerprint,
    // venue versions and all.
    let mut event = swap();
    event.fingerprint.venue_state_version.insert(VenueId(2), 3);
    let plain = Fixture { routes: routes() };
    let out = FiniteSizeEngine::measured().propose(&hits, &permit, &event, &frontier, &plain);
    assert_eq!(out.proposals[0].state_fingerprint, event.fingerprint);
}
