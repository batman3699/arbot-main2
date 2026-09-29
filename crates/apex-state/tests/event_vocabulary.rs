//! §12.4's event classes, and the two questions the vocabulary has to answer.

use alloy_primitives::{Address, B256};
use apex_state::feed::event::{EventClass, EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{ChainId, PoolId};
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use std::collections::{BTreeMap, BTreeSet};

const BASE: ChainId = ChainId(8453);

fn pools() -> Vec<PoolId> {
    vec![PoolId { chain: BASE, address: Address::repeat_byte(0x33) }]
}

fn every_kind() -> Vec<EventKind> {
    vec![
        EventKind::Block,
        EventKind::Preconfirmation,
        EventKind::PendingSwap {
            target: B256::repeat_byte(0x7f),
            pools: pools(),
            notional_usd: Some(7_500.0),
        },
        EventKind::LiquidityChange { pools: pools(), added: false },
        EventKind::Liquidation { pools: pools() },
        EventKind::OracleMutation { pools: pools() },
        EventKind::StableDislocation { pools: pools() },
        EventKind::TickTransition { pools: pools() },
        EventKind::HookMutation { pools: pools() },
        EventKind::FeeChange { pools: pools() },
    ]
}

/// §12.4 names eight template classes, and `TEMPLATE_CLASSES` must be exactly
/// those — asserted in **both** directions.
///
/// One direction alone is not enough, and that lesson is Task 8.1's: its first
/// draft listed `SimFail` among the unreachable miss buckets and the
/// both-directions assertion caught it. A class in the constant that no variant
/// produces is a template set nothing ever fills; a variant that produces a class
/// the constant omits is an event Engine D silently ignores.
#[test]
fn the_eight_template_classes_are_exactly_the_reachable_ones() {
    let reachable: BTreeSet<EventClass> =
        every_kind().iter().filter_map(EventKind::class).collect();
    let declared: BTreeSet<EventClass> = EventKind::TEMPLATE_CLASSES.into_iter().collect();

    assert_eq!(reachable, declared, "§12.4's eight, and only those");
    assert_eq!(declared.len(), 8, "§12.4 names eight");

    // And the labels are distinct, because they key a metrics histogram.
    let labels: BTreeSet<&str> = declared.iter().map(|c| c.label()).collect();
    assert_eq!(labels.len(), 8, "two classes share a label");
}

/// The two block-level kinds are deliberately **not** template classes.
///
/// A sealed block does move pool state — it just does not say which pools. §12.1
/// revalues the templates touching an event's pools, so a `Block` with no pool
/// set can only trigger broad discovery, which is the slow path by construction.
#[test]
fn a_block_names_no_pools_and_is_not_a_template_class() {
    assert_eq!(EventKind::Block.class(), None);
    assert_eq!(EventKind::Preconfirmation.class(), None);

    for kind in [EventKind::Block, EventKind::Preconfirmation] {
        let ev = event(kind);
        assert!(
            ev.touched_pools().is_empty(),
            "a block moves pools without saying which; answering 'all of them' \
             would make every sealed block a full rebuild"
        );
    }
}

/// Every template-class event names the pools it moved. That is the frontier's
/// revaluation key, so a class that returned nothing would be a class Engine D
/// could never act on.
#[test]
fn every_template_class_names_the_pools_it_moved() {
    for kind in every_kind() {
        let Some(class) = kind.class() else { continue };
        let ev = event(kind);
        assert!(
            !ev.touched_pools().is_empty(),
            "{class:?} named no pools, so nothing can be revalued for it"
        );
    }
}

/// The recorded stream is data on disk, so the wire form has to round-trip.
/// Task 8.5 replaces the fixture with a real capture; a schema that only works
/// in one direction would make that a rewrite.
#[test]
fn every_kind_round_trips_through_the_wire_form() {
    for kind in every_kind() {
        let ev = event(kind);
        let json = serde_json::to_string(&ev).expect("serialise");
        let back: StateEvent = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(ev, back);
    }
}

fn event(kind: EventKind) -> StateEvent {
    StateEvent {
        chain: BASE,
        at: Ordinal::confirmed(47_079_437, 12, 0),
        observed_at: UnixNanos(1_781_049_614_240_000_000),
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
