//! R26: judging an arm against what the chain did.

use apex_runtime::armed::eval::{
    bid_wei_per_gas, judge, landing, merge_renewals, net_wei, next_flashblock, own_landing, Landing, Logged, Point, TxEffect, Write, F_PERMILLE,
};
use apex_runtime::armed::lines::{Arm, ArmedLine, Renewal, StartAt, Word};
use apex_runtime::armed::predicate::{expiry, Op, Predicate};
use ethers_core::types::U256;

const A: &str = "0x00000000000000000000000000000000000000aa";
const B: &str = "0x00000000000000000000000000000000000000bb";

fn storage(addr: &str, slot: &str, op: Op, v: u64) -> Predicate {
    Predicate::Storage { address: addr.into(), slot: slot.into(), mask: None, op, value: format!("{v:#x}") }
}

fn arm() -> Arm {
    Arm {
        arm_id: 7,
        at_ns: 0,
        armed_block: 100,
        to_block: 105,
        route_hash: "0xroute".into(),
        trigger_leg: 0,
        rung: 1,
        step: 10,
        pools: [A.into(), B.into()],
        venues: ["UniswapV3".into(), "UniswapV3".into()],
        // A's price at least 50; B between 9 and 11 with liquidity at least 900.
        predicates: vec![
            storage(A, "0x0", Op::AtLeast, 50),
            storage(B, "0x0", Op::AtLeast, 9),
            storage(B, "0x0", Op::AtMost, 11),
            storage(B, "0x4", Op::AtLeast, 900),
            expiry(105),
        ],
        start_words: vec![
            Word { address: A.into(), slot: "0x0".into(), value: "0x28".into() }, // 40
            Word { address: B.into(), slot: "0x0".into(), value: "0xa".into() },  // 10
            Word { address: B.into(), slot: "0x4".into(), value: "0x3e8".into() }, // 1000
        ],
        start_ticks: vec![(B.into(), 0)],
        size_wei: "1000".into(),
        mid: "10".into(),
        output_wei: "1100".into(),
        surplus_wei: "90".into(),
        cost_wei: "10".into(),
        gas_expected: 300_000,
        gas_limit: 400_000,
        base_fee_wei: "5000000".into(),
        other_wei: "1".into(),
        deadline: 0,
        calldata: "0x".into(),
        calldata_hash: "0x".into(),
        sender_slot: 0,
        lender: "balancer".into(),
        bid_wei_per_gas: String::new(),
        p_win: 0.0,
        renewals: Vec::new(),
        start: None,
        executor: None,
        start_per_eth: None,
        bid_model: None,
        alt_bid_wei_per_gas: None,
        alt_p_win: None,
        model_version: None,
        start_at: Vec::new(),
    }
}

fn set(addr: &str, slot: u64, v: u64) -> Write {
    Write::Set { address: addr.into(), slot: U256::from(slot), value: U256::from(v) }
}

/// A write as its log: the pool it is a write to (a Liquidity's `pool`, else
/// its address), at `log_index`.
fn logged(log_index: u64, write: Write) -> Logged {
    let pool = match &write {
        Write::Liquidity { pool, .. } => pool.clone(),
        Write::Set { address, .. } | Write::Tick { address, .. } => address.clone(),
    };
    Logged { log_index, pool, write }
}

/// Transaction `i`'s writes, its logs numbered from `100 i`.
fn tx(block: u64, i: u64, writes: Vec<Write>) -> TxEffect {
    let writes = writes.into_iter().enumerate().map(|(k, w)| logged(i * 100 + k as u64, w)).collect();
    TxEffect { block, tx_index: i, tx: format!("0x{block}{i}"), writes }
}

fn tx_at(block: u64, i: u64, writes: Vec<Logged>) -> TxEffect {
    TxEffect { block, tx_index: i, tx: format!("0x{block}{i}"), writes }
}

fn burn(delta: i128) -> Write {
    Write::Liquidity { address: B.into(), slot: U256::from(4), pool: B.into(), lower: -10, upper: 10, delta }
}

/// The arm's start words already hold a burn of 60 on B: 940 of liquidity,
/// over the 900 floor; the same burn counted twice is 880, under it.
fn arm_after_burn(start_at: Vec<StartAt>) -> Arm {
    let mut a = arm();
    a.start_words[2].value = "0x3ac".into(); // 940
    a.start_at = start_at;
    a
}

#[test]
fn an_arm_fires_on_the_first_transaction_that_makes_every_predicate_hold() {
    let effects = vec![
        tx(100, 3, vec![set(A, 0, 60)]),         // the arming block itself: not watched
        tx(101, 5, vec![set(A, 0, 45)]),         // not yet
        tx(102, 7, vec![set(A, 0, 55)]),         // fires
        tx(102, 8, vec![set(B, 0, 10)]),         // still holds
        tx(103, 1, vec![set(A, 0, 41)]),         // the rival: closes it
    ];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.fire, Some(Point { block: 102, tx_index: 7, tx: "0x1027".into() }));
    assert_eq!(j.rival, Some(Point { block: 103, tx_index: 1, tx: "0x1031".into() }));
    assert!(j.trigger_met);
}

#[test]
fn the_other_pools_band_and_liquidity_gate_the_fire() {
    let effects = vec![tx(101, 1, vec![set(B, 0, 12)]), tx(101, 2, vec![set(A, 0, 55)])];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.fire, None, "B left its band");
    assert!(j.trigger_met, "A reached its trigger: the band, not the trigger, missed");
    // A burn in range takes B's liquidity under its floor.
    let effects = vec![
        tx(101, 1, vec![Write::Liquidity { address: B.into(), slot: U256::from(4), pool: B.into(), lower: -10, upper: 10, delta: -200 }]),
        tx(101, 2, vec![set(A, 0, 55)]),
    ];
    assert_eq!(judge(&arm(), &effects).unwrap().fire, None, "1000 − 200 < 900");
    // Out of range, it does not count: above the tick, or ending at it (a
    // position holds [lower, upper)).
    for (lower, upper) in [(20, 30), (-10, 0)] {
        let effects = vec![
            tx(101, 1, vec![Write::Liquidity { address: B.into(), slot: U256::from(4), pool: B.into(), lower, upper, delta: -200 }]),
            tx(101, 2, vec![set(A, 0, 55)]),
        ];
        assert!(judge(&arm(), &effects).unwrap().fire.is_some(), "[{lower}, {upper}) does not hold tick 0");
    }
    // Starting at it, it does.
    let effects = vec![
        tx(101, 1, vec![Write::Liquidity { address: B.into(), slot: U256::from(4), pool: B.into(), lower: 0, upper: 10, delta: -200 }]),
        tx(101, 2, vec![set(A, 0, 55)]),
    ];
    assert_eq!(judge(&arm(), &effects).unwrap().fire, None, "[0, 10) holds tick 0");
}

#[test]
fn nothing_after_the_window_counts() {
    let effects = vec![tx(101, 1, vec![set(A, 0, 49)]), tx(106, 1, vec![set(A, 0, 55)])];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.fire, None);
    assert!(!j.trigger_met, "49 < 50 in the window; 55 only after it");
    // The window's last block is in it.
    let effects = vec![tx(105, 9, vec![set(A, 0, 55)])];
    assert_eq!(judge(&arm(), &effects).unwrap().fire.map(|p| p.block), Some(105));
}

#[test]
fn renewals_extend_an_arm_and_its_block_bound() {
    let lines = vec![
        ArmedLine::Arm(Box::new(arm())),
        ArmedLine::Renew { arm_id: 7, to_block: 110, at_ns: 1, deadline: Some(40), calldata: Some("0xrenewed".into()) },
        // A line written before renewals carried their own transaction.
        ArmedLine::Renew { arm_id: 7, to_block: 115, at_ns: 2, deadline: None, calldata: None },
    ];
    let merged = merge_renewals(lines);
    let a = &merged[0];
    assert_eq!(a.to_block, 115);
    assert!(a.predicates.contains(&expiry(115)));
    assert!(!a.predicates.contains(&expiry(105)));
    // A renewal is its own transaction, re-signed with a fresh deadline: valid
    // after the window before it, and no further back.
    assert_eq!(a.renewals, vec![Renewal { after_block: 105, to_block: 110, deadline: 40, calldata: "0xrenewed".into() }]);
    assert_eq!((a.calldata_at(101), a.calldata_at(105)), ("0x", "0x"), "the arm's own in its window");
    assert_eq!((a.calldata_at(106), a.calldata_at(115)), ("0xrenewed", "0xrenewed"));
}

#[test]
fn landing_follows_the_flashblock_rule() {
    let k = (102, 4);
    assert_eq!(next_flashblock((102, 10)), (103, 1));
    // No rival: won either way.
    let l = landing(k, false, None, 1, 0);
    assert!(l.optimistic && l.pessimistic);
    // Rival in the trigger's flashblock: optimistic needs the higher bid; pessimistic loses.
    let l = landing(k, false, Some((102, 4)), 10, 5);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, false, Some((102, 4)), 5, 10);
    assert!(!l.optimistic && !l.pessimistic);
    // Rival in the next: optimistic wins outright; pessimistic needs the higher bid.
    let l = landing(k, false, Some((102, 5)), 5, 10);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, false, Some((102, 5)), 10, 5);
    assert!(l.optimistic && l.pessimistic);
    // Rival later still: won either way.
    let l = landing(k, false, Some((102, 7)), 0, 1_000);
    assert!(l.optimistic && l.pessimistic);
}

/// A trigger that is its flashblock's last transaction cannot be followed in
/// it: the contest is in the next flashblock, by priority fee. Live arm 3114
/// (2026-10-29): trigger at index 7 closing flashblock 1, our 0.005 gwei
/// expired, the rival's 0.053 gwei landed at index 9 in flashblock 2. The
/// old rule called that a win by position.
#[test]
fn a_trigger_that_closes_its_flashblock_is_contested_in_the_next() {
    let k = (102, 4);
    let l = landing(k, true, Some((102, 5)), 5_000_001, 53_150_146);
    assert!(!l.optimistic && !l.pessimistic, "outbid in the next flashblock");
    let l = landing(k, true, Some((102, 5)), 60_000_000, 53_150_146);
    assert!(l.optimistic && !l.pessimistic, "the higher bid wins there; a flashblock later still loses");
    let l = landing(k, true, Some((102, 6)), 0, 1_000);
    assert!(l.optimistic && !l.pessimistic, "a rival two flashblocks on loses to us in the next, not one after");
    let l = landing(k, true, None, 0, 0);
    assert!(l.optimistic && l.pessimistic);
    let l = landing((102, 10), true, Some((103, 1)), 1, 2);
    assert!(!l.optimistic, "the block's last flashblock is followed by the next block's first");
}

/// The rival was our own send: it landed at the bid we sent, so every bid at
/// least as high lands; a lower one is not known to, and counts as lost.
#[test]
fn our_own_landing_wins_at_the_bid_sent_and_above() {
    assert_eq!(own_landing(5_000_001, 5_000_001), Landing { optimistic: true, pessimistic: true });
    assert_eq!(own_landing(9_000_000, 5_000_001), Landing { optimistic: true, pessimistic: true });
    assert_eq!(own_landing(5_000_000, 5_000_001), Landing { optimistic: false, pessimistic: false });
}

#[test]
fn bids_and_nets_are_exact() {
    assert_eq!(F_PERMILLE, [50, 100, 200, 300, 500, 700, 900]);
    // 0.5 × 3e14 wei over 3e5 gas = 5e8 wei per gas.
    assert_eq!(bid_wei_per_gas(U256::from(300_000_000_000_000u64), 300_000, 500), 500_000_000);
    // gross 1e15 − 3e5 × (5e6 + 5e8) − 1e10
    assert_eq!(net_wei(U256::from(10u64.pow(15)), 300_000, 5_000_000, 500_000_000, 10_000_000_000), 848_490_000_000_000);
}

// ------------------------------------------------------------------ R41: start_at

/// **A start point ahead of the arming block is not replayed twice.** The
/// book had already applied a burn at (101, log 3) when the arm was taken at
/// head 100: replaying it again on top of words that hold it would invent a
/// breach of the liquidity floor.
#[test]
fn a_start_point_ahead_of_the_armed_block_is_not_replayed_twice() {
    let start_at = vec![
        StartAt { pool: A.into(), block: 100, log_index: None },
        StartAt { pool: B.into(), block: 101, log_index: Some(3) },
    ];
    let effects = vec![tx_at(101, 1, vec![logged(3, burn(-60))]), tx_at(101, 2, vec![logged(5, set(A, 0, 55))])];
    let j = judge(&arm_after_burn(start_at), &effects).unwrap();
    assert_eq!(j.fire, Some(Point { block: 101, tx_index: 2, tx: "0x1012".into() }));
    assert_eq!(j.rival, None);
}

/// Arms from before `start_at` are judged exactly as before.
#[test]
fn without_start_at_the_answer_is_today_s() {
    let effects = vec![tx_at(101, 1, vec![logged(3, burn(-60))]), tx_at(101, 2, vec![logged(5, set(A, 0, 55))])];
    assert_eq!(judge(&arm_after_burn(Vec::new()), &effects).unwrap().fire, None, "940 − 60 < 900");
}

/// A pool read at a block holds every write of that block, and none after it.
#[test]
fn a_read_start_point_skips_its_whole_block() {
    let start_at = vec![StartAt { pool: B.into(), block: 101, log_index: None }];
    let effects = vec![
        tx_at(101, 4, vec![logged(9, burn(-60))]),
        tx_at(102, 1, vec![logged(2, set(A, 0, 55))]),
        tx_at(103, 1, vec![logged(1, burn(-60))]),
    ];
    let j = judge(&arm_after_burn(start_at), &effects).unwrap();
    assert_eq!(j.fire, Some(Point { block: 102, tx_index: 1, tx: "0x1021".into() }));
    assert_eq!(j.rival, Some(Point { block: 103, tx_index: 1, tx: "0x1031".into() }), "102's burn is applied");
}

// ------------------------------------------------------------------ R41: blocks

use apex_runtime::armed::eval::{land, landing_in_blocks, Placement};
use apex_runtime::armed::run::{settle_blocks, unit_of, OffsetUnit};

const BOTH: Landing = Landing { optimistic: true, pessimistic: true };

/// Nothing follows a block's last transaction in it: a trigger that closes
/// its block is contested in the next, by priority fee.
#[test]
fn in_blocks_a_trigger_that_closes_its_block_is_contested_in_the_next() {
    assert!(!landing_in_blocks(101, true, Some(102), 5, 9).optimistic, "outbid in the next block");
    assert!(landing_in_blocks(101, true, Some(102), 9, 5).optimistic, "our bid higher");
    assert!(!landing_in_blocks(101, true, Some(101), 9, 5).optimistic, "a rival in the trigger's own block is before us");
}

/// On Denim a validity transaction can follow its trigger in the trigger's
/// own block: a rival there is a fee contest.
#[test]
fn in_blocks_a_rival_after_the_trigger_in_its_own_block_is_a_fee_contest() {
    assert!(!landing_in_blocks(101, false, Some(101), 5, 9).optimistic);
    assert!(landing_in_blocks(101, false, Some(101), 9, 5).optimistic);
}

#[test]
fn in_blocks_a_rival_in_a_later_block_loses() {
    assert_eq!(landing_in_blocks(101, false, Some(102), 1, 9), Landing { optimistic: true, pessimistic: false });
    assert_eq!(landing_in_blocks(101, false, Some(103), 1, 9), BOTH);
    assert_eq!(landing_in_blocks(101, false, None, 1, 9), BOTH);
}

#[test]
fn our_own_landing_in_block_mode_wins_at_the_bid_sent_and_above() {
    let p = Placement::Blocks { fire: 101, closes: false, rival: Some(101) };
    assert_eq!(land(&p, true, 7, 7), BOTH);
    assert_eq!(land(&p, true, 6, 7), Landing { optimistic: false, pessimistic: false });
    // Placed in flashblocks, the rule is the flashblock rule's.
    let f = Placement::Flashblocks { fire: (101, 3), closes: false, rival: Some((101, 3)) };
    assert_eq!(land(&f, false, 9, 5), landing((101, 3), false, Some((101, 3)), 9, 5));
}

/// The rival is the first transaction after the fire to break a predicate:
/// one before the trigger in its block never counts.
#[test]
fn a_write_before_the_fire_in_its_block_is_never_the_rival() {
    let effects = vec![
        tx(101, 1, vec![set(B, 0, 12)]), // B out of its band, before the trigger
        tx(101, 2, vec![set(A, 0, 55)]), // the trigger: B still out
        tx(101, 3, vec![set(B, 0, 10)]), // B back: fires
    ];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.fire, Some(Point { block: 101, tx_index: 3, tx: "0x1013".into() }));
    assert_eq!(j.rival, None);
}

#[test]
fn of_two_rivals_after_the_fire_the_first_decides() {
    let effects = vec![tx(102, 7, vec![set(A, 0, 55)]), tx(102, 9, vec![set(A, 0, 41)]), tx(103, 1, vec![set(B, 0, 12)])];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.rival, Some(Point { block: 102, tx_index: 9, tx: "0x1029".into() }));
}

/// A minute in the configuration's blocks.
#[test]
fn settle_is_a_minute_in_the_config_s_blocks() {
    assert_eq!((settle_blocks(2_000), settle_blocks(200)), (30, 300));
}

/// The fire block's own time says which era's blocks the offsets count.
#[test]
fn the_unit_is_the_fire_block_s_era() {
    assert_eq!(unit_of(false, Some(1_793_901_599)), Some(OffsetUnit::Block2s));
    assert_eq!(unit_of(false, Some(1_793_901_600)), Some(OffsetUnit::Block200ms));
    assert_eq!(unit_of(true, Some(1_793_901_600)), Some(OffsetUnit::Flashblock));
    assert_eq!(unit_of(true, None), Some(OffsetUnit::Flashblock));
    assert_eq!(unit_of(false, None), None);
}
