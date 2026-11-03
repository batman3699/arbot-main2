//! The walk against `best_size` (Task 8.5 R40.1), the search it must match:
//! seeded random pool pairs with initialized ticks in the way, both directions,
//! at several gas prices. `best_size` is the reference for optimality, so the
//! walk's net, each crossing's gas charged, must reach its net.

use apex_math::cl_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::{quote_exact_input_multi_tick, TickLadder};
use apex_math::finite_size::{best_size, Priced, SearchBudget, SizedRoute, Surplus};
use apex_math::walk::{quote, walk, WalkLeg, WalkPrice};
use ethers_core::types::U256;

const CROSS_GAS: u64 = 30_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[(self.next() % of.len() as u64) as usize]
    }
    /// `10^lo` to `10^hi`, log-uniform.
    fn magnitude(&mut self, lo: i64, hi: i64) -> u128 {
        let tenths = self.range(lo * 10, hi * 10) as f64 / 10.0;
        10f64.powf(tenths) as u128
    }
}

struct Pool {
    state: ClPoolState,
    ladder: TickLadder,
    zero_for_one: bool,
}

/// A pool whose price falls (or rises) as the route trades, with up to `count`
/// ticks in its way, each moving liquidity by a random factor.
fn pool(rng: &mut Rng, tick: i32, zero_for_one: bool, count: i64) -> Pool {
    let spacing = rng.pick(&[1, 10, 60, 200]);
    let tick = tick - tick.rem_euclid(spacing);
    let liquidity = rng.magnitude(21, 25);
    let mut ticks = Vec::new();
    let (mut at, mut l) = (tick, liquidity as f64);
    for _ in 0..count {
        let gap = rng.range(1, 40) as i32 * spacing;
        at = if zero_for_one { at - gap } else { at + gap };
        let after = l * [0.3, 0.5, 0.8, 1.5, 3.0][rng.range(0, 4) as usize];
        // Crossing down takes the net away, and crossing up adds it.
        let net = if zero_for_one { l - after } else { after - l };
        ticks.push((at, net as i128));
        l = after;
    }
    let state = ClPoolState {
        sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
        liquidity,
        tick,
        tick_spacing: spacing,
        fee_ppm: rng.pick(&[100, 500, 3_000]),
        balance0: None,
        balance1: None,
    };
    Pool { state, ladder: TickLadder::new(ticks, MIN_TICK, MAX_TICK), zero_for_one }
}

fn leg(p: &Pool) -> WalkLeg<'_> {
    WalkLeg::Cl { state: &p.state, ladder: &p.ladder, zero_for_one: p.zero_for_one, cross_gas: CROSS_GAS }
}

struct Route<'a> {
    pools: [&'a Pool; 2],
    wei_per_gas: U256,
}

impl Route<'_> {
    fn crossings(&self, amount_in: U256) -> Option<u32> {
        let [a, b] = self.pools;
        let one = quote_exact_input_multi_tick(&a.state, &a.ladder, amount_in, a.zero_for_one, u32::MAX)?;
        let two = quote_exact_input_multi_tick(&b.state, &b.ladder, one.amount_out, b.zero_for_one, u32::MAX)?;
        Some(one.ticks_crossed + two.ticks_crossed)
    }
}

impl SizedRoute for Route<'_> {
    fn output(&self, amount_in: U256) -> Option<U256> {
        let [a, b] = self.pools;
        quote([leg(a), leg(b)], amount_in).map(|q| q[1])
    }
    fn fixed_cost(&self) -> U256 {
        U256::zero()
    }
    fn max_input(&self) -> U256 {
        U256::exp10(27)
    }
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        let cost = U256::from(self.crossings(amount_in)?) * U256::from(CROSS_GAS) * self.wei_per_gas;
        Some(Priced { output: self.output(amount_in)?, cost })
    }
    fn rounding(&self) -> U256 {
        U256::from(16)
    }
}

fn net_of(route: &Route, size: U256) -> f64 {
    let Some(p) = route.priced(size) else { return f64::NEG_INFINITY };
    p.output.as_u128() as f64 - size.as_u128() as f64 - p.cost.as_u128() as f64
}

#[test]
fn the_walk_reaches_best_sizes_net() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut judged, mut worse, mut worst) = (0u32, 0u32, 0f64);
    let mut shortfalls = Vec::new();
    for case in 0..500 {
        let high = rng.range(-40_000, 40_000) as i32;
        let gap = rng.range(40, 1_500) as i32;
        let down_first = rng.next().is_multiple_of(2);
        // The first hop sells where the pool is dear, and the second buys back.
        let (one, two) = if down_first { (high, high - gap) } else { (high - gap, high) };
        let (a, b) = (pool(&mut rng, one, down_first, rng_count(case)), pool(&mut rng, two, !down_first, rng_count(case + 1)));
        let wei_per_gas = U256::from(rng.pick(&[0u64, 10_000_000, 1_000_000_000, 100_000_000_000]));
        let route = Route { pools: [&a, &b], wei_per_gas };
        let price = WalkPrice { wei_per_gas, rate: None };
        let walked = walk([leg(&a), leg(&b)], U256::MAX, price, 64);
        let best = best_size(&route, SearchBudget::for_decimals(18));
        let (Some(w), Ok(best)) = (walked, best) else { continue };
        let Surplus::Gain(gain) = best.net else { continue };
        if gain.is_zero() {
            continue;
        }
        judged += 1;
        let (reference, got) = (gain.as_u128() as f64, net_of(&route, w.size));
        // The gain past a boundary ignores the next one, so at worst one
        // crossing's gas is spent on a segment that didn't quite repay it.
        let allowed = (CROSS_GAS as f64 * wei_per_gas.as_u128() as f64) * 1.01 + 64.0 + reference * 1e-7;
        assert!(reference - got <= allowed, "case {case}: {got} against {reference}, {allowed} allowed");
        let short = (reference - got) / reference;
        shortfalls.push(short);
        if short > 1e-4 {
            worse += 1;
        }
        worst = worst.max(short);
    }
    shortfalls.sort_by(f64::total_cmp);
    let at = |q: f64| shortfalls[((shortfalls.len() - 1) as f64 * q) as usize];
    println!("judged {judged}, over 0.01%: {worse}, worst {worst:.6}, p50 {:.8}, p95 {:.8}, p99 {:.8}", at(0.5), at(0.95), at(0.99));
    assert!(judged >= 400, "only {judged} cases judged");
    assert!(worse <= 5, "{worse} cases short by more than 0.01%");
}

fn rng_count(seed: i64) -> i64 {
    1 + (seed * 7 + 3) % 6
}
