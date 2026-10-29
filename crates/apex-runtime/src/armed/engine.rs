//! The arming engine (R26): for every cycle and trigger pool, the rungs and
//! their predicates and calldata; the best `max_arms` armed, one virtual
//! sender each; a line per arm and per renewal. Pure: the shadow worker calls
//! [`Engine::tick`] and writes what it returns.

use super::bid::{best_bid, rank_value, Bid};
use super::lines::{Arm, ArmedLine, Word};
use super::moves::{spread_key, MoveStats};
use super::plan::{armed_call, armed_call2};
use apex_exec::lean2::Lean2Lender;
use super::predicate::{self, layout_of};
use super::threshold::{band_paying, rungs, trigger_up, Band, Pricing, Rung};
use crate::live::book::PoolSnapshot;
use crate::live::frontier::Cycle;
use crate::live::reads::ChainReads;
use alloy_primitives::{keccak256, Address};
use apex_chain::base::adapter::gas_limit_over;
use apex_types::cost::GasUsed;
use apex_search::frontier::RouteId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Slots repriced per pass at most, in the shadow. [`REPRICE_FOR`] is the
/// bound that binds: slots differ in cost, and one pass of 24 took 744 ms.
pub const REPRICE_BUDGET: usize = 256;

/// How long a pass goes on starting slots, in the shadow: the rest wait for
/// the next, and the arms priced so far go out.
///
/// The pass ran every 200 ms on one thread and was pegged: 12 h of CPU in the
/// 13 h R26.3 was up, 83 slots a second, and 58 once R27's sizes ran past
/// Balancer's holding. So it prices on [`workers`] threads, for this long.
pub const REPRICE_FOR: Duration = Duration::from_millis(150);

/// The pause before a pass when the last left nothing waiting. A flashblock's
/// writes land 200 ms apart, and a pass with nothing stale takes 0.4 ms.
pub const IDLE_PAUSE: Duration = Duration::from_millis(50);

/// The pause before the next pass, the last having left `waiting` stale slots:
/// none while any wait.
pub fn pause_after(waiting: u64) -> Duration {
    if waiting > 0 {
        Duration::ZERO
    } else {
        IDLE_PAUSE
    }
}

/// The threads a pass prices on: all but one of the machine's, at most six.
/// The capture path's own work is small beside a pass: on the host its threads
/// used about 2 s of CPU each in 13 minutes.
pub fn workers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, 6)
}

#[derive(Clone, Copy, Debug)]
pub struct EngineSettings {
    pub max_arms: usize,
    pub rungs: u8,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
    pub executor: Address,
    /// R37: `LeanExecutor2`. Set, every arm is built for it; unset, a USDC
    /// or v4 route is not armed (v1 runs neither).
    pub executor2: Option<Address>,
    pub chain_id: u64,
    /// Slots repriced per tick at most ([`REPRICE_BUDGET`] in the shadow).
    pub reprice_budget: usize,
    /// How long a pass goes on starting slots ([`REPRICE_FOR`] in the shadow).
    pub reprice_for: Duration,
    /// The threads a pass prices its slots on.
    pub workers: usize,
    /// R36: the most an arm bids, in wei per gas over the base fee.
    pub max_priority_wei_per_gas: u128,
}

impl EngineSettings {
    /// An arm's window in blocks: `window_s` at `block_time_ms`, rounded up.
    pub fn window_blocks(&self) -> u64 {
        (self.window_s * 1_000).div_ceil(self.block_time_ms.max(1)).max(1)
    }
}

type Key = (RouteId, u8, u8); // cycle, trigger leg, rung

/// A slot's rungs as last priced, with the pool versions they were priced at.
struct Computed {
    seqs: [u64; 2],
    at_ns: u64,
    /// Each rung with its band and its bid (R36): the band pays the bid's tip.
    rungs: Vec<(Rung, Band, Bid)>,
}

struct Live {
    arm: Arm,
    /// What decides "unchanged": step, size, mid and the band's prices.
    fingerprint: String,
}

struct State {
    moves: MoveStats,
    computed: BTreeMap<(RouteId, u8), Computed>,
    live: BTreeMap<Key, Live>,
    /// Each virtual sender's busy-until block; 0 is free.
    senders: Vec<u64>,
    next_arm_id: u64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct EngineStats {
    pub arms: u64,
    pub renewals: u64,
    pub slots_priced: u64,
    pub slots_armable: u64,
    pub unarmable_pools: u64,
    pub last_tick_us: u64,
    /// The slots the engine prices: each cycle, either pool the trigger.
    pub slots: u64,
    /// Of those, the slots priced at the book as the last pass saw it: the
    /// ones a sender can take.
    pub slots_current: u64,
    /// The stale slots the last pass left for the next.
    pub stale_waiting: u64,
    /// Passes so far, and over them: their time, the slots each left waiting,
    /// and the slots each found current.
    pub passes: u64,
    pub pass_us_total: u64,
    pub waiting_total: u64,
    pub current_total: u64,
}

/// The measured winners' bids per gas on a route, WETH-in pool first (R36).
pub type RouteBids = Arc<dyn Fn(Address, Address) -> Vec<u128> + Send + Sync>;

pub struct Engine {
    cycles: BTreeMap<RouteId, Cycle>,
    bids: RouteBids,
    settings: EngineSettings,
    unarmable: BTreeSet<Address>,
    state: Mutex<State>,
    arms: AtomicU64,
    renewals: AtomicU64,
    priced: AtomicU64,
    armable: AtomicU64,
    last_tick_us: AtomicU64,
    slots: u64,
    current: AtomicU64,
    waiting: AtomicU64,
    passes: AtomicU64,
    pass_us_total: AtomicU64,
    waiting_total: AtomicU64,
    current_total: AtomicU64,
}

/// A pool's position for move statistics: its tick, or log-price in bps.
fn position(p: &PoolSnapshot) -> f64 {
    match &p.reserves {
        Some(r) if !r.reserve0.is_zero() => (r.reserve1.as_u128() as f64 / r.reserve0.as_u128() as f64).ln() * 10_000.0,
        _ => f64::from(p.state.tick),
    }
}

/// The words an arm's predicates watch, as `p` holds them now.
fn words_of(p: &PoolSnapshot) -> Vec<Word> {
    let l = layout_of(p);
    let a = format!("{:#x}", l.address);
    let w = |slot: U256, v: U256| Word { address: a.clone(), slot: format!("{slot:#x}"), value: format!("{v:#x}") };
    match (&p.reserves, l.reserve_slots) {
        (Some(r), Some((s0, s1))) => vec![w(s0, r.reserve0), w(s1, r.reserve1)],
        _ => [l.price_slot.map(|s| w(s, p.state.sqrt_price_x96)), l.liquidity_slot.map(|s| w(s, U256::from(p.state.liquidity)))]
            .into_iter()
            .flatten()
            .collect(),
    }
}

impl Engine {
    pub fn new(cycles: BTreeMap<RouteId, Cycle>, settings: EngineSettings, unarmable: BTreeSet<Address>) -> Self {
        let state = State {
            moves: MoveStats::new(settings.window_s * 1_000_000_000),
            computed: BTreeMap::new(),
            live: BTreeMap::new(),
            senders: vec![0; settings.max_arms],
            next_arm_id: 0,
        };
        let slots = 2 * cycles.values().filter(|c| !c.legs.iter().any(|l| unarmable.contains(&l.pool))).count() as u64;
        Self {
            cycles,
            bids: Arc::new(|_, _| Vec::new()),
            settings,
            unarmable,
            state: Mutex::new(state),
            arms: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            priced: AtomicU64::new(0),
            armable: AtomicU64::new(0),
            last_tick_us: AtomicU64::new(0),
            slots,
            current: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            passes: AtomicU64::new(0),
            pass_us_total: AtomicU64::new(0),
            waiting_total: AtomicU64::new(0),
            current_total: AtomicU64::new(0),
        }
    }

    /// R36: the winners' bids each route's arms bid against. Without them an
    /// arm bids nothing and ranks as R26's did.
    #[must_use]
    pub fn with_bids(mut self, bids: RouteBids) -> Self {
        self.bids = bids;
        self
    }

    /// R36: the move statistics the ranking has learned, to save.
    pub fn moves_snapshot(&self, now_ns: u64) -> super::moves::MovesSnapshot {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).moves.snapshot(now_ns)
    }

    /// R36: move statistics saved by an earlier run, or seeded from logs.
    pub fn restore_moves(&self, snapshot: &super::moves::MovesSnapshot) -> Result<usize, String> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).moves.restore(snapshot)
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            arms: self.arms.load(Relaxed),
            renewals: self.renewals.load(Relaxed),
            slots_priced: self.priced.load(Relaxed),
            slots_armable: self.armable.load(Relaxed),
            unarmable_pools: self.unarmable.len() as u64,
            last_tick_us: self.last_tick_us.load(Relaxed),
            slots: self.slots,
            slots_current: self.current.load(Relaxed),
            stale_waiting: self.waiting.load(Relaxed),
            passes: self.passes.load(Relaxed),
            pass_us_total: self.pass_us_total.load(Relaxed),
            waiting_total: self.waiting_total.load(Relaxed),
            current_total: self.current_total.load(Relaxed),
        }
    }

    /// One pass: observe moves, reprice stale slots, arm the best, renew the
    /// unchanged. Returns the lines to write.
    pub fn tick(
        &self,
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        head: u64,
        base_fee_wei: u128,
        px: &Pricing,
        now_ns: u64,
    ) -> Vec<ArmedLine> {
        let started = std::time::Instant::now();
        let s = self.settings;
        let window_ns = s.window_s * 1_000_000_000;
        let mut guard = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = &mut *guard;
        for p in snapshot.values() {
            st.moves.observe(p.spec.pool, position(p), now_ns);
        }
        // R36: each armed route's spread, both ways: what a fire needs to move.
        for c in self.cycles.values() {
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            let (x, y) = (position(a), position(b));
            st.moves.observe(spread_key(a.spec.pool, b.spec.pool), x - y, now_ns);
            st.moves.observe(spread_key(b.spec.pool, a.spec.pool), y - x, now_ns);
        }

        // Reprice stale slots within the budget. A slot is stale when either
        // pool changed or its window has passed. One whose pool changed is not
        // armed until it is repriced, while one that only aged still is, on
        // its last pricing: so the changed go first, then the oldest.
        let mut stale: Vec<((RouteId, u8), (bool, u64))> = Vec::new();
        for (id, c) in &self.cycles {
            if c.legs.iter().any(|l| self.unarmable.contains(&l.pool)) {
                continue;
            }
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            for leg in 0..2u8 {
                let k = st.computed.get(&(*id, leg));
                let current = k.is_some_and(|k| k.seqs == [a.seq, b.seq]);
                let fresh = current && k.is_some_and(|k| now_ns < k.at_ns.saturating_add(window_ns));
                if !fresh {
                    stale.push(((*id, leg), (current, k.map_or(0, |k| k.at_ns))));
                }
            }
        }
        stale.sort_by_key(|(_, order)| *order);
        let work: Vec<(RouteId, u8)> = stale.iter().take(s.reprice_budget).map(|(slot, _)| *slot).collect();
        let done = self.reprice(&work, snapshot, px, now_ns, started);
        let waiting = (stale.len() - done.len()) as u64;
        self.priced.fetch_add(done.len() as u64, Relaxed);
        st.computed.extend(done);
        self.armable.store(st.computed.values().filter(|k| !k.rungs.is_empty()).count() as u64, Relaxed);

        // Rank every rung priced at this book: P(the route's spread moves ≥
        // step within the window) × the surplus its plan expects. A rung priced before either
        // pool changed is left out until it is repriced: its trigger and band
        // are prices the pools may have left, so it is neither armed nor renewed.
        let moves = &st.moves;
        let mut ranked: Vec<(f64, Key)> = Vec::new();
        let mut current = 0u64;
        for ((id, leg), k) in &st.computed {
            let c = &self.cycles[id];
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            if k.seqs != [a.seq, b.seq] {
                continue;
            }
            current += 1;
            let (pool, other) = (c.legs[usize::from(*leg)].pool, c.legs[1 - usize::from(*leg)].pool);
            for (r, b, bid) in &k.rungs {
                // R36: the chance the route's spread moves past the rung, not the
                // trigger pool alone: the market moving both pools fires nothing.
                let p = moves.p_at_least(spread_key(pool, other), trigger_up(c, usize::from(*leg)), r.step);
                // R36: the chance of a move past the rung, of the bid winning,
                // and what a win keeps after its tip (the band already pays the
                // tip). A rung whose bid outbids no one takes no sender. A
                // move's chance may be 0 for a pool not yet seen moving: it
                // orders the arms, as in R26, and excludes none.
                // R37: ranked in wei, whatever the cycle's start token.
                let Some(pxc) = px.for_cycle(c) else { continue };
                if bid.p_win > 0.0 {
                    ranked.push((rank_value(p, bid, pxc.in_wei(b.point.surplus), b.point.gas.expected.0), (*id, *leg, r.index)));
                }
            }
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

        // Free the senders whose arms' windows have passed. A head is a sealed
        // block, so a window ends when its last block is the head: freed then,
        // a sender's next transaction can be valid from the next block.
        for busy in &mut st.senders {
            if *busy <= head {
                *busy = 0;
            }
        }
        let to_block = head + s.window_blocks();
        let mut out = Vec::new();
        for (_, key) in ranked.into_iter().take(s.max_arms) {
            let (id, leg, index) = key;
            let c = &self.cycles[&id];
            let Some((rung, band, bid)) = st.computed[&(id, leg)].rungs.iter().find(|(r, _, _)| r.index == index) else { continue };
            let fingerprint = format!(
                "{}:{:#x}:{:#x}:{:#x}:{:#x}",
                rung.step,
                band.point.size,
                band.point.mid,
                band.edges[0].state.sqrt_price_x96,
                band.edges[1].state.sqrt_price_x96
            );
            if let Some(live) = st.live.get_mut(&key) {
                if live.fingerprint == fingerprint {
                    if live.arm.to_block <= head {
                        // A new transaction on the same sender: re-signed with a
                        // deadline that runs from now, as the arm's did.
                        let deadline = self.deadline(now_ns);
                        // Renewed on the cycle's own pricing, its lender too,
                        // as it was armed (R37).
                        let Some(pxc) = px.for_cycle(c) else { continue };
                        let Some((calldata, calldata_hash, _)) = self.calldata(c, band, snapshot, deadline, &pxc) else { continue };
                        live.arm.to_block = to_block;
                        live.arm.deadline = deadline;
                        live.arm.calldata.clone_from(&calldata);
                        live.arm.calldata_hash = calldata_hash;
                        st.senders[usize::from(live.arm.sender_slot)] = to_block;
                        self.renewals.fetch_add(1, Relaxed);
                        out.push(ArmedLine::Renew {
                            arm_id: live.arm.arm_id,
                            to_block,
                            at_ns: now_ns,
                            deadline: Some(deadline),
                            calldata: Some(calldata),
                        });
                    }
                    continue;
                }
                if live.arm.to_block > head {
                    continue; // changed, but its sender is busy until its window ends
                }
            }
            let Some(sender) = st.senders.iter().position(|b| *b == 0) else { break };
            let Some(arm) = self.arm(c, leg, (rung, band, bid), snapshot, (head, to_block), base_fee_wei, px, now_ns, sender, st.next_arm_id) else {
                continue;
            };
            st.next_arm_id += 1;
            st.senders[sender] = to_block;
            self.arms.fetch_add(1, Relaxed);
            out.push(ArmedLine::Arm(Box::new(arm.clone())));
            st.live.insert(key, Live { arm, fingerprint });
        }
        let took = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.last_tick_us.store(took, Relaxed);
        self.current.store(current, Relaxed);
        self.waiting.store(waiting, Relaxed);
        self.passes.fetch_add(1, Relaxed);
        self.pass_us_total.fetch_add(took, Relaxed);
        self.waiting_total.fetch_add(waiting, Relaxed);
        self.current_total.fetch_add(current, Relaxed);
        out
    }

    /// `work`'s slots priced at `snapshot`, in order, on the pass's threads,
    /// until the pass's time is up. The first is always priced, or a book too
    /// slow for the time would never be priced at all.
    fn reprice(
        &self,
        work: &[(RouteId, u8)],
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        px: &Pricing,
        now_ns: u64,
        started: std::time::Instant,
    ) -> Vec<((RouteId, u8), Computed)> {
        let s = self.settings;
        let next = AtomicUsize::new(0);
        let price = || {
            let mut out = Vec::new();
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= work.len() || (i > 0 && started.elapsed() >= s.reprice_for) {
                    return out;
                }
                let (id, leg) = work[i];
                let c = &self.cycles[&id];
                let pair = [Arc::clone(&snapshot[&c.legs[0].pool]), Arc::clone(&snapshot[&c.legs[1].pool])];
                let bids = (self.bids)(c.legs[0].pool, c.legs[1].pool);
                // R37: priced in the cycle's own start token; a USDC cycle
                // without a rate is priced at nothing.
                let found: Vec<(Rung, Band, Bid)> = match px.for_cycle(c) {
                    Some(pxc) => rungs(c, &pair, usize::from(leg), &pxc, s.rungs)
                        .into_iter()
                        .filter_map(|r| bid_and_band(c, &pair, usize::from(leg), &r, &pxc, &s, &bids).map(|(b, bid)| (r, b, bid)))
                        .collect(),
                    None => Vec::new(),
                };
                out.push(((id, leg), Computed { seqs: [pair[0].seq, pair[1].seq], at_ns: now_ns, rungs: found }));
            }
        };
        let threads = s.workers.clamp(1, work.len().max(1));
        if threads == 1 {
            return price();
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads).map(|_| scope.spawn(price)).collect();
            // A thread that panicked loses its slots to the next pass.
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn arm(
        &self,
        c: &Cycle,
        leg: u8,
        (rung, band, bid): (&Rung, &Band, &Bid),
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        (head, to_block): (u64, u64),
        base_fee_wei: u128,
        px: &Pricing,
        now_ns: u64,
        sender: usize,
        arm_id: u64,
    ) -> Option<Arm> {
        let s = self.settings;
        let (t, o) = (usize::from(leg), 1 - usize::from(leg));
        let trigger_pool = snapshot.get(&c.legs[t].pool)?;
        let other_pool = snapshot.get(&c.legs[o].pool)?;
        let mut predicates = predicate::trigger(trigger_pool, &rung.trigger, trigger_up(c, t))?;
        predicates.extend(predicate::band(other_pool, &band.edges, s.liquidity_floor_bps)?);
        predicates.push(predicate::expiry(to_block));
        let deadline = self.deadline(now_ns);
        let point = &band.point;
        let px = &px.for_cycle(c)?;
        let (calldata, calldata_hash, lender) = self.calldata(c, band, snapshot, deadline, px)?;
        let start_ticks = [trigger_pool, other_pool]
            .iter()
            .filter(|p| p.reserves.is_none())
            .map(|p| (format!("{:#x}", p.spec.pool), p.state.tick))
            .collect();
        Some(Arm {
            arm_id,
            at_ns: now_ns,
            armed_block: head,
            to_block,
            route_hash: format!("{:#x}", c.commitment.route_hash),
            trigger_leg: leg,
            rung: rung.index,
            step: rung.step,
            pools: [format!("{:#x}", c.legs[0].pool), format!("{:#x}", c.legs[1].pool)],
            venues: [format!("{:?}", c.legs[0].venue), format!("{:?}", c.legs[1].venue)],
            predicates,
            start_words: [words_of(trigger_pool), words_of(other_pool)].concat(),
            start_ticks,
            size_wei: point.size.to_string(),
            mid: point.mid.to_string(),
            output_wei: point.output.to_string(),
            surplus_wei: point.surplus.to_string(),
            cost_wei: point.cost.to_string(),
            gas_expected: point.gas.expected.0,
            // The fire lies past the threshold the ceiling was priced at: fires
            // used up to 7.4% over it in the first 10 hours, and four ran out
            // at 1.25x it. So the shared formula, refunds included, goes over
            // 1.25x the ceiling. Unused limit is not charged on Base.
            gas_limit: gas_limit_over(GasUsed(point.gas.ceiling.0.saturating_mul(5) / 4)).0,
            base_fee_wei: base_fee_wei.to_string(),
            other_wei: px.costs.other_wei.to_string(),
            deadline,
            calldata,
            calldata_hash,
            sender_slot: u16::try_from(sender).unwrap_or(u16::MAX),
            lender,
            bid_wei_per_gas: bid.per_gas.to_string(),
            p_win: bid.p_win,
            renewals: Vec::new(),
            start: (c.start != crate::live::frontier::WETH).then_some(c.start),
            executor: Some(self.settings.executor2.unwrap_or(self.settings.executor)),
            start_per_eth: px.rate.map(|(num, den)| [num.to_string(), den.to_string()]),
        })
    }

    /// An arm's or renewal's deadline in seconds: its window, plus one.
    fn deadline(&self, now_ns: u64) -> u64 {
        now_ns / 1_000_000_000 + self.settings.window_s + 1
    }

    /// The plan at `band`'s point with `deadline`, as calldata, its hash, and
    /// the lender it borrows from: Balancer while it covers the size, then the
    /// Morpho lender (R27).
    fn calldata(
        &self,
        c: &Cycle,
        band: &Band,
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        deadline: u64,
        px: &Pricing,
    ) -> Option<(String, String, String)> {
        let s = self.settings;
        // Both pools held: an arm on a cycle the book no longer has is not built.
        snapshot.get(&c.legs[0].pool)?;
        snapshot.get(&c.legs[1].pool)?;
        let choice = px.lenders.and_then(|l| l.for_size(band.point.size)).unwrap_or(crate::live::lenders::MORPHO);
        let morpho = choice.id == crate::live::lenders::MORPHO_FLASH;
        let (call, lender) = match s.executor2 {
            // R37: a v4 route's choice is PoolManager's holding, and
            // `armed_call2` borrows from PoolManager whatever it is passed.
            Some(executor2) => {
                let asked = if morpho { Lean2Lender::Morpho } else { Lean2Lender::Balancer };
                let call = armed_call2(c, band.point.size, band.point.mid, band.point.output, executor2, s.chain_id, deadline, asked).ok()?;
                let lender = match call.plan() {
                    apex_exec::call::CallPlan::Lean2(p) if p.lender == Lean2Lender::PoolManager => "pool_manager",
                    _ if morpho => "morpho",
                    _ => "balancer",
                };
                (call, lender)
            }
            None => (
                armed_call(c, band.point.size, band.point.mid, band.point.output, s.executor, s.chain_id, deadline, &choice).ok()?,
                if morpho { "morpho" } else { "balancer" },
            ),
        };
        let data = call.data();
        Some((format!("0x{}", alloy_primitives::hex::encode(data)), format!("{:#x}", keccak256(data)), lender.into()))
    }
}

/// R36: a rung's bid against the route's measured winners (`bids`; none
/// measured bids nothing and ranks as R26's did), and its band paying that
/// bid's tip at both edges.
pub fn bid_and_band(
    cycle: &Cycle,
    pair: &[Arc<PoolSnapshot>; 2],
    leg: usize,
    rung: &Rung,
    px: &Pricing,
    s: &EngineSettings,
    bids: &[u128],
) -> Option<(Band, Bid)> {
    let gas = rung.point.gas.expected.0;
    let bid = if bids.is_empty() {
        Bid { per_gas: 0, p_win: 1.0 }
    } else {
        // R37: winners bid wei a gas, so the surplus is measured in wei.
        best_bid(bids, px.in_wei(rung.point.surplus), gas, s.max_priority_wei_per_gas)
    };
    // And the band pays the tip in the cycle's start token.
    let tip = px.in_start(U256::from(bid.per_gas).saturating_mul(U256::from(gas)));
    band_paying(cycle, pair, leg, rung, px, s.band_cap_bps, tip).map(|b| (b, bid))
}

/// Every pool whose watched words do not read back, at `block`, as the book
/// loaded them: their arms would watch the wrong storage, so they are left
/// out (R26). Run at boot, on the freshly loaded book.
pub async fn verify_layouts(
    reads: &ChainReads,
    snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
    block: u64,
) -> BTreeSet<Address> {
    let mut bad = BTreeSet::new();
    for p in snapshot.values() {
        let l = layout_of(p);
        let a = l.address;
        let ok = match (&p.reserves, l.reserve_slots, l.price_slot, l.liquidity_slot) {
            (Some(r), Some((s0, s1)), _, _) => matches!(
                (reads.storage_at(a, s0, block).await, reads.storage_at(a, s1, block).await),
                (Ok(w0), Ok(w1)) if w0 == r.reserve0 && w1 == r.reserve1
            ),
            (None, _, Some(ps), Some(ls)) => matches!(
                (reads.storage_at(a, ps, block).await, reads.storage_at(a, ls, block).await),
                (Ok(w), Ok(lw)) if (w & predicate::price_mask()) == p.state.sqrt_price_x96
                    && (lw & predicate::liquidity_mask()) == U256::from(p.state.liquidity)
            ),
            _ => false,
        };
        if !ok {
            bad.insert(p.spec.pool);
        }
    }
    bad
}
