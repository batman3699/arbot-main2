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

use ethers_core::types::U256;

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
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            max_evaluations: 128,
            min_input: U256::from(1_000_000_000_000u64), // 1e-6 of an 18-decimal token
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

/// What one probe of a size search learned.
enum Probe {
    Net(Surplus),
    /// The route cannot be priced at this size, nor at any larger one.
    Refused,
    /// The evaluation budget is gone.
    Spent,
}

/// Find the input that maximises net profit, or say why there is none.
///
/// `net(x) = output(x) - x - cost(x)`, with each size's cost from
/// [`SizedRoute::priced`]. Under a fixed cost it is concave — a concave
/// function minus a linear one — so it is unimodal and a ternary search finds
/// the maximum without evaluating every size. The search is over integers and
/// narrows to a handful of candidates, which are then scanned exactly; ternary
/// search alone can be off by one on a lattice.
///
/// A cost that steps up with the size (gas, per tick crossed) leaves `net`
/// concave between its steps and small steps across them, so the search may
/// stop near the best size rather than on it. What it reports is still exact:
/// a size, and the net that size earns at its own cost.
///
/// **A size the route refuses bounds the search; it does not end it.** A
/// concentrated pool's quote runs off its proven ladder, or past its tick
/// limit, beyond some size — and more input runs further, so every larger size
/// is refused too. The range ends at what its pool *holds*, which for a deep
/// pool is thousands of times what it can price, so the first probes are
/// usually refused. The search narrows below each refusal. Until 2026-10-05 a
/// refusal ended it, and a route paying 44 bps at a tenth of a WETH reported no
/// profitable size because its pool held a million.
pub fn best_size<R: SizedRoute + ?Sized>(
    route: &R,
    budget: SearchBudget,
) -> Result<SizedOpportunity, NoSize> {
    let lo_bound = budget.min_input.max(U256::one());
    let hi_bound = route.max_input();
    if hi_bound < lo_bound {
        return Err(NoSize::RangeEmpty);
    }

    let mut evaluations = 0u32;
    let mut best: Option<SizedOpportunity> = None;
    let mut evaluate = |x: U256, best: &mut Option<SizedOpportunity>| -> Probe {
        if evaluations >= budget.max_evaluations {
            return Probe::Spent;
        }
        evaluations += 1;
        let Some(priced) = route.priced(x) else { return Probe::Refused };
        let net = Surplus::of(priced.output, x.saturating_add(priced.cost));
        let candidate = SizedOpportunity {
            amount_in: x,
            output: priced.output,
            net,
        };
        if best.is_none_or(|b| candidate.net > b.net) {
            *best = Some(candidate);
        }
        Probe::Net(net)
    };

    let (mut lo, mut hi) = (lo_bound, hi_bound);
    let three = U256::from(3u64);
    while hi > lo && hi - lo > U256::from(2u64) {
        let span = hi - lo;
        let m1 = lo + span / three;
        let m2 = hi - span / three;
        if m1 >= m2 {
            break;
        }
        // The lower probe first: refused, the higher one would be too.
        let f1 = match evaluate(m1, &mut best) {
            Probe::Net(f) => f,
            Probe::Refused => {
                hi = m1 - U256::one();
                continue;
            }
            Probe::Spent => break,
        };
        let f2 = match evaluate(m2, &mut best) {
            Probe::Net(f) => f,
            Probe::Refused => {
                hi = m2 - U256::one();
                continue;
            }
            Probe::Spent => break,
        };
        if f1 < f2 {
            lo = m1 + U256::one();
        } else {
            hi = m2;
        }
    }

    // Exact scan of the narrowed bracket. Bounded: the loop above reduces the
    // span geometrically, so this is a handful of points. A refusal ends it:
    // every size past it is refused too.
    let mut x = lo;
    while x <= hi {
        if !matches!(evaluate(x, &mut best), Probe::Net(_)) {
            break;
        }
        x = x.saturating_add(U256::one());
        if x == U256::zero() {
            break;
        }
    }

    match best {
        None => Err(NoSize::Unpriceable),
        Some(b) if b.net.is_gain() => Ok(b),
        Some(b) => Err(NoSize::NoProfitableSize { best: b }),
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
