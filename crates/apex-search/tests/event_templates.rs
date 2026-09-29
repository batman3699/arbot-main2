//! **BP-063.** §12.4's eight event classes, and what each one means for a
//! template that touches it.

use alloy_primitives::{Address, B256};
use apex_search::engine_d::{action_for, EventEngine, Skipped, StaleAttribute, TemplateAction};
use apex_search::frontier::{FeeVariant, Frontier, GasClass, RouteId, RouteTemplate};
use apex_state::feed::event::{EventClass, EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{ChainId, FlashProviderId, PoolId, TokenId, VenueId};
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use std::collections::{BTreeMap, BTreeSet};

const BASE: ChainId = ChainId(8453);
const NOW: UnixNanos = UnixNanos(1_781_049_614_240_000_000);

fn pool(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

fn resident() -> Frontier {
    let mut f = Frontier::new();
    f.insert(RouteTemplate {
        id: RouteId(1),
        chain: BASE,
        topology: vec![
            TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
            TokenId { chain: BASE, address: Address::repeat_byte(0x02) },
            TokenId { chain: BASE, address: Address::repeat_byte(0x01) },
        ],
        venue_sequence: vec![VenueId(1), VenueId(2)],
        fee_variants: vec![
            FeeVariant { venue: VenueId(1), pool: pool(0x33), fee_ppm: 500 },
            FeeVariant { venue: VenueId(2), pool: pool(0x44), fee_ppm: 300 },
        ],
        tick_neighborhood: BTreeMap::new(),
        hook_fingerprint: None,
        flash_source: FlashProviderId(1),
        expected_gas_class: GasClass::TwoHopConcentrated,
        last_profitable: None,
    })
    .expect("a consistent template");
    f
}

fn event(kind: EventKind) -> StateEvent {
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
        kind,
    }
}

fn touching(class: EventClass) -> EventKind {
    let pools = vec![pool(0x33)];
    match class {
        EventClass::LargeSwap => EventKind::PendingSwap {
            target: B256::repeat_byte(0x7f),
            pools,
            notional_usd: Some(7_500.0),
        },
        EventClass::LiquidityChange => EventKind::LiquidityChange { pools, added: false },
        EventClass::Liquidation => EventKind::Liquidation { pools },
        EventClass::OracleMutation => EventKind::OracleMutation { pools },
        EventClass::StableDislocation => EventKind::StableDislocation { pools },
        EventClass::TickTransition => EventKind::TickTransition { pools },
        EventClass::HookMutation => EventKind::HookMutation { pools },
        EventClass::FeeChange => EventKind::FeeChange { pools },
    }
}

/// **BP-063's test.** Every §12.4 class maps to an action, and the map is
/// exhaustive in both directions.
///
/// One direction alone would not do: a class with no action is an event the
/// engine silently drops, and an action no class produces is a branch nothing
/// exercises. Task 8.1's first draft got the second wrong and the
/// both-directions assertion caught it.
#[test]
fn event_templates() {
    let engine = EventEngine::measured();
    let frontier = resident();

    let mut seen_actions: BTreeSet<&'static str> = BTreeSet::new();
    for class in EventKind::TEMPLATE_CLASSES {
        let response = engine.respond(&event(touching(class)), &frontier);
        assert_eq!(response.class, Some(class), "{class:?} was classified as something else");
        assert!(response.skipped.is_none(), "{class:?} touched a resident template: {response:?}");
        assert!(!response.is_empty(), "{class:?} produced no work at all");

        match action_for(class) {
            TemplateAction::Revalue => {
                assert_eq!(response.revalue, vec![RouteId(1)], "{class:?}");
                assert!(response.invalidate.is_empty(), "{class:?}");
                seen_actions.insert("revalue");
            }
            TemplateAction::Invalidate(attr) => {
                assert_eq!(response.invalidate, vec![(RouteId(1), attr)], "{class:?}");
                assert!(
                    response.revalue.is_empty(),
                    "{class:?} invalidates a carried attribute; repricing it would price \
                     against the stale value"
                );
                seen_actions.insert("invalidate");
            }
        }
    }

    assert_eq!(
        seen_actions,
        BTreeSet::from(["invalidate", "revalue"]),
        "both actions must be reachable, or one branch is decoration"
    );
}

/// **The finding this engine is built around.** Three of §12.4's eight classes do
/// not say a price moved — they say a carried attribute is now wrong.
///
/// Repricing on those would price against the stale value. That is not
/// hypothetical: the fast path carrying no tick ladder is the measured mechanism
/// behind a ~140 bps local-vs-quoter gap, and a template repriced across a tick
/// transition reaches the same error by a second route.
#[test]
fn three_classes_invalidate_an_attribute_rather_than_moving_a_price() {
    let invalidating: BTreeMap<EventClass, StaleAttribute> = EventKind::TEMPLATE_CLASSES
        .into_iter()
        .filter_map(|c| match action_for(c) {
            TemplateAction::Invalidate(a) => Some((c, a)),
            TemplateAction::Revalue => None,
        })
        .collect();

    assert_eq!(
        invalidating,
        BTreeMap::from([
            (EventClass::TickTransition, StaleAttribute::TickNeighborhood),
            (EventClass::HookMutation, StaleAttribute::HookFingerprint),
            (EventClass::FeeChange, StaleAttribute::FeeVariants),
        ]),
        "exactly these three, and each names the attribute it stales"
    );

    // And the three attributes are distinct: two classes staling the same field
    // would mean one of them is not really about that field.
    let attrs: BTreeSet<StaleAttribute> = invalidating.values().copied().collect();
    assert_eq!(attrs.len(), 3);
    let labels: BTreeSet<&str> = attrs.iter().map(|a| a.label()).collect();
    assert_eq!(labels.len(), 3, "two attributes share a label");
}

/// A block moves state and names no pools, so there is nothing to key a template
/// on. Skipped, and the skip says which kind it was.
#[test]
fn a_block_is_not_a_template_class() {
    let engine = EventEngine::measured();
    let r = engine.respond(&event(EventKind::Block), &resident());
    assert_eq!(r.class, None);
    assert_eq!(r.skipped, Some(Skipped::NotATemplateClass));
    assert!(r.is_empty());
    // The permit exists anyway: the frontier WAS consulted, and an empty
    // revaluation is exactly what broad discovery is for.
    assert_eq!(r.permit.hits(), 0);
}

/// **The notional floor, and the measurement behind it.** The census that found
/// this repository's first net-positive samples triggered on swaps ≥ $5,000.
#[test]
fn a_swap_below_the_notional_floor_is_skipped_and_counted() {
    let engine = EventEngine::measured();
    let frontier = resident();

    let small = event(EventKind::PendingSwap {
        target: B256::repeat_byte(0x7f),
        pools: vec![pool(0x33)],
        notional_usd: Some(100.0),
    });
    let r = engine.respond(&small, &frontier);
    assert_eq!(r.skipped, Some(Skipped::BelowNotionalFloor));
    assert!(r.is_empty());
    assert_eq!(r.class, Some(EventClass::LargeSwap), "still classified; just not acted on");

    // Exactly at the floor is admitted -- the census's threshold is ">= $5,000".
    let at_floor = event(EventKind::PendingSwap {
        target: B256::repeat_byte(0x7f),
        pools: vec![pool(0x33)],
        notional_usd: Some(EventEngine::MEASURED_NOTIONAL_FLOOR_USD),
    });
    assert!(engine.respond(&at_floor, &frontier).skipped.is_none());
}

/// An **unmeasured** notional is admitted, and that asymmetry is deliberate.
///
/// "We did not measure this swap's size" and "this swap was small" are different
/// facts. Admitting an unmeasured swap costs one `BTreeMap` lookup; skipping one
/// costs an opportunity nobody will ever know about. The lookup is cheap enough
/// that the conservative direction is also the affordable one.
#[test]
fn an_unmeasured_notional_is_admitted_rather_than_assumed_small() {
    let engine = EventEngine::measured();
    let unknown = event(EventKind::PendingSwap {
        target: B256::repeat_byte(0x7f),
        pools: vec![pool(0x33)],
        notional_usd: None,
    });
    let r = engine.respond(&unknown, &resident());
    assert!(r.skipped.is_none(), "an unmeasured swap is not a small one");
    assert_eq!(r.revalue, vec![RouteId(1)]);
}

/// Only swaps carry a notional. A tick transition has no size, and gating it on
/// one would silence the class entirely.
#[test]
fn the_notional_floor_applies_only_to_swaps() {
    let engine = EventEngine::new(1_000_000.0);
    let frontier = resident();
    for class in EventKind::TEMPLATE_CLASSES {
        if class == EventClass::LargeSwap {
            continue;
        }
        let r = engine.respond(&event(touching(class)), &frontier);
        assert!(
            r.skipped.is_none(),
            "{class:?} has no notional and must not be gated on one"
        );
    }
}

/// An event on a pool nobody holds a template for is skipped with its own
/// reason — distinct from "below the floor", because the two call for different
/// responses: one is a threshold to re-measure, the other is a frontier to grow.
#[test]
fn an_event_on_no_resident_pool_says_so_distinctly() {
    let engine = EventEngine::measured();
    let elsewhere = event(EventKind::PendingSwap {
        target: B256::repeat_byte(0x7f),
        pools: vec![pool(0x99)],
        notional_usd: Some(50_000.0),
    });
    let r = engine.respond(&elsewhere, &resident());
    assert_eq!(r.skipped, Some(Skipped::NoResidentTemplate));
    assert_ne!(r.skipped, Some(Skipped::BelowNotionalFloor));
    assert_eq!(r.permit.hits(), 0, "and broad discovery is what this case is for");
}

/// The floor is configurable, because it is a measured value that will move. The
/// same census swept the depth floor across $100k / $50k / $25k and the middle
/// one beat the others by an order of magnitude on dollars per 45 minutes.
#[test]
fn the_floor_is_a_parameter_not_a_constant() {
    let permissive = EventEngine::new(10.0);
    let strict = EventEngine::new(1_000_000.0);
    let frontier = resident();
    let swap = event(EventKind::PendingSwap {
        target: B256::repeat_byte(0x7f),
        pools: vec![pool(0x33)],
        notional_usd: Some(7_500.0),
    });

    assert!(permissive.respond(&swap, &frontier).skipped.is_none());
    assert_eq!(
        strict.respond(&swap, &frontier).skipped,
        Some(Skipped::BelowNotionalFloor)
    );
    assert!(
        (EventEngine::default().notional_floor_usd()
            - EventEngine::MEASURED_NOTIONAL_FLOOR_USD)
            .abs()
            < f64::EPSILON,
        "the default is the measured value, not a round number"
    );
}
