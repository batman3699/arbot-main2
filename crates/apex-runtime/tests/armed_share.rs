//! Task 8.5 R38: the bid against our own fires' contests, as shares of the
//! arm's surplus (`best_share`), and which bid an arm sends (`arm_bid`).

use apex_runtime::armed::bid::{arm_bid, best_bid, best_share, Bid, BidChoice, BidModel, ShareSample, MIN_EFFECTIVE};
use ethers_core::types::U256;

fn sample(rows: &[(u128, u64)], effective: u64, fit_ok: bool) -> ShareSample {
    ShareSample::new(rows.to_vec(), effective, fit_ok, "r38-test".into()).unwrap()
}

/// The vectors the census's Python mirror reads too: both must agree to the wei.
#[test]
fn best_share_matches_the_golden_vectors() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/best_share_vectors.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for case in v["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let rows: Vec<(u128, u64)> =
            case["rows"].as_array().unwrap().iter().map(|r| (r[0].as_u64().unwrap().into(), r[1].as_u64().unwrap())).collect();
        let surplus = U256::from_dec_str(case["surplus"].as_str().unwrap()).unwrap();
        let gas = case["gas"].as_u64().unwrap();
        let cap: u128 = case["cap"].as_str().unwrap().parse().unwrap();
        let got = ShareSample::new(rows, 1, true, "v".into()).and_then(|s| best_share(&s, surplus, gas, cap).map_err(str::to_string));
        match case.get("error").and_then(|e| e.as_str()) {
            Some(e) => assert_eq!(got.unwrap_err(), e, "{name}"),
            None => {
                let b = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                let below = case["below"].as_u64().unwrap() as f64;
                let total = case["total"].as_u64().unwrap() as f64;
                assert_eq!(b, Bid { per_gas: case["bid"].as_str().unwrap().parse().unwrap(), p_win: below / total }, "{name}");
            }
        }
    }
}

/// A row is unusable unless the rows are sorted and weighted.
#[test]
fn a_sample_refuses_unsorted_or_weightless_rows() {
    assert!(ShareSample::new(vec![(5, 1), (4, 1)], 2, true, "v".into()).is_err());
    assert!(ShareSample::new(vec![(4, 1), (5, 0)], 2, true, "v".into()).is_err());
    let s = sample(&[(0, 2), (0, 3), (9, 5)], 3, true);
    assert_eq!(s.total(), 10);
}

/// An overflowing product fails closed rather than wrapping.
#[test]
fn an_overflow_fails_closed() {
    let s = sample(&[(u128::MAX, 1)], 1, true);
    assert_eq!(best_share(&s, U256::MAX, 1, u128::MAX), Err("overflow"));
}

/// Whatever the rows, the bid keeps a strictly positive surplus and stays
/// under the cap, and its `p_win` is the weight below that very bid.
#[test]
fn the_bid_pays_and_its_chance_is_its_own() {
    let rows: Vec<(u128, u64)> = (0..200u128).map(|i| (i * i * 37_000_000, 1 + (i % 3) as u64)).collect();
    let s = sample(&rows, 200, true);
    for (surplus, gas, cap) in [(10u64.pow(13), 291_200u64, 20_000_000_000u128), (3 * 10u64.pow(12), 614_600, 5_000_000), (10u64.pow(15), 250_000, u128::MAX)] {
        let surplus = U256::from(surplus);
        let b = best_share(&s, surplus, gas, cap).unwrap();
        assert!(b.per_gas <= cap);
        assert!(U256::from(b.per_gas) * U256::from(gas) < surplus);
        let below: u64 = rows
            .iter()
            .filter(|(x, _)| U256::from(*x) * surplus < U256::from(b.per_gas) * U256::from(gas) * U256::from(1_000_000_000u64))
            .map(|(_, w)| *w)
            .sum();
        assert_eq!(b.p_win, below as f64 / s.total() as f64);
    }
}

const WINNERS: [u128; 4] = [0, 1_000_000, 10_000_000, 50_000_000];

/// Which bid is sent, and which rides along to be judged against it.
#[test]
fn an_arm_sends_its_model_s_bid_and_carries_the_other() {
    let surplus = U256::from(50_000_000_000_000u64);
    let gas = 500_000;
    let cap = 20_000_000_000;
    let winners = best_bid(&WINNERS, surplus, gas, cap);
    let rows: Vec<(u128, u64)> = (0..MIN_EFFECTIVE as u128).map(|i| (i * 1_000_000, 1)).collect();
    let fit = sample(&rows, MIN_EFFECTIVE, true);
    let share = best_share(&fit, surplus, gas, cap).unwrap();
    assert_ne!(share, winners, "the test needs the two rules to differ");

    let a = arm_bid(&WINNERS, None, BidModel::BestBid, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (winners, None, BidChoice::BestBid));
    let a = arm_bid(&WINNERS, Some(&fit), BidModel::BestBid, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (winners, Some(share), BidChoice::BestBid));
    let a = arm_bid(&WINNERS, Some(&fit), BidModel::BestShare, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (share, Some(winners), BidChoice::BestShare));

    let a = arm_bid(&WINNERS, None, BidModel::BestShare, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (winners, None, BidChoice::Fallback("no contest sample")));
    let unfit = sample(&rows, MIN_EFFECTIVE, false);
    let a = arm_bid(&WINNERS, Some(&unfit), BidModel::BestShare, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (winners, Some(share), BidChoice::Fallback("contest sample unfit")));
    let thin = sample(&rows, MIN_EFFECTIVE - 1, true);
    let a = arm_bid(&WINNERS, Some(&thin), BidModel::BestShare, surplus, gas, cap);
    assert_eq!((a.sent, a.alt, a.choice), (winners, None, BidChoice::Fallback("too few effective rows")));
    let a = arm_bid(&WINNERS, Some(&fit), BidModel::BestShare, U256::zero(), gas, cap);
    assert_eq!((a.sent.per_gas, a.alt, a.choice), (0, None, BidChoice::Fallback("zero surplus")));
    // No winners measured: R36's rule bids nothing and ranks as R26's did.
    let a = arm_bid(&[], Some(&fit), BidModel::BestBid, surplus, gas, cap);
    assert_eq!((a.sent, a.alt), (Bid { per_gas: 0, p_win: 1.0 }, Some(share)));
}

/// The label an arm records: the model, or the fallback and why.
#[test]
fn a_choice_names_itself() {
    assert_eq!(BidChoice::BestBid.label(), "best_bid");
    assert_eq!(BidChoice::BestShare.label(), "best_share");
    assert_eq!(BidChoice::Fallback("contest sample unfit").label(), "best_bid (fallback: contest sample unfit)");
}
