//! R26: judging an arm against what the chain did.

use apex_runtime::armed::eval::{
    bid_wei_per_gas, judge, landing, merge_renewals, net_wei, next_flashblock, Point, TxEffect, Write, F_PERMILLE,
};
use apex_runtime::armed::lines::{Arm, ArmedLine, Renewal, Word};
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
    }
}

fn set(addr: &str, slot: u64, v: u64) -> Write {
    Write::Set { address: addr.into(), slot: U256::from(slot), value: U256::from(v) }
}

fn tx(block: u64, i: u64, writes: Vec<Write>) -> TxEffect {
    TxEffect { block, tx_index: i, tx: format!("0x{block}{i}"), writes }
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
    let l = landing(k, None, 1, 0);
    assert!(l.optimistic && l.pessimistic);
    // Rival in the trigger's flashblock: optimistic needs the higher bid; pessimistic loses.
    let l = landing(k, Some((102, 4)), 10, 5);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, Some((102, 4)), 5, 10);
    assert!(!l.optimistic && !l.pessimistic);
    // Rival in the next: optimistic wins outright; pessimistic needs the higher bid.
    let l = landing(k, Some((102, 5)), 5, 10);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, Some((102, 5)), 10, 5);
    assert!(l.optimistic && l.pessimistic);
    // Rival later still: won either way.
    let l = landing(k, Some((102, 7)), 0, 1_000);
    assert!(l.optimistic && l.pessimistic);
}

#[test]
fn bids_and_nets_are_exact() {
    assert_eq!(F_PERMILLE, [50, 100, 200, 300, 500, 700, 900]);
    // 0.5 × 3e14 wei over 3e5 gas = 5e8 wei per gas.
    assert_eq!(bid_wei_per_gas(U256::from(300_000_000_000_000u64), 300_000, 500), 500_000_000);
    // gross 1e15 − 3e5 × (5e6 + 5e8) − 1e10
    assert_eq!(net_wei(U256::from(10u64.pow(15)), 300_000, 5_000_000, 500_000_000, 10_000_000_000), 848_490_000_000_000);
}
