//! Task 8.5 R32 — `apex send-probe`: the transaction it signs, and how a
//! send's inclusion is timed against Base's flashblock stream.

use alloy_primitives::address;
use apex_chain::base::flashblock_stream::FlashblockSeen;
use apex_runtime::send_probe::{inclusion, probe_tx, Inclusion};

const TRADER: alloy_primitives::Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");

/// A zero-value self-transfer: 21,000 gas, no data, the tip asked, and the
/// conventional twice-the-base-fee headroom on top.
#[test]
fn the_probe_is_a_self_transfer_of_nothing() {
    let tx = probe_tx(8453, 12, 5_000_000, 1_000_000, TRADER);
    assert_eq!((tx.chain_id, tx.nonce, tx.to, tx.gas_limit), (8453, 12, TRADER, 21_000));
    assert!(tx.data.is_empty(), "no calldata: a transfer, and the signer carries no value");
    assert_eq!((tx.max_priority_fee_per_gas, tx.max_fee_per_gas), (1_000_000, 11_000_000));
}

fn fb(block: u64, index: u32, first_tx: Option<u32>, tx_count: u32, arrived_ms: u64) -> FlashblockSeen {
    FlashblockSeen { block, index, arrived_at_ns: arrived_ms * 1_000_000, first_tx, tx_count, block_timestamp: None }
}

/// The flashblock that carried the transaction, how long after the send it
/// reached us, and when the one before it did: the window the send missed.
#[test]
fn a_send_is_timed_to_the_flashblock_that_carried_it() {
    let seen = [fb(100, 0, Some(0), 1, 1_000), fb(100, 1, Some(1), 40, 1_200), fb(100, 2, Some(41), 30, 1_400), fb(101, 0, Some(0), 1, 3_000)];
    assert_eq!(
        inclusion(&seen, 100, 50, 900_000_000),
        Some(Inclusion { flashblock: 2, arrived_after_ms: 500, before_arrived_after_ms: Some(300) })
    );
    assert_eq!(
        inclusion(&seen, 100, 0, 950_000_000),
        Some(Inclusion { flashblock: 0, arrived_after_ms: 50, before_arrived_after_ms: None }),
        "a block's first flashblock has none before it"
    );
    assert_eq!(inclusion(&seen, 100, 71, 900_000_000), None, "beyond what was seen");
    assert_eq!(inclusion(&[fb(100, 3, None, 9, 1_600)], 100, 2, 900_000_000), None, "a block not seen whole");
}
