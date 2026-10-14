//! Engine C: finite-size route evaluation (PLAN.md §12.2, blueprint §12.3).
//!
//! # What Engine C is actually for
//!
//! Task 2.5 specified a fixture *"where infinitesimal rates show no negative
//! cycle but a finite size of 0.4 WETH is profitable across two venues"*, and
//! asked for a test asserting Engine C finds it while Engine A does not.
//!
//! **No such fixture exists.** Every venue this repository prices — constant
//! product, the Solidly curves, concentrated liquidity — has an output that is
//! concave in its input and zero at zero. For any such function the average
//! rate over `[0, x]` is at most the marginal rate at 0, and a cycle's
//! finite-size gross is the product of its hops' average rates. So the
//! finite-size gross can never exceed the marginal gross, and a cycle Engine A
//! rejects on marginal rates has no profitable size. Measured across spreads
//! of 50, 61, 65, 80, 200 and 1000 bps on a two-pool fixture: `avg <= marginal`
//! in every case, and the property test below asserts it over random ones.
//!
//! Engine C earns its place the other way round.
//!
//! **It refuses what Engine A proposes.** Edge weights in `graph.rs` are
//! rate-only — `compute_edge_weight` is `-ln(rate)` and nothing adds a gas
//! term, whatever the stale comment on `Edge::weight` says. So Engine A calls
//! any cycle with a marginal gross above 1 a negative cycle, regardless of
//! whether *any* size pays for the transaction. On the same fixture with gas at
//! 0.00002 WETH (~1 cent, this repository's own measurement):
//!
//! | spread | marginal gross | best gross over all sizes | clears gas |
//! |---|---|---|---|
//! | 60.5 bps | 1.0000228 | +0.00000006 WETH | no — 300x short |
//! | 61 bps | 1.0000725 | +0.00000066 WETH | no |
//! | 63 bps | 1.0002713 | +0.00000921 WETH | no |
//! | 65 bps | 1.0004701 | +0.00002765 WETH | yes |
//! | 80 bps | 1.0019611 | +0.00048050 WETH | yes |
//!
//! Engine A accepts all five. Three of them cannot be traded at any size. That
//! band is not a corner case: it is where the cheap frontier sits, and it is
//! consistent with the census finding 0 of 750 candidates net-positive.
//!
//! **And it says what size to trade.** The optimum on that fixture is 0.120
//! WETH at 65 bps and 0.490 WETH at 80 bps — so the plan's 0.4 WETH was the
//! right number for the wrong reason. A rate-only search cannot express a
//! quantity at all.

use ethers_core::types::{I256, U256};

/// Profit or loss, exactly, without squeezing a 256-bit quantity into a signed
/// integer.
///
/// `Ord` ranks every gain above every loss, gains by increasing magnitude and
/// losses by *decreasing* magnitude, so `max()` picks the best outcome whether
/// or not any outcome is positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surplus {
    Gain(U256),
    Loss(U256),
}

impl Surplus {
    pub const fn is_gain(&self) -> bool {
        matches!(self, Self::Gain(_))
    }

    /// The signed quantity as a magnitude plus a sign, for reporting.
    pub const fn magnitude(&self) -> U256 {
        match self {
            Self::Gain(v) | Self::Loss(v) => *v,
        }
    }

    fn of(output: U256, cost: U256) -> Self {
        if output >= cost {
            Self::Gain(output - cost)
        } else {
            Self::Loss(cost - output)
        }
    }
}

impl PartialOrd for Surplus {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Surplus {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Gain(a), Self::Gain(b)) => a.cmp(b),
            (Self::Loss(a), Self::Loss(b)) => b.cmp(a),
            (Self::Gain(_), Self::Loss(_)) => Ordering::Greater,
            (Self::Loss(_), Self::Gain(_)) => Ordering::Less,
        }
    }
}

/// What one pricing of a route says at one size: what it returns, and what
/// executing it at that size costs — both in input-token units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Priced {
    pub output: U256,
    pub cost: U256,
}

/// A closed route, priced at whatever size it is asked about.
pub trait SizedRoute {
    /// What the route returns for `amount_in`, in the SAME token. `None` when
    /// the route cannot be priced at that size — a refusal, never a zero.
    fn output(&self, amount_in: U256) -> Option<U256>;

    /// Cost that does not scale with size, in input-token units: gas, the L1
    /// data fee, a flash-loan flat component.
    ///
    /// This is the whole reason Engine C exists. A size-independent cost makes
    /// the profitable set an interval rather than a half-line, and an interval
    /// is not something a rate-only weight can express.
    fn fixed_cost(&self) -> U256;

    /// The largest input the route can absorb, from the thinnest hop's real
    /// holdings.
    fn max_input(&self) -> U256;

    /// What the route returns for `amount_in` and what executing it at that
    /// size costs: one pricing, both answers. **Every search sizes against
    /// this**, never against `fixed_cost` alone.
    ///
    /// The default charges `fixed_cost` at every size. A route whose cost grows
    /// with its size overrides it — a concentrated-liquidity hop pays gas for
    /// every initialized tick it crosses, so a larger trade costs more to
    /// settle — and a search that charged the fixed part alone would size past
    /// the point where the next unit of input pays for the gas it adds.
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        Some(Priced { output: self.output(amount_in)?, cost: self.fixed_cost() })
    }

    /// How far a quoted output can sit below the route's concave curve, in
    /// input-token units: what integer truncation takes. Zero for a route
    /// whose arithmetic is exact.
    ///
    /// [`best_size`] bounds the sizes it has not priced from the concavity of
    /// the gross. A truncated output is a sawtooth under that curve, and a
    /// bound drawn through two truncated quotes can be low by this much; the
    /// search raises its bounds and its tolerance by it rather than assume it
    /// away (R27.2).
    fn rounding(&self) -> U256 {
        U256::zero()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizedOpportunity {
    pub amount_in: U256,
    pub output: U256,
    pub net: Surplus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchBudget {
    /// Ceiling on route evaluations. §29 requires every search stage to carry
    /// a compute budget rather than run to convergence.
    pub max_evaluations: u32,
    /// Smallest input worth considering, in input-token units. Below this the
    /// fee arithmetic truncates to nothing and the answer is noise.
    pub min_input: U256,
    /// The first size priced: one at which the route's integer arithmetic is
    /// exact to well under a basis point (R27.2). The search walks up and
    /// down from it, so no size is left out.
    pub start: U256,
    /// The search stops when no size it has not priced can beat its best by
    /// more than this, in parts per million of that best net plus the fixed
    /// cost (R27.2). Four times the route's [`SizedRoute::rounding`] is added.
    pub tolerance_ppm: u32,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            max_evaluations: 128,
            min_input: U256::from(1_000_000_000_000u64), // 1e-6 of an 18-decimal token
            start: U256::from(1_000_000_000_000_000_000u64), // one 18-decimal token
            tolerance_ppm: 1,
        }
    }
}

/// Why no size was returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoSize {
    /// The route refuses every size in range.
    Unpriceable,
    /// `max_input` is below `min_input`; the route has no usable range.
    RangeEmpty,
    /// Priced at every size and profitable at none.
    ///
    /// **The Engine C verdict.** Engine A would have proposed this cycle: its
    /// marginal gross exceeds 1. No size pays for the transaction.
    NoProfitableSize { best: SizedOpportunity },
}

/// **INV-40.** The most common rejection this system makes has to say which miss
/// it is.
///
/// It had no implementation until Task 2b.4, and the reason is worth recording:
/// `scripts/ci/every_rejection_explains.sh` is a *naming* convention, its list
/// named `NoLane` because that was the `No*` type that existed, and `NoSize` was
/// therefore never asked. This is the rejection that matters most — this
/// repository measured **96% of candidates** ending in `no_profitable_size`, so
/// the single largest bucket in the missed-opportunity ledger was the one type
/// the gate could not see. The pattern is `No[A-Z]…` now.
///
/// `NoProfitableSize` is `LowEv` and not `SimFail`: the route priced fine at
/// every size. Nothing failed — it simply does not pay, which is what `LOW_EV`
/// means and is the whole content of the finding.
///
/// `Unpriceable` is `SimFail`: a venue refused to price, which is a *failure to
/// evaluate* rather than an economic verdict, and conflating the two would hide a
/// broken adapter inside the largest bucket.
///
/// `RangeEmpty` is `NoFlashLiquidity`: the route's thinnest hop cannot absorb the
/// minimum input, which is a depth fact about the venue and not about this
/// trade's economics.
impl apex_types::miss::ExplainsMiss for NoSize {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        use apex_types::miss::MissReason as R;
        match self {
            Self::NoProfitableSize { .. } => R::LowEv,
            Self::Unpriceable => R::SimFail,
            Self::RangeEmpty => R::NoFlashLiquidity,
        }
    }
}

/// One size the search has priced.
#[derive(Clone, Copy, Debug)]
struct Probe {
    x: U256,
    output: U256,
    cost: U256,
    /// `output - x`.
    gross: I256,
    /// `gross - cost`.
    net: I256,
}

fn signed(v: U256) -> Option<I256> {
    I256::try_from(v).ok()
}

impl Probe {
    /// `None` when the figures do not fit a signed word: read as a refusal.
    fn of(x: U256, priced: Priced) -> Option<Self> {
        let gross = signed(priced.output)?.checked_sub(signed(x)?)?;
        let net = gross.checked_sub(signed(priced.cost)?)?;
        Some(Self { x, output: priced.output, cost: priced.cost, gross, net })
    }
}

/// The line through `(ax, ag)` and `(bx, bg)`, `ax < bx`, at `x`, less any
/// fraction above zero: a gross is a whole number, so what bounds it also
/// bounds it at the next whole number down. `None` on overflow, which a caller
/// reads as "no bound".
fn line_at(ax: U256, ag: I256, bx: U256, bg: I256, x: U256) -> Option<I256> {
    let run = signed(bx.checked_sub(ax)?)?;
    if run <= I256::zero() {
        return None;
    }
    let along = signed(x)?.checked_sub(signed(ax)?)?;
    // Division truncates toward zero: down for a rise, up for a fall.
    ag.checked_add(bg.checked_sub(ag)?.checked_mul(along)?.checked_div(run)?)
}

/// Sizes the search has not priced, between two it has or at an end of the
/// range.
struct Region {
    /// No size in the region nets more than this. `None`: not known.
    bound: Option<I256>,
    /// The size to price to learn more about it.
    next: U256,
    /// The region above every size priced.
    open: bool,
}

/// What pricing one size did.
enum Asked {
    Priced,
    /// The route cannot be priced at this size, nor at any larger one.
    Refused,
    /// The evaluation budget is gone.
    Spent,
}

struct Search<'a, R: SizedRoute + ?Sized> {
    route: &'a R,
    budget: SearchBudget,
    lo: U256,
    /// The largest size not known to be refused.
    top: U256,
    /// A refusal has bounded the range: the walk up closes in on it by halves.
    refused: bool,
    /// Every size priced within the range, by size.
    probes: Vec<Probe>,
    /// The highest net priced, the smallest size among equals.
    best: Option<Probe>,
    evaluations: u32,
    fixed: I256,
    rounding: I256,
}

impl<R: SizedRoute + ?Sized> Search<'_, R> {
    fn price(&mut self, x: U256) -> Asked {
        if self.evaluations >= self.budget.max_evaluations {
            return Asked::Spent;
        }
        self.evaluations += 1;
        match self.route.priced(x).and_then(|priced| Probe::of(x, priced)) {
            Some(p) => {
                if self.best.is_none_or(|b| p.net > b.net || (p.net == b.net && p.x < b.x)) {
                    self.best = Some(p);
                }
                let at = self.probes.partition_point(|q| q.x < x);
                self.probes.insert(at, p);
                Asked::Priced
            }
            None => {
                // `x >= lo >= 1`. A larger size is refused too, so nothing
                // priced lies above this one.
                self.top = x - U256::one();
                self.refused = true;
                Asked::Refused
            }
        }
    }

    /// What a size not yet priced must beat to matter: the best net (or
    /// nothing, when the best is a loss) and the tolerance.
    fn bar(&self) -> I256 {
        let best = self.best.map_or(I256::zero(), |b| b.net.max(I256::zero()));
        let share = best
            .checked_add(self.fixed)
            .and_then(|v| v.checked_mul(I256::from(self.budget.tolerance_ppm)))
            .and_then(|v| v.checked_div(I256::from(1_000_000)))
            .unwrap_or_else(I256::zero);
        let sawtooth = self.rounding.checked_mul(I256::from(4)).unwrap_or_else(I256::zero);
        best.checked_add(share).and_then(|v| v.checked_add(sawtooth)).unwrap_or(best)
    }

    /// The sizes not yet priced, as regions between the priced ones, low to
    /// high, each with a bound on its net.
    ///
    /// A region's net is bounded by concavity: the gross lies under the line
    /// through any two priced sizes, outside those two. Each line is drawn
    /// through its nearer point raised by the route's rounding, and the
    /// origin, where the gross is zero, stands to the left of the first size.
    /// A region's cost is at least its left end's, or the fixed cost below the
    /// smallest size priced. A bound that overflows is no bound.
    fn regions(&self) -> Vec<Region> {
        let n = self.probes.len();
        let Some(last) = self.probes.last() else { return Vec::new() };
        let origin = (U256::zero(), I256::zero());
        let at = |i: usize| (self.probes[i].x, self.probes[i].gross);
        let raised = |i: usize| Some((self.probes[i].x, self.probes[i].gross.checked_add(self.rounding)?));
        let left_of = |i: usize| if i == 0 { origin } else { at(i - 1) };
        let less = |gross: Option<I256>, cost: Option<I256>| gross?.checked_sub(cost?);
        let one = U256::one();
        let mut regions = Vec::with_capacity(n + 1);

        // Below the smallest size priced: the line through the two smallest,
        // carried down. One priced size bounds nothing below it.
        let first = &self.probes[0];
        if first.x > self.lo {
            let gross = if n >= 2 {
                raised(0).and_then(|(x, g)| {
                    let b = at(1);
                    Some(line_at(x, g, b.0, b.1, self.lo)?.max(g))
                })
            } else {
                None
            };
            regions.push(Region {
                bound: less(gross, Some(self.fixed)),
                next: (first.x / 4).max(self.lo),
                open: false,
            });
        }

        // Between two priced sizes: under the line from the left, carried
        // right, and under the line from the right, carried left.
        for i in 0..n - 1 {
            let (a, b) = (&self.probes[i], &self.probes[i + 1]);
            if b.x - a.x < U256::from(2u64) {
                continue;
            }
            let from_left = raised(i).and_then(|(x, g)| {
                let l = left_of(i);
                Some(line_at(l.0, l.1, x, g, b.x)?.max(g))
            });
            let from_right = if i + 2 < n {
                raised(i + 1).and_then(|(x, g)| {
                    let r = at(i + 2);
                    Some(line_at(x, g, r.0, r.1, a.x)?.max(g))
                })
            } else {
                None
            };
            let gross = match (from_left, from_right) {
                (Some(l), Some(r)) => Some(l.min(r)),
                (l, r) => l.or(r),
            };
            regions.push(Region {
                bound: less(gross, signed(a.cost)),
                // Halve it: toward the gross's peak, or toward a cost step inside.
                next: a.x + (b.x - a.x) / 2,
                open: false,
            });
        }

        // Above the largest size priced.
        if last.x < self.top {
            let gross = raised(n - 1).and_then(|(x, g)| {
                let l = left_of(n - 1);
                Some(line_at(l.0, l.1, x, g, self.top)?.max(g))
            });
            regions.push(Region {
                bound: less(gross, signed(last.cost)),
                next: if self.refused {
                    last.x + (self.top - last.x + one) / 2
                } else {
                    last.x.saturating_mul(U256::from(4u64)).min(self.top)
                },
                open: true,
            });
        }
        regions
    }

    /// The size to price next, or `None` when no size left unpriced can net
    /// more than [`Self::bar`].
    fn next(&self) -> Option<U256> {
        let bar = self.bar();
        let regions = self.regions();
        let above = |r: &&Region| r.bound.is_none_or(|b| b > bar);
        // The region above the largest size first, while it can still pay: it
        // sets the scale of the answer.
        if let Some(open) = regions.last().filter(|r| r.open).filter(above) {
            return Some(open.next);
        }
        // Then the highest bound, no bound being the highest; of equals, the
        // higher region.
        regions.iter().filter(above).max_by_key(|r| (r.bound.is_none(), r.bound)).map(|r| r.next)
    }
}

/// Find the input that maximises net profit, or say why there is none.
///
/// `net(x) = output(x) - x - cost(x)`, with each size's cost from
/// [`SizedRoute::priced`].
///
/// **The search bounds what it has not priced (R27.2).** It assumes, of the
/// sizes the route prices:
///
/// - the gross, `output(x) - x`, is concave, from zero at zero size, to within
///   [`SizedRoute::rounding`];
/// - the cost never falls as the size grows, and is never under
///   [`SizedRoute::fixed_cost`];
/// - a refused size bounds the range: every larger size is refused too.
///
/// Then every size it has not priced lies in a region whose net it can bound
/// ([`Search::next`]). It prices inside the region with the highest bound and
/// stops when none can beat its best by more than its tolerance:
///
/// `(max(best, 0) + fixed_cost) * tolerance_ppm / 10^6 + 4 * rounding`.
///
/// So when it stops by that rule, **no size nets more than `max(best, 0)` plus
/// the tolerance**, and a route it calls unprofitable has no size netting more
/// than the tolerance. A cost that steps up (gas, per tick crossed) does not
/// weaken this: a region is charged the cost of its left end, so a size just
/// under a step is found. Nothing here assumes the net has one peak.
///
/// Outside those assumptions, and when the budget ends it, the search still
/// reports a size it priced and the net that size earns: no more is claimed.
///
/// Until R27.2 this was a ternary search over the whole range. It never
/// converged inside its budget, so every search cost the full 128 quotes, the
/// first of them at the largest sizes the route allowed; and across a cost
/// step it promised nothing. `tests/support/ternary.rs` keeps it as a
/// comparator.
///
/// **A size the route refuses bounds the search; it does not end it.** A
/// concentrated pool's quote runs off its proven ladder, or past its tick
/// limit, beyond some size, and more input runs further, so every larger size
/// is refused too. Until 2026-10-05 a refusal ended the search, and a route
/// paying 44 bps at a tenth of a WETH reported no profitable size because its
/// pool held a million.
pub fn best_size<R: SizedRoute + ?Sized>(
    route: &R,
    budget: SearchBudget,
) -> Result<SizedOpportunity, NoSize> {
    let lo = budget.min_input.max(U256::one());
    let hi = route.max_input();
    if hi < lo {
        return Err(NoSize::RangeEmpty);
    }
    let mut search = Search {
        route,
        budget,
        lo,
        top: hi,
        refused: false,
        probes: Vec::new(),
        best: None,
        evaluations: 0,
        // A cost too large to hold bounds nothing; a rounding too large to
        // hold leaves every region unbounded.
        fixed: signed(route.fixed_cost()).unwrap_or_else(I256::zero),
        rounding: signed(route.rounding()).unwrap_or(I256::MAX),
    };

    // The first size: `start`, or the largest quarter of it the route prices.
    let mut x = budget.start.clamp(lo, hi);
    while matches!(search.price(x), Asked::Refused) && x > lo {
        x = (x / 4).max(lo);
    }
    while let Some(x) = search.next() {
        if matches!(search.price(x), Asked::Spent) {
            break;
        }
    }

    match search.best {
        None => Err(NoSize::Unpriceable),
        Some(p) => {
            let found = SizedOpportunity {
                amount_in: p.x,
                output: p.output,
                net: Surplus::of(p.output, p.x.saturating_add(p.cost)),
            };
            if found.net.is_gain() {
                Ok(found)
            } else {
                Err(NoSize::NoProfitableSize { best: found })
            }
        }
    }
}

/// Engine A's own test, exactly: is the round trip above parity at this size?
///
/// Kept separate from [`marginal_gross_bps`] because that one rounds to whole
/// basis points and Engine A does not. The band where the two engines disagree
/// is thin — a 61 bps spread against two 30 bps fees leaves a gross of
/// +0.7 THOUSANDTHS of a basis point — and a reporting helper that rounds it
/// to zero cannot be the thing a test asserts on.
///
/// `None` when the route cannot be priced.
pub fn is_profitable_at<R: SizedRoute + ?Sized>(route: &R, probe: U256) -> Option<bool> {
    Some(route.output(probe)? > probe)
}

/// The marginal gross rate, in basis points above parity, as Engine A sees it.
///
/// **Resolution: whole basis points.** Engine A works on `-ln(rate)` in
/// floating point and resolves far finer, so this is for reporting, not for
/// deciding. Use [`is_profitable_at`] to ask Engine A's actual question.
///
/// Engine A's edge weights are `-ln(rate)` with no cost term, so this is
/// exactly the quantity its negative-cycle test is about. Exposed here so the
/// two engines can be compared on one route rather than by inspection.
///
/// `None` when the route cannot be priced at the probe size.
pub fn marginal_gross_bps<R: SizedRoute + ?Sized>(route: &R, probe: U256) -> Option<i64> {
    if probe.is_zero() {
        return None;
    }
    let out = route.output(probe)?;
    // (out - probe) / probe in bps, computed without floating point.
    let scaled = out.checked_mul(U256::from(10_000u64))?;
    let ratio = scaled / probe;
    let bps = i64::try_from(ratio.min(U256::from(i64::MAX as u64)).as_u64()).ok()?;
    Some(bps - 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A route over the sizes `1..=n`: `gross[x]` before its output is
    /// truncated to a multiple of `unit`, and `cost[x]`.
    struct Table {
        gross: Vec<i64>,
        cost: Vec<u64>,
        unit: u64,
    }

    impl Table {
        fn out(&self, x: u64) -> u64 {
            (x as i64 + self.gross[x as usize]) as u64 / self.unit * self.unit
        }
        fn net(&self, x: u64) -> i64 {
            self.out(x) as i64 - x as i64 - self.cost[x as usize] as i64
        }
    }

    impl SizedRoute for Table {
        fn output(&self, amount_in: U256) -> Option<U256> {
            Some(U256::from(self.out(amount_in.as_u64())))
        }
        fn fixed_cost(&self) -> U256 {
            U256::from(self.cost[1])
        }
        fn max_input(&self) -> U256 {
            U256::from(self.gross.len() as u64 - 1)
        }
        fn priced(&self, amount_in: U256) -> Option<Priced> {
            Some(Priced { output: self.output(amount_in)?, cost: U256::from(self.cost[amount_in.as_u64() as usize]) })
        }
        /// A truncated output is under the exact one by less than a unit.
        fn rounding(&self) -> U256 {
            U256::from(if self.unit > 1 { self.unit } else { 0 })
        }
    }

    /// A fixed sequence: the same routes on every run.
    struct Seq(u64);

    impl Seq {
        fn up_to(&mut self, most: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % (most + 1)
        }
    }

    /// **No size in a region nets more than the region's bound.** The search's
    /// guarantee is this and nothing else: it stops when no bound is above its
    /// bar. Checked by pricing every size, on 600 routes (a concave gross with
    /// kinks, a cost that steps, an output exact or truncated) each priced at
    /// a handful of sizes anywhere in its range, not only where the search
    /// would go.
    #[test]
    fn no_size_in_a_region_nets_more_than_its_bound() {
        let mut bounded = 0;
        for seed in 0..600u64 {
            let mut seq = Seq(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5);
            let n = 30 + seq.up_to(770);
            let (mut gross, mut cost) = (vec![0i64], vec![0u64]);
            let (mut slope, mut level) = (seq.up_to(80) as i64 - 1, seq.up_to(500));
            for _ in 1..=n {
                if seq.up_to(40) == 0 {
                    slope = (slope - seq.up_to(25) as i64).max(-1);
                }
                if seq.up_to(60) == 0 {
                    level += seq.up_to(900);
                }
                gross.push(gross.last().unwrap() + slope);
                cost.push(level);
            }
            let table = Table { gross, cost, unit: [1, 1, 7, 64][(seed % 4) as usize] };
            let mut search = Search {
                route: &table,
                budget: SearchBudget { max_evaluations: u32::MAX, min_input: U256::one(), start: U256::one(), tolerance_ppm: 0 },
                lo: U256::one(),
                top: U256::from(n),
                refused: false,
                probes: Vec::new(),
                best: None,
                evaluations: 0,
                fixed: signed(table.fixed_cost()).unwrap(),
                rounding: signed(table.rounding()).unwrap(),
            };
            for _ in 0..2 + seq.up_to(9) {
                let x = U256::from(1 + seq.up_to(n - 1));
                if search.probes.iter().all(|p| p.x != x) {
                    search.price(x);
                }
            }
            // The regions, as `regions` orders them: below, between, above.
            let sizes: Vec<u64> = search.probes.iter().map(|p| p.x.as_u64()).collect();
            let mut spans = Vec::new();
            if sizes[0] > 1 {
                spans.push((1, sizes[0] - 1));
            }
            spans.extend(sizes.windows(2).filter(|w| w[1] - w[0] >= 2).map(|w| (w[0] + 1, w[1] - 1)));
            if *sizes.last().unwrap() < n {
                spans.push((sizes.last().unwrap() + 1, n));
            }
            let regions = search.regions();
            assert_eq!(regions.len(), spans.len(), "seed {seed}");
            for (region, (from, to)) in regions.iter().zip(spans) {
                assert!(U256::from(from) <= region.next && region.next <= U256::from(to), "seed {seed}: the next size is in its region");
                let Some(bound) = region.bound else { continue };
                let (size, net) = (from..=to).map(|x| (x, table.net(x))).max_by_key(|(_, net)| *net).unwrap();
                assert!(I256::from(net) <= bound, "seed {seed}: size {size} in {from}..={to} nets {net}, over its bound {bound}");
                bounded += 1;
            }
        }
        assert!(bounded > 2_000, "{bounded} bounds checked");
    }
}
