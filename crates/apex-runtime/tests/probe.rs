//! R25: the region probe's arithmetic.

use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockSeen, Frame};
use apex_runtime::probe::{arrival_offsets_ms, pending_log_delays_ms, percentiles};

/// Index 0 of a block stamped 1,000 s opens at 998 s; one reaching us at
/// 998.128 s arrived 128 ms after its slot opened. Other indexes don't count.
#[test]
fn the_arrival_offset_is_measured_from_the_slot_opening() {
    let seen = |index, at_ms: u64, ts| FlashblockSeen {
        block: 1,
        index,
        arrived_at_ns: at_ms * 1_000_000,
        first_tx: Some(0),
        tx_count: 1,
        block_timestamp: ts,
    };
    let v = arrival_offsets_ms(&[seen(0, 998_128, Some(1_000)), seen(1, 998_300, None), seen(0, 1_000_131, Some(1_002))]);
    assert_eq!(v, vec![128, 131]);
}

/// A log is matched to the flashblock that added its transaction index.
#[test]
fn a_pending_log_is_timed_against_the_flashblock_that_carried_it() {
    let mut ix = FlashblockIndexer::new();
    let f = |index, n| Frame { block: 7, index, transactions: n, block_timestamp: None };
    ix.observe(&f(0, 1), 1_000_000_000);
    ix.observe(&f(1, 10), 1_200_000_000);
    // tx 5 rode index 1 (txs 1..=10), arriving 45 ms after it; tx 99 matches nothing.
    let d = pending_log_delays_ms(&ix, &[(7, 5, 1_245_000_000), (7, 99, 1_300_000_000)]);
    assert_eq!(d, vec![45]);
}

#[test]
fn percentiles_are_nearest_rank() {
    let mut v: Vec<i64> = (1..=100).collect();
    assert_eq!(percentiles(&mut v), Some((10, 50, 90)));
    // Rank ceil(n·p/100): of seven, p50 is the 4th and p90 the 7th.
    let mut seven = vec![7, 3, 1, 5, 2, 6, 4];
    assert_eq!(percentiles(&mut seven), Some((1, 4, 7)));
    assert_eq!(percentiles(&mut Vec::new()), None);
}
