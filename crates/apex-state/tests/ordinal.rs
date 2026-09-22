//! §5.2 ordering. Field order in `Ordinal` IS the comparison order, so these
//! tests are what stops a well-meaning reorder from silently redefining
//! "earlier".

use apex_state::Ordinal;

#[test]
fn the_legacy_block_ordering_is_preserved_among_confirmed_observations() {
    // What continuity.rs guaranteed before flashblock fields existed: block,
    // then tx_index, then log_index.
    let a = Ordinal::confirmed(100, 5, 1);
    let b = Ordinal::confirmed(100, 5, 2);
    let c = Ordinal::confirmed(100, 6, 0);
    let d = Ordinal::confirmed(101, 0, 0);

    assert!(a < b && b < c && c < d);
}

#[test]
fn a_later_flashblock_orders_after_an_earlier_one_regardless_of_tx_index() {
    // The reason flashblock_index sits ABOVE tx_index: within one payload the
    // tx order is meaningful, but across payloads the payload order dominates.
    let early = Ordinal::preconfirmed(7, 1, 100, 99, 0);
    let late = Ordinal::preconfirmed(7, 2, 100, 0, 0);

    assert!(early < late, "flashblock 1 tx 99 must precede flashblock 2 tx 0");
}

#[test]
fn an_anchor_read_sorts_at_the_end_of_its_block() {
    // An eth_call at HEAD has no position in the log stream. Placing it last is
    // what stops a delta being applied on top of an anchor that already
    // contained it -- the TOCTOU the legacy anchor path had to fix.
    let anchor = Ordinal::end_of_block(100);

    assert!(Ordinal::confirmed(100, u64::MAX - 1, 0) < anchor);
    assert!(Ordinal::preconfirmed(9, 9, 100, 9, 9) < anchor);
    assert!(anchor < Ordinal::confirmed(101, 0, 0), "but still within its own block");
}

#[test]
fn confirmed_and_preconfirmed_are_distinguishable() {
    assert!(!Ordinal::confirmed(1, 0, 0).is_preconfirmed());
    assert!(Ordinal::preconfirmed(7, 0, 1, 0, 0).is_preconfirmed());
    assert!(!Ordinal::end_of_block(1).is_preconfirmed(), "an anchor is not a preconfirmation");
}

#[test]
fn ordering_is_total_and_survives_a_round_trip() {
    let mut xs = vec![
        Ordinal::confirmed(101, 0, 0),
        Ordinal::end_of_block(100),
        Ordinal::preconfirmed(7, 2, 100, 0, 0),
        Ordinal::confirmed(100, 5, 1),
        Ordinal::preconfirmed(7, 1, 100, 0, 0),
    ];
    xs.sort_unstable();

    for w in xs.windows(2) {
        assert!(w[0] <= w[1]);
    }
    let json = serde_json::to_string(&xs).unwrap();
    let back: Vec<Ordinal> = serde_json::from_str(&json).unwrap();
    assert_eq!(xs, back);
}
