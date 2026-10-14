#![allow(dead_code)]
//! Routes given as tables over a few thousand integer sizes, and the oracle
//! that prices every one of them: what R27.2's search is held to.

use apex_math::finite_size::{NoSize, Priced, SizedOpportunity, SizedRoute, Surplus};
use ethers_core::types::U256;
use std::cell::Cell;

/// A route over the sizes `1..=n`.
#[derive(Clone, Debug)]
pub struct Table {
    /// `gross[x]` is `output(x) - x` before truncation; `gross[0] = 0`.
    pub gross: Vec<i64>,
    /// `cost[x]`; `cost[0]` is unused.
    pub cost: Vec<u64>,
    /// The least any size costs.
    pub fixed: u64,
    pub cap: u64,
    /// Sizes above this are refused.
    pub limit: Option<u64>,
    /// Outputs are truncated down to a multiple of this; 1 is exact.
    pub unit: u64,
    /// What the route declares as its rounding.
    pub rounding: u64,
}

impl Table {
    pub fn n(&self) -> u64 {
        self.gross.len() as u64 - 1
    }

    pub fn top(&self) -> u64 {
        self.cap.min(self.n()).min(self.limit.unwrap_or(u64::MAX))
    }

    fn out(&self, x: u64) -> Option<u64> {
        if x == 0 || x > self.n() || self.limit.is_some_and(|l| x > l) {
            return None;
        }
        let o = x as i64 + self.gross[x as usize];
        (o >= 0).then(|| o as u64 / self.unit * self.unit)
    }

    /// The route's own net at `x`.
    pub fn net(&self, x: u64) -> Option<i128> {
        Some(i128::from(self.out(x)?) - i128::from(x) - i128::from(self.cost[x as usize]))
    }

    /// The size at which the gross is highest, the smallest among equals.
    pub fn gross_peak(&self) -> u64 {
        (1..=self.top()).max_by_key(|x| (self.gross[*x as usize], std::cmp::Reverse(*x))).unwrap_or(1)
    }
}

fn small(a: U256) -> Option<u64> {
    (a <= U256::from(u64::MAX)).then(|| a.as_u64())
}

impl SizedRoute for Table {
    fn output(&self, amount_in: U256) -> Option<U256> {
        small(amount_in).and_then(|x| self.out(x)).map(U256::from)
    }
    fn fixed_cost(&self) -> U256 {
        U256::from(self.fixed)
    }
    fn max_input(&self) -> U256 {
        U256::from(self.cap.min(self.n()))
    }
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        let x = small(amount_in)?;
        Some(Priced { output: U256::from(self.out(x)?), cost: U256::from(self.cost[x as usize]) })
    }
    fn rounding(&self) -> U256 {
        U256::from(self.rounding)
    }
}

/// Every size from `lo` up priced: the highest net, and the smallest size
/// that earns it. `None` when the route prices none of them.
pub fn oracle(t: &Table, lo: u64) -> Option<(u64, i128)> {
    (lo.max(1)..=t.top()).filter_map(|x| Some((x, t.net(x)?))).max_by_key(|(x, n)| (*n, std::cmp::Reverse(*x)))
}

/// What a search found: its size and its signed net.
pub fn net_of(found: &Result<SizedOpportunity, NoSize>) -> Option<(u64, i128)> {
    let signed = |b: &SizedOpportunity| {
        let n = match b.net {
            Surplus::Gain(v) => v.as_u128() as i128,
            Surplus::Loss(v) => -(v.as_u128() as i128),
        };
        (b.amount_in.as_u64(), n)
    };
    match found {
        Ok(b) => Some(signed(b)),
        Err(NoSize::NoProfitableSize { best }) => Some(signed(best)),
        Err(_) => None,
    }
}

/// A route that counts what is asked of it.
pub struct Counting<'a, R: SizedRoute + ?Sized> {
    pub inner: &'a R,
    quotes: Cell<u32>,
    outputs: Cell<u32>,
    largest: Cell<U256>,
}

impl<'a, R: SizedRoute + ?Sized> Counting<'a, R> {
    pub fn new(inner: &'a R) -> Self {
        Self { inner, quotes: Cell::new(0), outputs: Cell::new(0), largest: Cell::new(U256::zero()) }
    }
    /// `priced` calls.
    pub fn quotes(&self) -> u32 {
        self.quotes.get()
    }
    /// `output` calls: the search makes none.
    pub fn outputs(&self) -> u32 {
        self.outputs.get()
    }
    /// The largest size asked about.
    pub fn largest(&self) -> U256 {
        self.largest.get()
    }
}

impl<R: SizedRoute + ?Sized> SizedRoute for Counting<'_, R> {
    fn output(&self, amount_in: U256) -> Option<U256> {
        self.outputs.set(self.outputs.get() + 1);
        self.inner.output(amount_in)
    }
    fn fixed_cost(&self) -> U256 {
        self.inner.fixed_cost()
    }
    fn max_input(&self) -> U256 {
        self.inner.max_input()
    }
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        self.quotes.set(self.quotes.get() + 1);
        self.largest.set(self.largest.get().max(amount_in));
        self.inner.priced(amount_in)
    }
    fn rounding(&self) -> U256 {
        self.inner.rounding()
    }
}

/// A fixed sequence: the same tables on every run.
struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    /// `lo..=hi`.
    fn in_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// A concave piecewise-linear gross (slopes from at most +60 down to no less
/// than -1, up to 8 kinks), a cost that steps up at up to 12 sizes, a cap and
/// sometimes a refusal.
pub fn generated(seed: u64) -> Table {
    let mut s = Seq(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
    let n = s.in_range(50, 3_000);
    let mut kinks: Vec<(u64, i64)> = (0..s.in_range(0, 8)).map(|_| (s.in_range(2, n), s.in_range(1, 20) as i64)).collect();
    kinks.sort();
    let mut slope = s.in_range(0, 61) as i64 - 1;
    let mut gross = vec![0i64];
    for x in 1..=n {
        while kinks.first().is_some_and(|(at, _)| *at == x) {
            slope = (slope - kinks.remove(0).1).max(-1);
        }
        gross.push(gross[x as usize - 1] + slope);
    }
    let fixed = s.in_range(0, 3_000);
    let mut steps: Vec<(u64, u64)> = (0..s.in_range(0, 12)).map(|_| (s.in_range(2, n), s.in_range(1, 4_000))).collect();
    steps.sort();
    let mut level = fixed;
    let mut cost = vec![0u64];
    for x in 1..=n {
        while steps.first().is_some_and(|(at, _)| *at == x) {
            level += steps.remove(0).1;
        }
        cost.push(level);
    }
    let cap = if s.next().is_multiple_of(3) { n } else { s.in_range(1, n) };
    let limit = s.next().is_multiple_of(4).then(|| s.in_range(1, n));
    Table { gross, cost, fixed, cap, limit, unit: 1, rounding: 0 }
}

/// A gross from a slope at each size (`slope(x)` is the step into `x`), with a
/// cost that is `fixed` and steps up by `by` at each `at`.
pub fn table(n: u64, slope: impl Fn(u64) -> i64, fixed: u64, steps: &[(u64, u64)]) -> Table {
    let mut gross = vec![0i64];
    let mut cost = vec![0u64];
    for x in 1..=n {
        gross.push(gross[x as usize - 1] + slope(x));
        cost.push(fixed + steps.iter().filter(|(at, _)| *at <= x).map(|(_, by)| by).sum::<u64>());
    }
    Table { gross, cost, fixed, cap: n, limit: None, unit: 1, rounding: 0 }
}
