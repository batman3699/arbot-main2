//! Task 8.5 R41: Denim's clocks. The read clock, the 20-head window the armed
//! lane's Denim guard reads (chain cadence from `timestampMs`, and head lag),
//! and the ten-block lattice the fork switch proves Denim by.
//!
//! The fixtures are Base Sepolia's `newHeads` after Denim, recorded from NZ on
//! 2026-10-31: one clean minute, and one with a delivery stall.

use apex_runtime::denim::{detect, lattice, Detect, HeadWindow, LatticeBlock, ReadClock, DENIM_MAINNET_TS, DENIM_SEPOLIA_TS};

/// `(number, timestamp, timestamp_ms, arrival_ms)`, as recorded.
fn heads(file: &str) -> Vec<(u64, u64, u64, u64)> {
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let q = |x: &serde_json::Value| u64::from_str_radix(x.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    v["heads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (q(&h["number"]), q(&h["timestamp"]), q(&h["timestampMs"]), h["arrival_ms"].as_u64().unwrap()))
        .collect()
}

fn clean() -> Vec<(u64, u64, u64, u64)> {
    heads("sepolia_denim_heads.json")
}

/// Twenty heads stamped 200 ms apart, the newest `last` at `newest_ms`.
fn stamped(last: u64, newest_ms: u64) -> HeadWindow {
    let mut w = HeadWindow::new();
    for n in last - 19..=last {
        w.observe(n, Some(newest_ms - (last - n) * 200));
    }
    w
}

// ------------------------------------------------------------- read clock

#[test]
fn the_read_clock_runs_a_round_at_most_every_interval() {
    let mut c = ReadClock::new(2_000);
    let due: Vec<u64> = (0..=10_000).step_by(200).filter(|t| c.due(*t)).collect();
    assert_eq!(due, vec![0, 2_000, 4_000, 6_000, 8_000, 10_000]);
}

#[test]
fn a_zero_interval_runs_every_head() {
    let mut c = ReadClock::new(0);
    assert!((0..20).all(|t| c.due(t * 200)));
}

/// A clock that steps back starts a round at once, rather than waiting for
/// the old time to come round again.
#[test]
fn a_clock_that_steps_back_starts_a_round() {
    let mut c = ReadClock::new(2_000);
    assert!(c.due(10_000));
    assert!(c.due(5_000), "stepped back");
    assert!(!c.due(6_999));
    assert!(c.due(7_000));
}

// ------------------------------------------------------------- head window

/// Sepolia's measured heads hold at the Denim block time and at no other.
#[test]
fn sepolia_heads_hold_at_200_and_not_at_2000() {
    let mut w = HeadWindow::new();
    for (i, (n, _, ms, arrival)) in clean().into_iter().enumerate() {
        w.observe(n, Some(ms));
        if i >= 19 {
            assert!(w.holds(200, 1_000, arrival), "head {n} at 200");
            assert!(!w.holds(2_000, 1_000, arrival), "head {n} at 2000");
        }
    }
    assert_eq!(w.chain_cadence_ms(), Some(200));
}

/// The chain's clock, not delivery's: five heads arriving inside 10 ms
/// change nothing.
#[test]
fn a_burst_of_arrivals_keeps_the_chain_cadence() {
    let hs = clean();
    let mut w = HeadWindow::new();
    let burst_at = hs[29].3;
    for (i, (n, _, ms, arrival)) in hs.iter().take(30).enumerate() {
        w.observe(*n, Some(*ms));
        let at = if (25..30).contains(&i) { burst_at + (i as u64 - 25) * 2 } else { *arrival };
        if i >= 19 {
            assert!(w.holds(200, 1_000, at), "head {n}");
        }
    }
    assert_eq!(w.chain_cadence_ms(), Some(200));
}

#[test]
fn a_skipped_head_keeps_the_cadence_and_is_counted() {
    let mut w = HeadWindow::new();
    for (i, (n, _, ms, _)) in clean().into_iter().take(40).enumerate() {
        if i != 25 {
            w.observe(n, Some(ms));
        }
    }
    assert_eq!((w.chain_cadence_ms(), w.skips()), (Some(200), 1));
}

#[test]
fn a_repeated_head_is_ignored() {
    let hs = clean();
    let mut w = HeadWindow::new();
    for (n, _, ms, _) in hs.iter().take(30) {
        w.observe(*n, Some(*ms));
        if *n == hs[25].0 {
            w.observe(*n, Some(*ms + 5));
            w.observe(*n - 1, Some(*ms));
        }
    }
    assert_eq!((w.chain_cadence_ms(), w.skips()), (Some(200), 0));
    assert_eq!(w.head_lag_ms(hs[29].2 + 100), Some(100));
}

/// At most `max_head_lag_ms` from the newest head's stamp, inclusive.
#[test]
fn head_lag_bounds_sends() {
    let t = 1_792_605_601_400;
    let w = stamped(100, t);
    assert!(w.holds(200, 1_000, t + 999));
    assert!(w.holds(200, 1_000, t + 1_000));
    assert!(!w.holds(200, 1_000, t + 1_001));
    assert_eq!(w.head_lag_ms(t + 1_001), Some(1_001));
}

/// No `timestampMs`, no sends: such a head passes neither check, until a
/// stamped one follows it.
#[test]
fn a_head_without_timestamp_ms_fails_both_checks() {
    let t = 1_792_605_601_400;
    let mut w = stamped(100, t);
    w.observe(101, None);
    assert_eq!((w.chain_cadence_ms(), w.head_lag_ms(t + 300)), (None, None));
    assert!(!w.holds(200, 1_000, t + 300));
    assert_eq!(w.without_ms(), 1);
    w.observe(102, Some(t + 400));
    assert!(w.holds(200, 1_000, t + 500));
}

/// The host's clock behind the chain's: a stamp after now has no lag.
#[test]
fn a_head_stamped_after_now_has_no_lag() {
    let t = 1_792_605_601_400;
    let w = stamped(100, t);
    assert_eq!(w.head_lag_ms(t - 50), Some(0));
    assert!(w.holds(200, 1_000, t - 50));
}

/// A real delivery stall (34 heads over a second late): the guard refuses
/// exactly those, and holds again once delivery recovers.
#[test]
fn a_delivery_stall_idles_the_guard_and_it_recovers() {
    let hs = heads("sepolia_denim_heads_stall.json");
    let mut w = HeadWindow::new();
    let (mut refused, mut held_after) = (0, 0);
    for (i, (n, _, ms, arrival)) in hs.iter().enumerate() {
        w.observe(*n, Some(*ms));
        if i >= 19 {
            let late = arrival - ms > 1_000;
            assert_eq!(w.holds(200, 1_000, *arrival), !late, "head {n}");
            refused += usize::from(late);
            held_after = if late { 0 } else { held_after + 1 };
        }
    }
    assert_eq!(refused, 34);
    assert!(held_after >= 30, "{held_after}");
}

// ------------------------------------------------------------- the lattice

fn ten_sepolia() -> Vec<LatticeBlock> {
    clean().into_iter().take(10).map(|(number, timestamp, ms, _)| LatticeBlock { number, timestamp, timestamp_ms: Some(ms) }).collect()
}

#[test]
fn the_lattice_proves_denim_and_nothing_else() {
    let ok = ten_sepolia();
    assert_eq!(lattice(&ok, DENIM_SEPOLIA_TS), Ok(()));
    let refused = |blocks: Vec<LatticeBlock>, fork_at: u64, word: &str| {
        let e = lattice(&blocks, fork_at).unwrap_err();
        assert!(e.contains(word), "{e} should name {word}");
    };
    refused(ok[..9].to_vec(), DENIM_SEPOLIA_TS, "ten");
    let two_s: Vec<LatticeBlock> =
        (0..10).map(|i| LatticeBlock { number: 100 + i, timestamp: 1_793_901_580 + 2 * i, timestamp_ms: None }).collect();
    refused(two_s, 0, "timestampMs");
    let mut gap = ok.clone();
    for b in &mut gap[5..] {
        b.number += 1;
    }
    refused(gap, DENIM_SEPOLIA_TS, "consecutive");
    let mut step = ok.clone();
    for b in &mut step[5..] {
        b.timestamp_ms = b.timestamp_ms.map(|m| m + 200);
        b.timestamp = b.timestamp_ms.unwrap() / 1_000;
    }
    refused(step, DENIM_SEPOLIA_TS, "step");
    let mut off = ok.clone();
    off[3].timestamp_ms = off[3].timestamp_ms.map(|m| m + 1);
    refused(off, DENIM_SEPOLIA_TS, "lattice");
    let mut second = ok.clone();
    second[3].timestamp += 1;
    refused(second, DENIM_SEPOLIA_TS, "second");
    refused(ok.clone(), ok[0].timestamp + 1, "fork");
}

/// The order of the switch's answers: the chain first, then the fork time,
/// then the proof.
#[test]
fn detect_orders_its_answers() {
    let sepolia = ten_sepolia();
    assert!(matches!(detect(84_532, 8_453, &sepolia, DENIM_SEPOLIA_TS), Detect::Anomaly(_)));
    assert_eq!(detect(84_532, 84_532, &sepolia, DENIM_SEPOLIA_TS), Detect::Denim);
    let mainnet: Vec<LatticeBlock> =
        (0..10).map(|i| LatticeBlock { number: 53_556_100 + i, timestamp: 1_793_901_572 + 2 * i, timestamp_ms: None }).collect();
    assert!(matches!(detect(8_453, 8_453, &mainnet, DENIM_MAINNET_TS), Detect::NotYet(_)));
    let mut half: Vec<LatticeBlock> = (0..10)
        .map(|i| LatticeBlock {
            number: 53_556_200 + i,
            timestamp: (1_793_901_700_000 + 200 * i) / 1_000,
            timestamp_ms: Some(1_793_901_700_000 + 200 * i),
        })
        .collect();
    assert_eq!(detect(8_453, 8_453, &half, DENIM_MAINNET_TS), Detect::Denim);
    half[4].timestamp_ms = None;
    assert!(matches!(detect(8_453, 8_453, &half, DENIM_MAINNET_TS), Detect::Anomaly(_)));
}
