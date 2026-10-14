//! R27.3: the economics stage's sizing against exhaustive search.
//!
//! The stage sizes a candidate with a warm start (`continuous::optimize`) and
//! a refinement that mints the size (`discrete::refine_detailed`). Until
//! R27.3 the warm start was a golden section on the net, which a cost that
//! steps misleads, and the refinement a climb that recovered a whole number
//! from its float. The warm start is now the certified size search's own
//! size, handed over exactly, and the live refinement prices it once.

#[path = "support/golden_climb.rs"]
mod golden_climb;
#[path = "../../apex-math/tests/support/synth.rs"]
mod synth;

use apex_econ::sizing::continuous::{optimize, ContinuousBudget};
use apex_econ::sizing::discrete::{refine_detailed, RefineBudget};
use apex_math::finite_size::{best_size, Priced, SearchBudget, SizedRoute, Surplus};
use ethers_core::types::U256;
use synth::{generated, oracle, Counting, Table};

fn signed(net: Surplus) -> i128 {
    match net {
        Surplus::Gain(v) => v.as_u128() as i128,
        Surplus::Loss(v) => -(v.as_u128() as i128),
    }
}

/// The pair as `LiveEconomics` runs it, on a table: sizes from 1, and a
/// budget for the warm start that cannot bind (a table's steps are far denser
/// than a pool's).
fn pair(t: &Table) -> Option<(u64, i128, bool)> {
    let start = optimize(t, ContinuousBudget { iterations: 10_000, min_input: U256::one() })?;
    let found = refine_detailed(t, start, RefineBudget::start_only())?;
    assert_eq!(found.evaluations, 1, "the refinement prices its start and no more");
    Some((found.amount.as_u64(), signed(found.net), found.size.is_some()))
}

/// **The pair returns the best net of every size**, across cost steps, caps
/// and refusals, and no size when none pays. The pair it replaces is the
/// comparator: it falls short where a cost step misleads a golden section,
/// and never does better.
#[test]
fn the_pair_sizes_to_the_oracle() {
    let (mut short, mut paying) = (0, 0);
    for seed in 0..300 {
        let t = generated(seed);
        let (best_size, best) = oracle(&t, 1).expect("size 1 prices");
        let (size, net, minted) = pair(&t).unwrap_or_else(|| panic!("seed {seed}: no warm start"));
        assert_eq!(t.net(size), Some(net), "seed {seed}: the net reported at {size} is the route's");
        assert_eq!(minted, net >= 0, "seed {seed}: a size is minted for a gain and only a gain");
        assert_eq!(net.max(0), best.max(0), "seed {seed}: found {net} at {size}, the oracle {best} at {best_size}");

        let old = golden_climb::golden_then_climb(&t, U256::one()).map_or(i128::MIN, |(_, n, _)| signed(n));
        assert!(old.max(0) <= net.max(0), "seed {seed}: the old pair found {old}, the new {net}");
        paying += u32::from(best > 0);
        short += u32::from(old.max(0) < best.max(0));
    }
    println!("of {paying} routes that pay, the old pair fell short of the oracle on {short}");
    assert!(short >= 10, "the comparator falls short on stepped costs: {short} of {paying}");
}

/// A route in wei: 0.1% more out than in, up to `cap`, at a cost that steps
/// up by `jump` from `step` on. Its division truncates, so its gross is a
/// sawtooth under one wei high, and it says so.
struct Wei {
    cap: U256,
    step: U256,
    jump: U256,
}

impl SizedRoute for Wei {
    fn output(&self, amount_in: U256) -> Option<U256> {
        Some(amount_in + amount_in / 1_000)
    }
    fn fixed_cost(&self) -> U256 {
        U256::from(1_000_000_000u64)
    }
    fn max_input(&self) -> U256 {
        self.cap
    }
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        let cost = self.fixed_cost() + if amount_in >= self.step { self.jump } else { U256::zero() };
        Some(Priced { output: self.output(amount_in)?, cost })
    }
    fn rounding(&self) -> U256 {
        U256::one()
    }
}

/// **The refinement starts at the search's own size, to the wei.** A `f64`
/// holds a wei amount near 18 WETH only to the nearest 2,048, so a start that
/// passed through one would be a different size from the one the search
/// priced: here, one it never priced at all.
#[test]
fn the_refinement_starts_at_the_searchs_own_size() {
    let wei = |whole: u64, odd: u64| U256::from(whole) * U256::exp10(18) + U256::from(odd);
    // The step costs more than the gross gains from it to the cap.
    let route = Wei { cap: wei(30, 12_345), step: wei(18, 690_000_000_001_235), jump: U256::exp10(17) };
    let searched = best_size(&route, SearchBudget::default()).expect("it pays");
    assert!(searched.amount_in < route.step, "under the step: {}", searched.amount_in);
    assert!(route.step - searched.amount_in < U256::exp10(14), "and close under it: {}", searched.amount_in);
    assert_ne!(searched.amount_in % U256::from(2_048u64), U256::zero(), "a size a float cannot hold");

    let counting = Counting::new(&route);
    let start = optimize(&counting, ContinuousBudget::default()).expect("a warm start");
    let warm = counting.quotes();
    let found = refine_detailed(&counting, start, RefineBudget::start_only()).expect("priced");
    assert_eq!(found.amount, searched.amount_in, "the size the search priced, exactly");
    assert_eq!(found.net, searched.net);
    assert_eq!((found.evaluations, counting.quotes() - warm), (1, 1), "one quote to mint it");
    assert!(found.size.is_some());
    // The float is still what the warm start shows the outside: the same
    // size, to the nearest 2,048 wei.
    assert_eq!(start.value(), searched.amount_in.as_u128() as f64);
}

/// The warm start spends at most what the golden section could: its
/// iterations and two (§29).
#[test]
fn the_warm_start_is_bounded() {
    let route = Wei { cap: U256::from(30u64) * U256::exp10(18), step: U256::from(18u64) * U256::exp10(18) + U256::from(7u64), jump: U256::exp10(17) };
    for iterations in [0u32, 1, 5, 60] {
        let counting = Counting::new(&route);
        let start = optimize(&counting, ContinuousBudget { iterations, ..ContinuousBudget::default() }).expect("the first size prices");
        assert!(counting.quotes() <= iterations + 2, "{iterations} iterations: {} quotes", counting.quotes());
        assert_eq!(start.evaluations(), counting.quotes());
    }
    // Unbounded in practice, it settles in far fewer than the 62 it spent.
    let counting = Counting::new(&route);
    optimize(&counting, ContinuousBudget::default()).expect("a warm start");
    assert!(counting.quotes() <= 62);
}

/// A warm start made on a route whose range has since shrunk (a lender's
/// holding fell, a pool's balance moved) is clamped into the range, never
/// priced outside it.
#[test]
fn a_start_outside_the_range_is_clamped_into_it() {
    let far = U256::from(30u64) * U256::exp10(18);
    let wide = Wei { cap: far, step: far + U256::one(), jump: U256::zero() };
    let start = optimize(&wide, ContinuousBudget::default()).expect("a warm start");
    assert_eq!(start.value(), 30e18, "the gross rises to the cap");

    let near = U256::from(5u64) * U256::exp10(18);
    let narrow = Wei { cap: near, step: far, jump: U256::zero() };
    let counting = Counting::new(&narrow);
    let found = refine_detailed(&counting, start, RefineBudget::start_only()).expect("priced");
    assert_eq!(found.amount, near);
    assert_eq!(counting.largest(), near, "nothing priced past the range");
}

/// A route in wei that pays 0.05% and truncates its output to a thousandth of
/// a token, and says so.
struct Coarse;

impl SizedRoute for Coarse {
    fn output(&self, amount_in: U256) -> Option<U256> {
        let unit = U256::exp10(15);
        Some((amount_in + amount_in / 2_000) / unit * unit)
    }
    fn fixed_cost(&self) -> U256 {
        U256::from(1_000_000_000u64)
    }
    fn max_input(&self) -> U256 {
        U256::from(30u64) * U256::exp10(18)
    }
    fn rounding(&self) -> U256 {
        U256::exp10(15)
    }
}

/// **The warm start searches the route with the route's own rounding.** At
/// one token this route's gain is half its unit, and truncates to nothing. A
/// search told the route is exact reads that as no gap, bounds every larger
/// size by it, and gives the route up after one quote; at the cap the route
/// pays fifteen units.
#[test]
fn the_warm_start_carries_the_routes_rounding() {
    assert_eq!(Coarse.output(U256::exp10(18)), Some(U256::exp10(18)), "the first quote shows no gain");
    let start = optimize(&Coarse, ContinuousBudget::default()).expect("a warm start");
    let found = refine_detailed(&Coarse, start, RefineBudget::start_only()).expect("priced");
    assert!(found.size.is_some(), "it pays: {found:?}");
    assert!(signed(found.net) > 14 * 10i128.pow(15), "close to the fifteen units at the cap: {}", signed(found.net));
}
