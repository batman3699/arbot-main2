//! R27.2: the size search against exhaustive search.
//!
//! The oracle prices every size of a route given as a table. The search is
//! held to it exactly when its tolerance is zero, and to within its tolerance
//! otherwise. The search before R27.2 is a comparator here, not an oracle: it
//! promises nothing once costs step, and the first test is a route it gets
//! wrong.

#[path = "support/synth.rs"]
mod synth;
#[path = "support/ternary.rs"]
mod ternary;

use apex_math::finite_size::{best_size, NoSize, SearchBudget, SizedOpportunity};
use ethers_core::types::U256;
use synth::{generated, net_of, oracle, table, Counting, Table};
use ternary::ternary_best_size;

/// A budget that cannot bind, and no tolerance: the search must be exact.
fn exact(start: u64) -> SearchBudget {
    SearchBudget { max_evaluations: 100_000, min_input: U256::one(), start: U256::from(start), tolerance_ppm: 0 }
}

/// The tolerance the search states, for what it found.
fn tolerance(t: &Table, found: i128, ppm: u32) -> i128 {
    (found.max(0) + i128::from(t.fixed)) * i128::from(ppm) / 1_000_000 + 4 * i128::from(t.rounding)
}

struct Seen {
    size: u64,
    net: i128,
    quotes: u32,
    largest: u64,
}

/// Run the search on `t` and hold it to its contract and to the oracle:
/// - what it reports is the route's own net at that size (claim 1);
/// - `Ok` exactly when that net is a gain;
/// - no size nets more than `max(found, 0)` plus the tolerance (claim 2);
/// - it asks for no `output`, and never for a size outside the range.
fn held(t: &Table, budget: SearchBudget, what: &str) -> Seen {
    let counting = Counting::new(t);
    let found = best_size(&counting, budget);
    let want = oracle(t, 1).expect("size 1 prices");
    let (size, net) = net_of(&found).unwrap_or_else(|| panic!("{what}: nothing priced, the oracle found {want:?}"));
    assert_eq!(t.net(size), Some(net), "{what}: the net reported at {size} is not the route's");
    assert_eq!(found.is_ok(), net >= 0, "{what}: Ok is a gain and only a gain");
    let slack = tolerance(t, net, budget.tolerance_ppm);
    assert!(
        net.max(0) + slack >= want.1.max(0),
        "{what}: found {net} at {size}, the oracle {} at {} (tolerance {slack})",
        want.1,
        want.0
    );
    assert_eq!(counting.outputs(), 0, "{what}: the search prices, it does not ask for outputs");
    assert!(counting.largest() <= U256::from(t.cap.min(t.n())), "{what}: a size past the range");
    Seen { size, net, quotes: counting.quotes(), largest: counting.largest().as_u64() }
}

/// A gross that rises to a peak near 600 of 1,000 sizes: slope 60 at size 1,
/// one less every ten sizes.
fn peaked(fixed: u64, steps: &[(u64, u64)]) -> Table {
    table(1_000, |x| 60 - (x / 10) as i64, fixed, steps)
}

/// **The design review's counter-example** (2026-10-14). The gross peaks at
/// 500 of 1,000; the cost steps up at 200 by more than the gross gains from
/// there to its peak. The best net is the size just under the step. A search
/// that brackets the gross's peak and looks inside the bracket never sees it.
#[test]
fn the_reviews_counter_example() {
    let t = table(1_000, |x| 5_000 - 10 * x as i64 + 5, 0, &[(200, 2_000_000)]);
    assert_eq!(t.gross_peak(), 500);
    assert_eq!(oracle(&t, 1), Some((199, 796_995)));
    for start in [1, 7, 64, 100, 400, 1_000] {
        let seen = held(&t, exact(start), &format!("start {start}"));
        assert_eq!((seen.size, seen.net), (199, 796_995), "start {start}");
    }
    // The search it replaces settles for the best of what it happened to price.
    let old = net_of(&ternary_best_size(&t, 128, U256::one())).unwrap();
    assert!(old.1 < 796_995, "the old search found {old:?}");
}

#[test]
fn a_cost_step_below_inside_and_above_the_peak() {
    let flat = held(&peaked(100, &[]), exact(64), "no step");
    assert!((599..=609).contains(&flat.size), "the peak's plateau: {}", flat.size);
    // Below: the step is paid long before the peak, and changes nothing.
    assert_eq!(held(&peaked(100, &[(100, 500)]), exact(64), "below").net, flat.net - 500);
    // Inside: 300 more from 590, where the gross has 10 left to gain.
    let inside = held(&peaked(100, &[(590, 300)]), exact(64), "inside");
    assert_eq!(inside.size, 589, "one below the step");
    // Above: never paid.
    assert_eq!(held(&peaked(100, &[(800, 5_000)]), exact(64), "above").net, flat.net);
    held(&peaked(100, &[(100, 500), (590, 300), (800, 5_000)]), exact(64), "all three");
}

#[test]
fn several_steps_near_the_peak() {
    for steps in [
        vec![(560, 30), (575, 40), (590, 50), (598, 5)],
        vec![(400, 2_000), (500, 900), (550, 300), (580, 100), (595, 20), (601, 1)],
        vec![(2, 1), (3, 1), (4, 1), (5, 1), (6, 1), (7, 1), (8, 1), (9, 1)],
        (1..=40).map(|i| (i * 15, 3 * i)).collect(),
    ] {
        for start in [1, 64, 700] {
            held(&peaked(50, &steps), exact(start), &format!("steps {steps:?}, start {start}"));
        }
    }
}

#[test]
fn the_best_at_the_cap_and_a_cap_below_start() {
    let mut rising = table(1_000, |_| 20, 300, &[]);
    rising.cap = 777;
    let seen = held(&rising, exact(64), "rising to the cap");
    assert_eq!((seen.size, seen.net), (777, 20 * 777 - 300), "the cap itself, not a size short of it");
    let mut small = peaked(10, &[]);
    small.cap = 5;
    assert_eq!(held(&small, exact(64), "cap 5, start 64").size, 5);
    small.cap = 1;
    assert_eq!(held(&small, exact(64), "cap 1").size, 1);
}

#[test]
fn a_refusal_below_at_and_above_the_best() {
    // The gross rises to a plateau from 599 to 609.
    for limit in [1, 300, 599, 600, 900] {
        let mut t = peaked(100, &[]);
        t.limit = Some(limit);
        for start in [1, 64, 950] {
            let seen = held(&t, exact(start), &format!("limit {limit}, start {start}"));
            assert!(seen.size <= limit && seen.size >= limit.min(599), "limit {limit}, start {start}: {}", seen.size);
            // A refusal is closed in on by halves, not a size at a time.
            assert!(seen.quotes <= 32, "limit {limit}, start {start}: {} quotes", seen.quotes);
        }
    }
}

#[test]
fn a_flat_gross_no_gap_and_a_gap_under_the_fixed_cost() {
    // Output equal to input at every size: nothing to gain, the cost to lose.
    let flat = held(&table(500, |_| 0, 10, &[]), exact(64), "flat");
    assert!(flat.net < 0 && flat.quotes <= 8, "{} quotes", flat.quotes);
    // Free and flat: every size nets zero, and none can beat another.
    let free = held(&table(500, |_| 0, 0, &[]), exact(64), "flat and free");
    assert!(free.net == 0 && free.quotes <= 8, "{} quotes", free.quotes);
    // No gap: every size returns less than it took.
    let losing = table(2_000, |_| -1, 7, &[]);
    let seen = held(&losing, exact(64), "no gap");
    assert!(seen.net < 0);
    // Two quotes: the line through them, carried down, bounds every smaller
    // size, and a loss is not chased to its smallest.
    assert_eq!(seen.quotes, 2, "a route with no gap is settled at once");
    // A gap of 500 against a fixed cost of 10,000.
    let thin = table(1_000, |x| if x <= 100 { 5 } else { -1 }, 10_000, &[]);
    assert!(held(&thin, exact(64), "a gap under the fixed cost").net < 0);
}

/// Of equal nets the smaller size: it costs no more and risks less.
#[test]
fn equal_nets_take_the_smaller_size() {
    let t = Table { gross: vec![0, 5, 5], cost: vec![0, 0, 0], fixed: 0, cap: 2, limit: None, unit: 1, rounding: 0 };
    for start in [1, 2, 64] {
        assert_eq!(held(&t, exact(start), "two sizes").size, 1, "start {start}");
    }
}

const STARTS: [u64; 5] = [1, 7, 64, 500, 3_000];

#[test]
fn generated_routes_match_the_oracle_exactly() {
    let (mut most, mut total, mut cases) = (0, 0u64, 0u64);
    for seed in 0..300 {
        let t = generated(seed);
        for start in STARTS {
            let seen = held(&t, exact(start), &format!("seed {seed}, start {start}"));
            // Under 16 times the size where the gross peaks, or 4 times the
            // start when the peak is below it: nothing far above the answer.
            let bound = (16 * t.gross_peak()).max(4 * start).min(t.cap.min(t.n()));
            assert!(seen.largest <= bound, "seed {seed}, start {start}: quoted {} past {bound}", seen.largest);
            most = most.max(seen.quotes);
            total += u64::from(seen.quotes);
            cases += 1;
        }
    }
    println!("exact: {cases} cases, {} quotes a case on average, {most} at most", total / cases);
    // Measured 2026-10-14: 8 on average and 31 at most. A bound that stopped
    // pruning, or a rule that stopped closing in, shows here.
    assert!(total / cases <= 10 && most <= 40, "{} on average, {most} at most", total / cases);
}

#[test]
fn generated_routes_are_within_the_tolerance() {
    for seed in 0..300 {
        let t = generated(seed);
        for (start, ppm) in [(64, 1), (64, 10_000), (1, 100_000), (500, 100_000)] {
            let budget = SearchBudget { tolerance_ppm: ppm, ..exact(start) };
            held(&t, budget, &format!("seed {seed}, start {start}, {ppm} ppm"));
        }
    }
}

/// An output truncated to a coarse unit, as a token of few decimals truncates
/// it: the gross is a sawtooth under its concave curve. A route that declares
/// the unit is searched within the tolerance that declaration gives, inside
/// the ordinary budget.
#[test]
fn a_truncated_output_is_within_its_declared_rounding() {
    for seed in 0..60 {
        let mut t = generated(seed);
        t.unit = 16;
        t.rounding = 16;
        let budget = SearchBudget { max_evaluations: 128, tolerance_ppm: 1, ..exact(64) };
        let seen = held(&t, budget, &format!("seed {seed}, truncated"));
        assert!(seen.quotes < 128, "seed {seed}: the budget ended it ({} quotes)", seen.quotes);
    }
}

/// Outside its assumptions the search still tells the truth about what it
/// priced, and stops at its budget.
#[test]
fn a_gross_that_is_not_concave_is_still_priced_honestly() {
    // Two humps: up 50 a size to 100, down 40 to 200, up 30 to 800, then down.
    let slope = |x: u64| match x {
        1..=100 => 50,
        101..=200 => -40,
        201..=800 => 30,
        _ => -1,
    };
    let t = table(1_000, slope, 200, &[(300, 4_000)]);
    for start in [1, 64, 500] {
        let budget = SearchBudget { max_evaluations: 128, ..exact(start) };
        let counting = Counting::new(&t);
        let found = best_size(&counting, budget);
        let (size, net) = net_of(&found).expect("it prices");
        assert_eq!(t.net(size), Some(net), "start {start}");
        assert_eq!(found.is_ok(), net >= 0);
        assert!(counting.quotes() <= 128);
    }
}

fn old_net(t: &Table) -> i128 {
    let old: Result<SizedOpportunity, NoSize> = ternary_best_size(t, 128, U256::one());
    net_of(&old).map_or(i128::MIN, |f| f.1)
}

/// The comparator: the new search is never under the old one.
#[test]
fn the_new_search_is_never_under_the_old() {
    let mut better = 0;
    for seed in 0..300 {
        let t = generated(seed);
        let new = held(&t, exact(64), &format!("seed {seed}")).net;
        let old = old_net(&t);
        assert!(new.max(0) >= old.max(0), "seed {seed}: new {new}, old {old}");
        better += u32::from(new.max(0) > old.max(0));
    }
    println!("the new search found a strictly better net than the old on {better} of 300 generated routes");
}

/// The budget ends the search where it stands (§29).
#[test]
fn the_budget_ends_the_search() {
    let t = peaked(100, &[(590, 300)]);
    let unbounded = held(&t, exact(64), "unbounded").quotes;
    assert!(unbounded > 3);
    for most in [1, 2, 3] {
        let counting = Counting::new(&t);
        let found = best_size(&counting, SearchBudget { max_evaluations: most, ..exact(64) });
        assert_eq!(counting.quotes(), most);
        let (size, net) = net_of(&found).expect("the first size prices");
        assert_eq!(t.net(size), Some(net), "what it reports it priced");
    }
}

/// Two truncated quotes can slope the wrong way: one at the top of its tooth,
/// the other at the bottom. A line through them, carried across a region,
/// would bound it too low. So every line is drawn through its nearer point
/// raised by the route's rounding. These are routes the search gets wrong
/// without that, each found by pricing 85,000 truncated routes against the
/// oracle on 2026-10-14 with one line left unraised:
///
/// - above the largest size priced: the first quote truncates to nothing, and
///   the route is given up after it;
/// - between two sizes, the line from the left;
/// - between two sizes, the line from the right.
///
/// Below the smallest size no such route was found: the walk down keeps the
/// two smallest sizes a factor of four apart, where the line is off by at most
/// 4/3 of the rounding, inside the tolerance. It is raised all the same, by
/// the one rule.
#[test]
fn a_line_through_truncated_quotes_is_raised_by_the_rounding() {
    let truncated = |mut t: Table, unit: u64| {
        t.unit = unit;
        t.rounding = unit;
        t
    };
    // The first quote, at size 1, truncates to nothing.
    let t = truncated(table(3_000, |x| (150 - (x / 17) as i64).max(-1), 500, &[]), 256);
    assert_eq!(oracle(&t, 1), Some((2_510, 191_806)));
    assert!(held(&t, exact(1), "the first quote truncates to nothing").net > 190_000);
    // The line from the left, and the line from the right.
    held(&truncated(generated(203), 256), exact(5), "seed 203");
    held(&truncated(steepened(generated(3_104), 10), 16), exact(1), "seed 3104");
}

/// `t` with every rise of its gross multiplied by `k`. Only a rise: an output
/// never falls as its input grows, so a gross never falls faster than 1 a size.
fn steepened(mut t: Table, k: i64) -> Table {
    let old = t.gross.clone();
    for x in 1..old.len() {
        let step = old[x] - old[x - 1];
        t.gross[x] = t.gross[x - 1] + if step > 0 { step * k } else { step };
    }
    t
}

/// The same hunt, kept: 4,000 generated routes, steepened, truncated to four
/// units, from five starts. None is short of the oracle by more than the
/// tolerance.
#[test]
#[ignore = "80,000 routes against the oracle: a minute in release"]
fn truncated_routes_by_the_thousand() {
    for seed in 0..4_000u64 {
        for unit in [3u64, 16, 50, 256] {
            for start in [1u64, 5, 64, 211, 900] {
                let mut t = steepened(generated(seed), 1 + (seed % 7) as i64 * 3);
                t.unit = unit;
                t.rounding = unit;
                held(&t, exact(start), &format!("seed {seed}, unit {unit}, start {start}"));
            }
        }
    }
}
