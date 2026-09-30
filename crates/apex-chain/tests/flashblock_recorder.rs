//! Task 8.5 R5 — the capacity model's observations, from `newFlashblocks`.
//!
//! Two whole blocks recorded from BlockPI 2026-10-01 (52,002,569 and
//! 52,002,570) set the shapes: eleven flashblocks, the first holding only the
//! L1-info deposit at 46,230 gas, cumulative gas and transactions rising from
//! there. Every refusal the recorder makes is one of those shapes broken.

mod support;

use apex_chain::base::flashblock::{sample, Capacity, FlashblockRecorder, MeasuredCapacityModel};
use apex_chain::rpc::ws::{parse_notification, Flashblock, Kind, Notification, WsSettings};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use support::ws::{Push, Script, WsNode};

/// Cumulative gas and transactions through each flashblock, as recorded.
const B569: [(u64, usize); 11] = [
    (46_230, 1),
    (3_854_766, 27),
    (5_132_969, 35),
    (21_936_726, 45),
    (22_452_210, 50),
    (24_035_868, 60),
    (24_875_356, 65),
    (25_897_731, 73),
    (26_582_125, 80),
    (28_366_420, 91),
    (31_946_781, 113),
];
const B570: [(u64, usize); 11] = [
    (46_230, 1),
    (14_875_217, 50),
    (14_966_646, 52),
    (15_493_610, 59),
    (16_993_517, 72),
    (18_732_018, 79),
    (19_137_742, 82),
    (20_125_662, 89),
    (21_006_183, 97),
    (21_315_182, 100),
    (35_464_897, 126),
];

fn fb(number: u64, (gas_used, transactions): (u64, usize)) -> Flashblock {
    Flashblock { number, gas_used, gas_limit: 400_000_000, transactions, deposits: 1 }
}

fn feed(rec: &mut FlashblockRecorder, number: u64, shape: &[(u64, usize)]) {
    for s in shape {
        rec.observe(&fb(number, *s));
    }
}

/// Close whatever block is being built by starting the next one.
fn close(rec: &mut FlashblockRecorder, next: u64) {
    rec.observe(&fb(next, B569[0]));
}

// ------------------------------------------------------------------ parsing

/// The payload's transactions are full objects; deposits are type `0x7e`.
#[test]
fn a_flashblock_notification_parses_with_its_deposits() {
    let ids = BTreeMap::from([("0xa".to_string(), Kind::NewFlashblocks)]);
    let msg = |txs: serde_json::Value| {
        json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xa","result":{
            "number":"0x3197d29","gasUsed":"0x2ab60e1","gasLimit":"0x17d78400","hash":"0x0","transactions":txs}}})
    };
    let n = parse_notification(&msg(json!([{"type":"0x7e"},{"type":"0x2"},{"type":"0x2"}])), &ids);
    assert_eq!(
        n,
        Some(Notification::Flashblock(Flashblock {
            number: 52_002_089,
            gas_used: 44_785_889,
            gas_limit: 400_000_000,
            transactions: 3,
            deposits: 1,
        }))
    );

    // Hashes carry no type: no deposits, so no block anchors on them.
    match parse_notification(&msg(json!(["0x01", "0x02"])), &ids) {
        Some(Notification::Flashblock(f)) => assert_eq!((f.transactions, f.deposits), (2, 0)),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------------ the recorder

/// **A whole block is its eleven cumulative gas figures, at their positions.**
#[test]
fn whole_blocks_become_observations_at_their_positions() {
    let mut rec = FlashblockRecorder::new(FlashblockRecorder::DEFAULT_WINDOW);
    feed(&mut rec, 569, &B569);
    feed(&mut rec, 570, &B570);
    close(&mut rec, 571);

    assert_eq!((rec.blocks(), rec.refused()), (2, 0));
    let obs = rec.observations();
    assert_eq!(obs.len(), 22);
    for (i, (gas, _)) in B570.iter().enumerate() {
        let o = obs.iter().find(|o| o.block == 570 && o.index == i as u32).unwrap();
        assert_eq!(o.cumulative_gas_budget, *gas, "index {i}");
    }
}

/// A block first seen at anything but its deposit-only flashblock was joined
/// mid-way: its positions are not its indexes.
#[test]
fn a_block_joined_mid_way_is_refused() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 569, &B569[3..]);
    feed(&mut rec, 570, &B570);
    close(&mut rec, 571);
    assert_eq!((rec.blocks(), rec.refused()), (1, 1));
    assert!(rec.observations().iter().all(|o| o.block == 570));

    // No transactions is no deposit: nothing shows this is index 0.
    let mut rec = FlashblockRecorder::new(10);
    rec.observe(&Flashblock { deposits: 0, transactions: 0, ..fb(569, B569[0]) });
    feed(&mut rec, 569, &B569[1..]);
    close(&mut rec, 570);
    assert_eq!((rec.blocks(), rec.refused()), (0, 1));
}

/// Within a block gas and transactions only grow; a block where either shrinks
/// is not one block's flashblocks in order.
#[test]
fn a_block_that_goes_backwards_is_refused() {
    for broken in [
        [B569[0], B569[1], B569[3], (B569[2].0, B569[3].1 + 1)], // gas falls, transactions rise
        [B569[0], B569[1], (B569[2].0, B569[1].1 - 1), B569[3]], // transactions fall, gas rises
    ] {
        let mut rec = FlashblockRecorder::new(10);
        feed(&mut rec, 569, &broken);
        close(&mut rec, 570);
        assert_eq!((rec.blocks(), rec.refused()), (0, 1), "{broken:?}");
    }
}

/// **A block missing a flashblock stays out of the model.** Its later
/// flashblocks would sit one index early, carrying more gas than that index
/// had — capacity overstated.
#[test]
fn a_block_missing_a_flashblock_is_left_out_of_the_model() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 569, &B569);
    let mut short = B570.to_vec();
    short.remove(4);
    feed(&mut rec, 570, &short);
    feed(&mut rec, 571, &B569);
    close(&mut rec, 572);
    assert_eq!(rec.blocks(), 3, "anchored and in order, so held");
    assert!(rec.observations().iter().all(|o| o.block != 570), "but not used");
}

/// As many short blocks as full ones: the tie goes to the longer count, since a
/// short block may be a full one that lost a notification.
#[test]
fn a_tie_goes_to_the_longer_count() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 569, &B569);
    feed(&mut rec, 570, &B570[..10]);
    close(&mut rec, 571);
    assert!(rec.observations().iter().all(|o| o.block == 569));
}

/// One duplicated notification makes a twelve-flashblock block. Against the
/// maximum count it would refuse every normal block in the window; the usual
/// count ignores it instead.
#[test]
fn one_duplicate_does_not_refuse_every_normal_block() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 569, &B569);
    let mut doubled = B570.to_vec();
    doubled.insert(5, B570[5]);
    feed(&mut rec, 570, &doubled);
    feed(&mut rec, 571, &B569);
    close(&mut rec, 572);
    let blocks: std::collections::BTreeSet<u64> = rec.observations().iter().map(|o| o.block).collect();
    assert_eq!(blocks, [569, 571].into());
}

/// Continuity lost mid-block: the block may be missing flashblocks, so it is
/// refused — once, though its remainder, which cannot anchor, is refused too.
#[test]
fn a_reset_refuses_the_block_being_built() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 569, &B569[..5]);
    rec.reset();
    feed(&mut rec, 569, &B569[5..]);
    close(&mut rec, 570);
    assert_eq!((rec.blocks(), rec.refused()), (0, 1));
}

/// An older block's flashblock arriving after a newer one's is not placed.
#[test]
fn an_older_blocks_flashblock_is_not_placed() {
    let mut rec = FlashblockRecorder::new(10);
    feed(&mut rec, 570, &B570[..6]);
    rec.observe(&fb(569, B569[10]));
    feed(&mut rec, 570, &B570[6..]);
    close(&mut rec, 571);
    assert_eq!((rec.blocks(), rec.refused()), (1, 0));
}

#[test]
fn the_window_keeps_the_most_recent_blocks() {
    let mut rec = FlashblockRecorder::new(2);
    for n in 569..572 {
        feed(&mut rec, n, &B569);
    }
    close(&mut rec, 572);
    let blocks: std::collections::BTreeSet<u64> = rec.observations().iter().map(|o| o.block).collect();
    assert_eq!(blocks, [570, 571].into());
}

/// **The model is built from use**, per index, and an index with too few
/// samples says so rather than borrowing a neighbour's (§5.6).
#[test]
fn the_model_is_measured_from_use_and_honest_about_gaps() {
    let mut rec = FlashblockRecorder::new(FlashblockRecorder::DEFAULT_WINDOW);
    assert!(rec.model().is_err(), "no observations, no model");

    let blocks = MeasuredCapacityModel::DEFAULT_MIN_SAMPLES as u64;
    for n in 0..blocks - 1 {
        feed(&mut rec, 1_000 + n, if n % 2 == 0 { &B569 } else { &B570 });
    }
    close(&mut rec, 2_000);
    let thin = rec.model().unwrap();
    assert!(matches!(thin.capacity_at(1), Some(Capacity::Unknown { .. })), "one sample short");

    // `close` opened block 2,000 at its deposit; the rest of it.
    feed(&mut rec, 2_000, &B570[1..]);
    close(&mut rec, 2_001);
    let q = rec.model().unwrap();
    assert_eq!(q.windows(), 11);
    // Index 0 is the deposit alone in every block; the last index carries the
    // whole block's use — p10 of the recorded blocks' totals.
    assert_eq!(q.q(0), Some(46_230));
    assert_eq!(q.q(10), Some(31_946_781));
    // Cumulative, so never smaller at a later index.
    for k in 1..11 {
        assert!(q.q(k) >= q.q(k - 1), "Q({k}) < Q({})", k - 1);
    }
}

// ------------------------------------------------------------------ sampling

fn push(number: u64, (gas, txs): (u64, usize)) -> Push {
    let mut t = vec![json!({"type": "0x7e"})];
    t.extend((1..txs).map(|_| json!({"type": "0x2"})));
    Push {
        subscription: 1,
        result: json!({
            "number": format!("{number:#x}"),
            "gasUsed": format!("{gas:#x}"),
            "gasLimit": "0x17d78400",
            "hash": format!("0x{}", "0".repeat(64)),
            "transactions": t,
        }),
    }
}

/// **A sample subscribes to `newFlashblocks` alone, on its own connection, and
/// records what arrives.** The block cut off at the end of the sample is
/// refused, not closed.
#[tokio::test]
async fn a_sample_records_whole_blocks_and_refuses_the_cut_off_one() {
    let mut pushes: Vec<Push> = B569.iter().map(|s| push(569, *s)).collect();
    pushes.extend(B570.iter().map(|s| push(570, *s)));
    pushes.extend(B569[..4].iter().map(|s| push(571, *s)));
    let node = WsNode::start(vec![Script { pushes, subscriptions: Some(1), ..Default::default() }]).await;

    let mut rec = FlashblockRecorder::new(10);
    let seen = sample(&node.url("/key"), WsSettings::default(), Duration::from_millis(600), &mut rec)
        .await
        .expect("a websocket url");

    assert_eq!(seen, 26);
    assert_eq!((rec.blocks(), rec.refused()), (2, 1));
    assert_eq!(node.connections(), 1);
    let subscribed: Vec<_> = node.requests().iter().map(|r| r["params"].clone()).collect();
    assert_eq!(subscribed, vec![json!(["newFlashblocks"])]);
}

/// A reconnect mid-sample loses whatever arrived while it was down, so the
/// block it cut through is refused — even when, as here, nothing was lost:
/// the recorder cannot know that.
#[tokio::test]
async fn a_reconnect_during_a_sample_refuses_the_block_it_cut() {
    let first: Vec<Push> = B569.iter().map(|s| push(569, *s)).chain(B570[..5].iter().map(|s| push(570, *s))).collect();
    let second: Vec<Push> = B570[5..]
        .iter()
        .map(|s| push(570, *s))
        .chain(B569.iter().map(|s| push(571, *s)))
        .chain(std::iter::once(push(572, B569[0])))
        .collect();
    let node = WsNode::start(vec![
        Script { pushes: first, close_after: true, subscriptions: Some(1), ..Default::default() },
        Script { pushes: second, subscriptions: Some(1), ..Default::default() },
    ])
    .await;

    let mut rec = FlashblockRecorder::new(10);
    sample(&node.url("/key"), WsSettings::default(), Duration::from_millis(1_500), &mut rec).await.unwrap();

    assert_eq!(node.connections(), 2, "it reconnected");
    let blocks: std::collections::BTreeSet<u64> = rec.observations().iter().map(|o| o.block).collect();
    assert_eq!(blocks, [569, 571].into(), "570 was cut by the reconnect");
    assert_eq!(rec.refused(), 2, "570, and 572 cut off by the sample's end");
}
