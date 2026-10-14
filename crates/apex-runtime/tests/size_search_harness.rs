//! R27.2: both size searches over the live universe, in one binary, under one
//! cost model. The search before R27.2 is the comparator; the targets below
//! are what the new one was held to before it shipped.
//!
//! Every cycle of the shipped universe at a pinned block, either pool the
//! trigger, the trigger pool moved by 0, 5, 20, 50, 100 and 200 ticks or bps:
//! the states the armed lane prices, from no gap to a spike.
//!
//! Measured 2026-10-14 on the laptop, 1,827 cases, 41 pools, 158 cycles:
//!
//! | | quotes p50 / p99 / max | new, us p50 / p99 | old, us p50 / p99 | faster, p50 |
//! |---|---|---|---|---|
//! | every case | 12 / 24 / 26 | 93 / 3,807 | 1,650 / 24,316 | 11.8x |
//! | a size pays (1,049) | 16 / 25 / 26 | 267 / 4,078 | 2,303 / 25,264 | 7.8x |
//! | no size pays (778) | 4 / 7 / 8 | 25 / 73 | 1,275 / 2,860 | 54x |
//!
//! The old search took 128 quotes on every case. The new one was never under
//! it by more than its tolerance, its budget ended none, and across 30,525
//! priced sizes no cost fell and no gross sat under its neighbours' line by
//! more than the route's rounding.
//!
//! R27.3 added the economics stage's sizing: the pair that mints a candidate's
//! size (a warm start, then the refinement) against the pair it replaced, on
//! the 1,049 cases that pay. One pass, and the stage runs four a candidate:
//!
//! | | quotes p50 / max | us p50 / p90 / p99 / max |
//! |---|---|---|
//! | new pair | 17 / 27 | 240 / 1,526 / 3,511 / 3,942 |
//! | old pair | 211 / 243 | 3,123 / 16,654 / 36,653 / 50,066 |
//!
//! The new pair netted strictly more on 16 cases and was never under the old
//! by more than the search's tolerance.
//!
//! In the main tree, in release (a debug build is 30 times slower):
//! APEX_SECRET_BLOCKPI_KEY=… APEX_SECRET_TRADER_KEY=0x…01 \
//!   cargo test -j 3 --release --offline -p apex-runtime --test size_search_harness -- --ignored --nocapture

#[path = "../../apex-econ/tests/support/golden_climb.rs"]
mod golden_climb;
#[path = "../../apex-math/tests/support/ternary.rs"]
mod ternary;

use alloy_primitives::Address;
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_config::Env;
use apex_econ::sizing::continuous::{optimize, ContinuousBudget};
use apex_econ::sizing::discrete::{refine_detailed, RefineBudget};
use apex_math::finite_size::{best_size, NoSize, Priced, SearchBudget, SizedOpportunity, SizedRoute, Surplus};
use apex_runtime::armed::threshold::{move_of, trigger_up};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::book::PoolBook;
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::gas;
use apex_runtime::live::inventory::Venue;
use apex_runtime::live::lenders::{Lenders, MORPHO_EXTRA_GAS_DUSTED};
use apex_runtime::live::pricing::{CostedCycle, LiveCycle};
use apex_runtime::live::reads::ChainReads;
use apex_runtime::shadow::config::ShadowConfig;
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// 2026-10-13 19:53 UTC, a quiet minute of the R27.1 run.
const BLOCK: u64 = 52_565_834;
/// The trigger pool's moves: none, then the armed lane's rung offsets.
const MOVES: [u32; 6] = [0, 5, 20, 50, 100, 200];
/// Searches timed per case.
const REPEATS: u32 = 20;

/// A route that records what is asked of it.
struct Counting<'a> {
    inner: &'a CostedCycle,
    asked: RefCell<Vec<U256>>,
    outputs: Cell<u32>,
}

impl<'a> Counting<'a> {
    fn new(inner: &'a CostedCycle) -> Self {
        Self { inner, asked: RefCell::new(Vec::new()), outputs: Cell::new(0) }
    }
    fn quotes(&self) -> u32 {
        self.asked.borrow().len() as u32 + self.outputs.get()
    }
    fn largest(&self) -> U256 {
        self.asked.borrow().iter().copied().max().unwrap_or_default()
    }
}

impl SizedRoute for Counting<'_> {
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
        self.asked.borrow_mut().push(amount_in);
        self.inner.priced(amount_in)
    }
    fn rounding(&self) -> U256 {
        self.inner.rounding()
    }
}

fn signed(found: &Result<SizedOpportunity, NoSize>) -> Option<(U256, f64)> {
    let of = |b: &SizedOpportunity| {
        let wei = b.net.magnitude().min(U256::from(u128::MAX)).as_u128() as f64;
        (b.amount_in, if matches!(b.net, Surplus::Gain(_)) { wei } else { -wei })
    };
    match found {
        Ok(b) => Some(of(b)),
        Err(NoSize::NoProfitableSize { best }) => Some(of(best)),
        Err(_) => None,
    }
}

fn weth(v: U256) -> f64 {
    v.min(U256::from(u128::MAX)).as_u128() as f64 / 1e18
}

fn micros(mut f: impl FnMut()) -> f64 {
    let t = Instant::now();
    for _ in 0..REPEATS {
        f();
    }
    t.elapsed().as_secs_f64() * 1e6 / f64::from(REPEATS)
}

fn at(sorted: &[f64], share: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * share).round() as usize]
}

fn spread(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (at(&v, 0.5), at(&v, 0.9), at(&v, 0.99), *v.last().unwrap())
}

struct Case {
    venues: String,
    pays: bool,
    new_quotes: f64,
    old_quotes: f64,
    new_us: f64,
    old_us: f64,
    new_size: f64,
    /// The largest size the new search asked about, over the size it settled on.
    reach: f64,
    spent: bool,
}

#[tokio::test]
#[ignore = "network: an archive read of the whole universe, then a minute or two of pricing in release"]
async fn both_size_searches_over_the_live_universe() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let config = ShadowConfig::load(&format!("{root}/ops/shadow.base.yaml"), &Env::secrets_only()).unwrap();
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let settings = FailoverSettings { request_timeout: std::time::Duration::from_secs(60), ..FailoverSettings::default() };
    let rpc: Arc<dyn RpcTransport> = Arc::new(FailoverTransport::connect(&urls, 8453, settings).await.unwrap());
    let reads = ChainReads::new(Arc::clone(&rpc));
    std::env::set_current_dir(&root).unwrap();
    let venues = [Venue::UniswapV3, Venue::Slipstream, Venue::PancakeV3, Venue::SlipstreamV3, Venue::AerodromeV2];
    let specs = apex_runtime::shadow::universe(&config.inventory, &venues, config.universe.filter()).unwrap();
    let (book, unloaded) = PoolBook::load(&reads, &specs, BLOCK).await.unwrap();
    book.refresh_twaps(&reads, BLOCK).await.unwrap();
    book.refresh_cp_fees(&reads, BLOCK).await.unwrap();
    let snap = book.snapshot();
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    println!("block {BLOCK}: {} pools held, {} not loaded, {} cycles", snap.len(), unloaded.len(), cycles.len());

    // The host's cost model at that hour, and its lenders with the dust in place.
    let costs = RouteCosts { other_wei: 113_024_129_341, wei_per_gas: 5_040_189 };
    let lenders = Lenders {
        balancer: U256::from(18_750_000_000_000_000_000u128),
        morpho: Some((Address::repeat_byte(0xee), U256::from(82_689u64) * U256::exp10(18))),
        morpho_extra_gas: MORPHO_EXTRA_GAS_DUSTED,
    };
    let budget = SearchBudget::default();
    let old_search = |route: &dyn SizedRoute| ternary::ternary_best_size(route, budget.max_evaluations, budget.min_input);

    let mut cases: Vec<Case> = Vec::new();
    let (mut swept, mut cost_falls, mut bends) = (0u64, 0u64, 0u64);
    let mut resized: Vec<[f64; 5]> = Vec::new();
    for cycle in cycles.values() {
        for leg in 0..2usize {
            for step in MOVES {
                let trigger = &snap[&cycle.legs[leg].pool];
                let Some(moved) = trigger.moved(move_of(trigger, trigger_up(cycle, leg), step)) else { continue };
                let mut pools: BTreeMap<Address, _> = (*snap).clone();
                pools.insert(cycle.legs[leg].pool, Arc::new(moved));
                let Some(live) = LiveCycle::new(cycle, &pools) else { continue };
                let route = CostedCycle::new(live, costs, gas::MEASURED).lent(Some(lenders));

                let counting = Counting::new(&route);
                let new = best_size(&counting, budget);
                let (new_quotes, largest) = (counting.quotes(), counting.largest());
                assert_eq!(counting.outputs.get(), 0, "the search prices; it asks for no bare output");
                let counted = Counting::new(&route);
                let old = old_search(&counted);
                let old_quotes = counted.quotes();
                let (Some((size, net)), Some((_, was))) = (signed(&new), signed(&old)) else {
                    assert_eq!(signed(&new).is_none(), signed(&old).is_none(), "one search priced and the other did not");
                    continue;
                };

                // Parity: never under the old search by more than the tolerance.
                let fixed = weth(route.fixed_cost()) * 1e18;
                let tolerance = (net.max(0.0) + fixed) * f64::from(budget.tolerance_ppm) / 1e6 + 4.0 * weth(route.rounding()) * 1e18;
                let venues = format!("{:?}>{:?}", cycle.legs[0].venue, cycle.legs[1].venue);
                assert!(
                    net.max(0.0) + tolerance >= was.max(0.0),
                    "{venues} leg {leg} step {step}: new {net} at {}, old {was}, tolerance {tolerance}",
                    weth(size)
                );

                let new_us = micros(|| {
                    let _ = std::hint::black_box(best_size(&route, budget));
                });
                let old_us = micros(|| {
                    let _ = std::hint::black_box(old_search(&route));
                });
                cases.push(Case {
                    venues,
                    pays: new.is_ok(),
                    new_quotes: f64::from(new_quotes),
                    old_quotes: f64::from(old_quotes),
                    new_us,
                    old_us,
                    new_size: weth(size),
                    reach: weth(largest) / weth(size).max(1e-18),
                    spent: new_quotes >= budget.max_evaluations,
                });

                // R27.3: the economics stage sizes a candidate again. Its pair
                // (a warm start, then the refinement that mints the size)
                // against the pair it replaced, on the cases that pay.
                if new.is_ok() {
                    let c = Counting::new(&route);
                    let start = optimize(&c, ContinuousBudget::default()).expect("a warm start");
                    let found = refine_detailed(&c, start, RefineBudget::start_only()).expect("priced");
                    let pair_quotes = c.quotes();
                    let c = Counting::new(&route);
                    let (_, old_net, _) = golden_climb::golden_then_climb(&c, ContinuousBudget::default().min_input).expect("the old pair prices");
                    let old_quotes = c.quotes();
                    let wei = |n: Surplus| {
                        let v = n.magnitude().min(U256::from(u128::MAX)).as_u128() as f64;
                        if matches!(n, Surplus::Gain(_)) { v } else { -v }
                    };
                    assert!(
                        wei(found.net).max(0.0) + tolerance >= wei(old_net).max(0.0),
                        "{} leg {leg} step {step}: the new pair nets {}, the old {}, tolerance {tolerance}",
                        cases.last().unwrap().venues, wei(found.net), wei(old_net)
                    );
                    let pair_us = micros(|| {
                        let s = optimize(&route, ContinuousBudget::default());
                        let _ = std::hint::black_box(s.and_then(|s| refine_detailed(&route, s, RefineBudget::start_only())));
                    });
                    let old_us = micros(|| {
                        let _ = std::hint::black_box(golden_climb::golden_then_climb(&route, ContinuousBudget::default().min_input));
                    });
                    resized.push([pair_us, old_us, f64::from(pair_quotes), f64::from(old_quotes), f64::from(u8::from(wei(found.net) > wei(old_net)))]);
                }

                // The assumptions, across a sweep of sizes: a cost that never
                // falls, and a gross no further under its neighbours' line
                // than the rounding.
                if step == 0 || step == 50 {
                    let top = route.max_input().min(U256::from(2_000u64) * U256::exp10(18));
                    let mut points: Vec<(f64, f64, f64)> = Vec::new();
                    let mut x = U256::exp10(15);
                    while x <= top {
                        if let Some(p) = route.priced(x) {
                            points.push((weth(x) * 1e18, weth(p.output) * 1e18 - weth(x) * 1e18, weth(p.cost) * 1e18));
                        }
                        x = x * U256::from(5u64) / U256::from(4u64);
                    }
                    let slack = weth(route.rounding()) * 1e18;
                    for w in points.windows(2) {
                        cost_falls += u64::from(w[1].2 < w[0].2);
                    }
                    for w in points.windows(3) {
                        let line = w[0].1 + (w[2].1 - w[0].1) * (w[1].0 - w[0].0) / (w[2].0 - w[0].0);
                        // A float's own error at these magnitudes is far under a wei-scale slack of 2.
                        bends += u64::from(w[1].1 + slack + line.abs() * 1e-12 < line);
                    }
                    swept += points.len() as u64;
                }
            }
        }
    }

    let show = |name: &str, of: &dyn Fn(&&Case) -> bool| {
        let picked: Vec<&Case> = cases.iter().filter(of).collect();
        if picked.is_empty() {
            return;
        }
        let quotes = spread(picked.iter().map(|c| c.new_quotes).collect());
        let new = spread(picked.iter().map(|c| c.new_us).collect());
        let old = spread(picked.iter().map(|c| c.old_us).collect());
        let faster = spread(picked.iter().map(|c| c.old_us / c.new_us).collect());
        println!(
            "{name:<34} {:>5} cases | quotes p50 {:>3.0} p90 {:>3.0} p99 {:>3.0} max {:>3.0} | new us p50 {:>6.0} p90 {:>6.0} p99 {:>6.0} | old us p50 {:>6.0} p90 {:>6.0} p99 {:>6.0} | faster p50 {:>5.1}x",
            picked.len(), quotes.0, quotes.1, quotes.2, quotes.3, new.0, new.1, new.2, old.0, old.1, old.2, faster.0
        );
    };
    show("every case", &|_| true);
    show("a size pays", &|c| c.pays);
    show("no size pays", &|c| !c.pays);
    let mut pairs: Vec<String> = cases.iter().map(|c| c.venues.clone()).collect();
    pairs.sort();
    pairs.dedup();
    for pair in pairs {
        show(&pair, &|c| c.venues == pair);
    }
    let paying: Vec<&Case> = cases.iter().filter(|c| c.pays).collect();
    let sizes = spread(paying.iter().map(|c| c.new_size).collect());
    let reach = spread(paying.iter().map(|c| c.reach).collect());
    let old_quotes = spread(cases.iter().map(|c| c.old_quotes).collect());
    let spent = cases.iter().filter(|c| c.spent).count();
    println!("sizes that pay: p50 {:.3} p90 {:.1} p99 {:.1} max {:.1} WETH", sizes.0, sizes.1, sizes.2, sizes.3);
    println!("largest size asked over the size settled on, where a size pays: p50 {:.1} p90 {:.1} p99 {:.1} max {:.1}", reach.0, reach.1, reach.2, reach.3);
    println!("old search: quotes p50 {:.0} max {:.0}; the new search's budget ended {spent} of {} cases", old_quotes.0, old_quotes.3, cases.len());
    println!("assumptions over {swept} priced sizes: the cost fell {cost_falls} times; the gross sat under its neighbours' line by more than the rounding {bends} times");
    let col = |i: usize| spread(resized.iter().map(|r| r[i]).collect());
    let (pair_us, was_us, pair_quotes, was_quotes) = (col(0), col(1), col(2), col(3));
    let better = resized.iter().filter(|r| r[4] > 0.0).count();
    println!(
        "the economics stage's sizing, one pass, on {} paying cases (it runs four a candidate):",
        resized.len()
    );
    println!("  new pair: quotes p50 {:.0} p90 {:.0} max {:.0}; us p50 {:.0} p90 {:.0} p99 {:.0} max {:.0}", pair_quotes.0, pair_quotes.1, pair_quotes.3, pair_us.0, pair_us.1, pair_us.2, pair_us.3);
    println!("  old pair: quotes p50 {:.0} p90 {:.0} max {:.0}; us p50 {:.0} p90 {:.0} p99 {:.0} max {:.0}", was_quotes.0, was_quotes.1, was_quotes.3, was_us.0, was_us.1, was_us.2, was_us.3);
    println!("  the new pair nets strictly more on {better}; it is never under the old by more than the tolerance");

    // The targets (spec, "Testing"). Not loosened to fit.
    let quotes = spread(cases.iter().map(|c| c.new_quotes).collect());
    let unpaid = spread(cases.iter().filter(|c| !c.pays).map(|c| c.new_quotes).collect());
    let faster = spread(cases.iter().map(|c| c.old_us / c.new_us).collect());
    let (new, old) = (spread(cases.iter().map(|c| c.new_us).collect()), spread(cases.iter().map(|c| c.old_us).collect()));
    assert!(cases.len() > 1_000, "{} cases", cases.len());
    assert!(quotes.3 <= 64.0, "the most quotes on any case: {}", quotes.3);
    assert!(quotes.0 <= 32.0, "the median: {}", quotes.0);
    assert!(unpaid.0 <= 8.0, "the median where no size pays: {}", unpaid.0);
    assert!(faster.0 >= 3.0, "the median search is {:.1} times faster", faster.0);
    assert!(new.2 <= old.2, "the 99th percentile: {} us against {}", new.2, old.2);
    // R27.3: one pass of the stage is at most 64 quotes and, at the median, at
    // least five times faster than the pair it replaced.
    assert!(pair_quotes.3 <= 64.0, "the most quotes in a pass: {}", pair_quotes.3);
    assert!(was_us.0 >= 5.0 * pair_us.0, "a pass at the median: {} us against {}", pair_us.0, was_us.0);
    assert_eq!(cost_falls, 0, "a cost fell as its size grew");
    assert_eq!(bends, 0, "a gross sat further under its neighbours' line than the route's rounding");
}
