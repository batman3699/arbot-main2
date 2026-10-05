//! **Task 2b.5's test.** The plane driven by a real `apex-search`, not a double.
//!
//! Task 8.4's end-to-end test proved the control plane could run a ticket to
//! `Reconciled` with every port faked. That was the right claim then — it is what
//! made "untestable as components" measurably untrue. It could not show that the
//! ports were *satisfiable*, and one of them turned out not to be: Task 8.4 named
//! `CandidateSource` for `apex-search`, and `apex-search` could not implement it
//! because only `apex-econ` can mint a `DiscreteSize`.
//!
//! So this test replaces exactly one double — the search — with the real
//! frontier, the real Engine D and the real Engine C, and requires the same
//! ticket to reach the same terminal state.

mod support;

use alloy_primitives::{Address, B256};
use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_math::finite_size::{best_size, NoSize, SearchBudget, SizedOpportunity, SizedRoute};
use apex_runtime::plane::{DispatchLane, Handled, Plane, Ports};
use apex_runtime::search::{flashblock_engine, FrontierSearch};
use apex_search::engine_c::{FiniteSizeEngine, TemplatePricer};
use apex_search::engine_d::EventEngine;
use apex_search::frontier::{
    FeeVariant, Frontier, GasClass, RouteId, RouteTemplate,
};
use apex_state::feed::event::{EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{FlashProviderId, PoolId, SignerLaneId, TokenId, VenueId};
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::state::{ReconstructionStatus, StateFingerprint};
use apex_types::ticket::TicketStatus;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EU256;
use std::collections::BTreeMap;
use std::sync::Arc;
use support::*;

const BOOT: UnixNanos = UnixNanos(1_000_000_000);
const GAS: u64 = 20_000_000_000_000;

fn p(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

/// A real constant-product round trip. The same curve `apex-search`'s own tests
/// use, so the economics under this test are arithmetic rather than a fixture.
#[derive(Clone, Copy)]
struct RoundTrip {
    reserve: u128,
    spread_bps: u64,
    fee_bps: u64,
}

impl RoundTrip {
    fn cp(&self, amount_in: EU256, r_in: u128, r_out: u128) -> Option<EU256> {
        let with_fee = amount_in.checked_mul(EU256::from(10_000 - self.fee_bps))?
            / EU256::from(10_000u64);
        let num = with_fee.checked_mul(EU256::from(r_out))?;
        let den = EU256::from(r_in).checked_add(with_fee)?;
        (!den.is_zero()).then(|| num / den)
    }
}

impl SizedRoute for RoundTrip {
    fn output(&self, amount_in: EU256) -> Option<EU256> {
        let mid = self.cp(amount_in, self.reserve, self.reserve)?;
        let tilted = self.reserve.checked_mul(10_000 + u128::from(self.spread_bps))? / 10_000;
        self.cp(mid, self.reserve, tilted)
    }
    fn fixed_cost(&self) -> EU256 {
        EU256::from(GAS)
    }
    fn max_input(&self) -> EU256 {
        EU256::from(self.reserve / 10)
    }
}

struct Pricer {
    routes: BTreeMap<RouteId, RoundTrip>,
}

impl TemplatePricer for Pricer {
    fn best_size(
        &self,
        id: RouteId,
        _at: &StateFingerprint,
        budget: SearchBudget,
    ) -> Result<SizedOpportunity, NoSize> {
        self.routes.get(&id).map_or(Err(NoSize::Unpriceable), |r| best_size(r, budget))
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

fn template(id: u64) -> RouteTemplate {
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
            FeeVariant { venue: VenueId(1), pool: p(0x33), fee_ppm: 3_000 },
            FeeVariant { venue: VenueId(2), pool: p(0x44), fee_ppm: 3_000 },
        ],
        tick_neighborhood: BTreeMap::new(),
        hook_fingerprint: None,
        flash_source: FlashProviderId(1),
        expected_gas_class: GasClass::TwoHopConstantProduct,
        last_profitable: None,
    }
}

fn search_over(spread_bps: u64) -> Arc<FrontierSearch> {
    let mut frontier = Frontier::new();
    frontier.insert(template(1)).expect("a consistent template");
    let pricer = Arc::new(Pricer {
        routes: BTreeMap::from([(
            RouteId(1),
            RoundTrip { reserve: 1_000_000_000_000_000_000_000, spread_bps, fee_bps: 30 },
        )]),
    });
    Arc::new(FrontierSearch::new(
        frontier,
        // `Unsafe`: this frontier was hand-seeded, not built from a verified pool
        // inventory. Nothing consults it yet -- that is INV-08, still open -- and
        // saying `Verified` here to make a test read nicer is exactly the claim
        // `ReconstructionStatus` refuses to let a caller make by accident.
        ReconstructionStatus::Unsafe,
        pricer,
        EventEngine::measured(),
        FiniteSizeEngine::measured(),
    ))
}

fn pool_signers() -> SignerPool {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    SignerPool::new(
        ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version },
        vec![LaneConfig {
            id: SignerLaneId(1),
            address: [0x22; 20],
            gas_reserve_wei: 1_000_000_000_000_000_000,
        }],
    )
}

fn plane_over(search: Arc<FrontierSearch>) -> Plane {
    Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(pool_signers()),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Live(Arc::new(NullDispatcher::new())),
        chain: Arc::new(FakeChain::landing()),
        search,
        econ: Arc::new(PassThroughEconomics::default()),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls: Arc::new(FixtureCalls),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    })
}

fn swap_on(pools: &[u8], block: u64) -> StateEvent {
    StateEvent {
        chain: BASE,
        at: Ordinal::confirmed(block, 12, 0),
        observed_at: UnixNanos(1_000_000_500),
        fingerprint: fingerprint(block, 1),
        kind: EventKind::PendingSwap {
            target: B256::repeat_byte(0x7f),
            pools: pools.iter().copied().map(p).collect(),
            notional_usd: Some(7_500.0),
        },
    }
}

/// **The task's test.** A real frontier, a real Engine D, a real Engine C, and a
/// ticket that still reaches `Reconciled`.
#[tokio::test]
async fn a_real_search_drives_a_ticket_to_reconciled() {
    let search = search_over(200);
    let plane = plane_over(Arc::clone(&search));
    plane.boot(&NoChain, BOOT).expect("boot");

    let handled = plane.on_event(&swap_on(&[0x33], 47_079_437)).await;
    let Some(Handled::Closed { outcome, .. }) = handled.first() else {
        panic!("expected a reconciled ticket, got {handled:?}");
    };
    assert!(outcome.is_success(), "{outcome:?}");

    let advanced: Vec<TicketStatus> = plane
        .registry()
        .journal()
        .replay()
        .expect("replay")
        .iter()
        .filter_map(|e| match e {
            apex_capture::journal::JournalEntry::Advanced { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(advanced.last(), Some(&TicketStatus::Reconciled), "{advanced:?}");
    assert_eq!(plane.registry().metrics().ticket_drop_count(), 0);
}

/// **Engine C's refusal, reaching the plane.** The same wiring at a spread that
/// cannot clear gas produces no ticket at all — and the frontier still ran, so
/// this is a refusal rather than a miss of the event.
#[tokio::test]
async fn a_spread_that_cannot_clear_gas_produces_no_ticket() {
    let search = search_over(61);
    let plane = plane_over(Arc::clone(&search));
    plane.boot(&NoChain, BOOT).expect("boot");

    let handled = plane.on_event(&swap_on(&[0x33], 47_079_437)).await;
    assert!(handled.is_empty(), "no proposal survives Engine C: {handled:?}");
    assert_eq!(plane.registry().metrics().tickets_admitted, 0);

    let (skipped, unpriced, declined, invalidated) = search.counters().snapshot();
    assert_eq!(declined, 1, "the template was priced and refused");
    assert_eq!((skipped, unpriced, invalidated), (0, 0, 0));
}

/// An event on no resident pool reaches Engine D's skip, and the counters say
/// which of the three reasons it was. Nothing is priced.
#[tokio::test]
async fn an_event_on_no_resident_pool_prices_nothing() {
    let search = search_over(200);
    let plane = plane_over(Arc::clone(&search));
    plane.boot(&NoChain, BOOT).expect("boot");

    let handled = plane.on_event(&swap_on(&[0x99], 47_079_437)).await;
    assert!(handled.is_empty(), "{handled:?}");
    let (skipped, _, declined, _) = search.counters().snapshot();
    assert_eq!(skipped, 1);
    assert_eq!(declined, 0, "nothing was priced, so nothing was refused");
}

/// **A tick transition stales a carried attribute, so the template stops being
/// priced.** Engine D says `Invalidate`; the search acts on it.
///
/// This is the ~140 bps mechanism closed at the wiring level: a template repriced
/// across a tick transition would price against a `tick_neighborhood` that no
/// longer contains the pool's tick.
#[tokio::test]
async fn a_tick_transition_takes_the_template_out_of_service() {
    let search = search_over(200);
    let plane = plane_over(Arc::clone(&search));
    plane.boot(&NoChain, BOOT).expect("boot");
    assert_eq!(search.resident(), 1);

    let mut tick = swap_on(&[0x33], 47_079_437);
    tick.kind = EventKind::TickTransition { pools: vec![p(0x33)] };
    let handled = plane.on_event(&tick).await;
    assert!(handled.is_empty(), "an invalidated template is not priced: {handled:?}");

    let (_, _, _, invalidated) = search.counters().snapshot();
    assert_eq!(invalidated, 1);
    assert_eq!(search.resident(), 0, "and it is out of service until refreshed");

    // A later swap on the same pool therefore finds nothing, rather than pricing
    // against the stale tick.
    let after = plane.on_event(&swap_on(&[0x33], 47_079_438)).await;
    assert!(after.is_empty(), "{after:?}");
    assert_eq!(plane.registry().metrics().tickets_admitted, 0);
}

/// The provenance is carried and is `Unsafe` for a hand-seeded frontier.
///
/// **Nothing consults it, and that is INV-08, which is open.** The test exists to
/// pin the honest value rather than to claim an enforcement: a later reader
/// should find the input already here, not discover that `Verified` was written
/// because it read better.
#[test]
fn a_hand_seeded_frontier_is_not_verified_state() {
    let search = search_over(200);
    assert_eq!(search.provenance(), ReconstructionStatus::Unsafe);
    assert!(
        !search.provenance().may_authorize_live_ticket(),
        "INV-08's predicate must refuse it, whenever the gate that asks is written"
    );
}

/// §29.3's budget, wired: with `ExactPricing` saturated, the §46.2 join does not
/// run and the proposal is declined rather than queued.
///
/// The budget existed and was tested in isolation (`resource_classes_isolated`);
/// nothing asserted the plane actually *consults* it on this path. A mutation
/// removing the reservation would otherwise have gone unnoticed — which is §6.5's
/// "written but never wired" pattern in its smallest form.
#[tokio::test]
async fn a_saturated_pricing_budget_declines_rather_than_queues() {
    use apex_runtime::workers::{Budgets, ResourceClass};

    let search = search_over(200);
    let plane = Plane::with_budgets(
        Ports {
            registry: Arc::new(TicketRegistry::new(
                Box::new(InMemoryJournal::new()),
                Box::new(ManualClock::at(BOOT.0)),
            )),
            pool: Arc::new(pool_signers()),
            gate: Arc::new(DispatchGate::shut()),
            dispatch: DispatchLane::Live(Arc::new(NullDispatcher::new())),
            chain: Arc::new(FakeChain::landing()),
            search,
            econ: Arc::new(PassThroughEconomics::default()),
            sim: Arc::new(AlwaysSucceeds),
            risk: Arc::new(AlwaysAdmits),
            commitments: Arc::new(FixtureCommitments),
            calls: Arc::new(FixtureCalls),
            signer: Arc::new(EchoSigner),
            live: Arc::new(FixedReadings(readings())),
            settlement: Arc::new(LandsAndFinalizes),
        },
        Budgets::with_capacity(1),
    );
    plane.boot(&NoChain, BOOT).expect("boot");

    // Hold the only pricing permit for the duration of the call.
    let held = plane.budgets().reserve(ResourceClass::ExactPricing).expect("the only permit");

    let handled = plane.on_event(&swap_on(&[0x33], 47_079_437)).await;
    assert!(
        matches!(
            handled.first(),
            Some(Handled::Declined(apex_runtime::plane::Decline::NoBudget(
                ResourceClass::ExactPricing
            )))
        ),
        "expected a budget refusal, got {handled:?}"
    );
    assert_eq!(plane.registry().metrics().tickets_admitted, 0);
    // INV-40: the refusal is a miss, because an opportunity went untaken for a
    // compute reason. Distinct from Engine C's decline, which is economic.
    assert_eq!(plane.misses().len(), 1);

    drop(held);
    let after = plane.on_event(&swap_on(&[0x33], 47_079_438)).await;
    assert!(
        matches!(after.first(), Some(Handled::Closed { .. })),
        "releasing the permit restores the path: {after:?}"
    );
}


/// **R17: an event is a flashblock, and one can move several pools.** Engine C
/// prices every template it touches. `measured()`'s eight per event was sized
/// for an event naming one pool, and leaves the most expensive unpriced.
#[tokio::test]
async fn a_flashblock_that_moves_several_pools_prices_every_template_it_touches() {
    // Four pools of one pair: twelve two-hop templates, each pool in six.
    let pools = [0x31u8, 0x32, 0x33, 0x34];
    let mut frontier = Frontier::new();
    let mut id = 0;
    for a in pools {
        for b in pools.into_iter().filter(|b| *b != a) {
            id += 1;
            let mut t = template(id);
            t.fee_variants[0].pool = p(a);
            t.fee_variants[1].pool = p(b);
            frontier.insert(t).expect("a consistent template");
        }
    }
    // No route prices, so every template looked at is refused.
    let pricer = Arc::new(Pricer { routes: BTreeMap::new() });

    for (engine, unpriced) in [(flashblock_engine(12), 0), (FiniteSizeEngine::measured(), 4)] {
        let search = Arc::new(FrontierSearch::new(
            frontier.clone(),
            ReconstructionStatus::Unsafe,
            pricer.clone(),
            EventEngine::measured(),
            engine,
        ));
        let plane = plane_over(Arc::clone(&search));
        plane.boot(&NoChain, BOOT).expect("boot");
        plane.on_event(&swap_on(&[0x31, 0x32, 0x33], 47_079_437)).await;
        let (_, left, declined, _) = search.counters().snapshot();
        assert_eq!((left, declined), (unpriced, 12 - unpriced), "max_templates {}", engine.max_templates());
    }
}
