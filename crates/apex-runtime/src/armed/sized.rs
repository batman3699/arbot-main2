//! The sized arming engine (Task 8.5 R40.1): standing arms for
//! `LeanExecutor3`, which sizes each trade itself at execution. Beside R39's
//! [`super::engine::Engine`], not in place of it: both run on one feed until
//! this one is flipped live.
//!
//! # What differs from R39
//!
//! - **A slot is priced through the mirror** ([`sized_points`]): a threshold
//!   at the band's adverse edge, no size, and a second tier when it can pay.
//! - **A candidate's value is its expected net**: the mirror's net across the
//!   move distribution, times the contest's chance, less the expected cost of
//!   `NoTrade`s and reverts.
//! - **The fill is greedy on marginal net** ([`fill`]). Arms sharing a
//!   trigger pool and direction are a group: one move makes them all
//!   eligible. A second route in a group is worth what is left after
//!   replaying the trades chosen before it on the book, and a tier 2 only
//!   what it adds to its tier 1.
//! - **A sample's zero is not evidence of zero**: the chance of winning is
//!   floored at [`P_WIN_FLOOR`], and of the move at [`P_MOVE_FLOOR`], so an
//!   unmeasured route ranks last instead of being left out. An arm costs
//!   nothing unless it pays, and routes never armed were 54% of the gap.
//! - **The fingerprint** is the threshold, the band and the bid. Size is the
//!   executor's, so a pool's liquidity moving doesn't re-arm.

use super::bid::{BidModel, ShareSample};
use super::engine::{position, words_of, BidInputs, EngineStats, RouteBids};
use super::lines::{Arm, ArmedLine, SizedLine, StartAt};
use super::moves::{spread_key, MoveStats};
use super::plan::{sized_call, sized_trade, SizedOutcome, SizedTerms};
use super::predicate;
use super::threshold::{expected_net, sized_points, trigger_up, Pricing, SizedArmPoint, BAND_WIDTHS, TIER1_BID_STEPS};
use crate::live::book::PoolSnapshot;
use crate::live::frontier::Cycle;
use crate::live::gas::{LEAN, SIZED};
use alloy_primitives::{keccak256, Address};
use apex_exec::lean2::Lean2Lender;
use apex_search::frontier::RouteId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The L2 gas price oracle, whose `getL1FeeUpperBound` a sized plan's L1
/// bound comes from.
pub const GAS_PRICE_ORACLE: Address = alloy_primitives::address!("420000000000000000000000000000000000000F");
/// A sized arm's transaction, unsigned, in bytes: a 162-byte plan's calldata
/// (260) and the envelope, rounded up.
pub const SIZED_TX_BYTES: u64 = 512;

/// `getL1FeeUpperBound(uint256)`'s calldata for a transaction of `bytes`.
pub fn l1_upper_call(bytes: u64) -> Vec<u8> {
    let mut data = keccak256("getL1FeeUpperBound(uint256)")[..4].to_vec();
    let mut word = [0u8; 32];
    U256::from(bytes).to_big_endian(&mut word);
    data.extend_from_slice(&word);
    data
}

/// The oracle's answer as wei; None for one that is not a word, or past 128 bits.
pub fn l1_upper_of(answer: &[u8]) -> Option<u128> {
    let word: [u8; 32] = answer.get(..32)?.try_into().ok()?;
    let v = U256::from_big_endian(&word);
    (v.bits() <= 128).then(|| v.as_u128())
}

/// The least chance of winning a candidate is valued at.
pub const P_WIN_FLOOR: f64 = 0.001;
/// The least chance of a move a candidate is valued at: a pool or spread not
/// yet seen moving that far is not known never to.
pub const P_MOVE_FLOOR: f64 = 0.000_1;

#[derive(Clone, Copy, Debug)]
pub struct SizedSettings {
    pub max_senders: usize,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
    pub tier2_multiple: u32,
    pub executor3: Address,
    pub chain_id: u64,
    pub max_size_wei: u128,
    pub max_size_usdc: u128,
    pub max_priority_wei_per_gas: u128,
    pub min_net_wei: u64,
    pub max_steps: u8,
    /// The priors a candidate's value is charged with, in parts per million of
    /// its fires.
    pub p_no_trade_ppm: u32,
    pub p_revert_ppm: u32,
    /// How long a pass goes on pricing slots, and on how many threads.
    pub reprice_for: Duration,
    pub workers: usize,
    pub l1_fee_bound_wei: u128,
}

impl SizedSettings {
    pub fn window_blocks(&self) -> u64 {
        (self.window_s * 1_000).div_ceil(self.block_time_ms.max(1)).max(1)
    }

    pub fn max_size(&self, start: Address) -> U256 {
        U256::from(if start == apex_exec::lean2::USDC { self.max_size_usdc } else { self.max_size_wei })
    }
}

/// A cycle, its trigger leg, and the tier.
pub type SizedKey = (RouteId, u8, u8);

/// One arm the fill can choose.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub key: SizedKey,
    /// The trigger pool, and whether the trigger's price rises.
    pub group: (Address, bool),
    pub ev: f64,
    pub worst_wei: u128,
}

/// The greedy fill: indices of the chosen candidates, in the order chosen.
/// Each pick is the candidate with the greatest marginal value per unit of
/// what it holds (`1 / senders` of the senders, `worst_wei / budget_wei` of
/// the budget); only the group it was taken from is re-valued, by
/// `marginal(candidate, chosen in its group)`. It stops at no positive value,
/// or when senders or budget run out. Every arm holds a sender for one window,
/// so sender-time needs no term of its own.
pub fn fill(
    cands: &[Candidate],
    senders: usize,
    budget_wei: u128,
    mut marginal: impl FnMut(&Candidate, &[usize]) -> f64,
) -> Vec<usize> {
    let mut value: Vec<f64> = cands.iter().map(|c| c.ev).collect();
    let mut chosen: Vec<usize> = Vec::new();
    let mut left = budget_wei;
    let density = |v: f64, c: &Candidate| v / (1.0 / senders.max(1) as f64 + c.worst_wei as f64 / budget_wei.max(1) as f64);
    while chosen.len() < senders {
        let best = (0..cands.len())
            .filter(|i| !chosen.contains(i) && value[*i] > 0.0 && cands[*i].worst_wei <= left)
            .max_by(|a, b| density(value[*a], &cands[*a]).total_cmp(&density(value[*b], &cands[*b])).then(b.cmp(a)));
        let Some(i) = best else { break };
        chosen.push(i);
        left -= cands[i].worst_wei;
        let group = cands[i].group;
        let in_group: Vec<usize> = chosen.iter().copied().filter(|j| cands[*j].group == group).collect();
        for j in 0..cands.len() {
            if cands[j].group == group && !chosen.contains(&j) {
                value[j] = marginal(&cands[j], &in_group);
            }
        }
    }
    chosen
}

/// A slot as last priced: the pools' versions, the band chosen, its points,
/// and the cap and lender its plans carry.
struct Priced {
    seqs: [u64; 2],
    at_ns: u64,
    points: Vec<SizedArmPoint>,
    terms: SizedTerms,
    lender: Lean2Lender,
}

struct Live {
    arm: Arm,
    fingerprint: String,
}

struct State {
    moves: MoveStats,
    priced: BTreeMap<(RouteId, u8), Priced>,
    live: BTreeMap<SizedKey, Live>,
    /// Each virtual sender's busy-until block; 0 is free.
    senders: Vec<u64>,
    next_arm_id: u64,
}

pub struct SizedEngine {
    cycles: BTreeMap<RouteId, Cycle>,
    bids: RouteBids,
    contest: Option<Arc<ShareSample>>,
    bid_model: BidModel,
    settings: SizedSettings,
    unarmable: BTreeSet<Address>,
    state: Mutex<State>,
    arms: AtomicU64,
    renewals: AtomicU64,
    priced: AtomicU64,
    armable: AtomicU64,
    last_tick_us: AtomicU64,
}

impl SizedEngine {
    pub fn new(cycles: BTreeMap<RouteId, Cycle>, settings: SizedSettings, unarmable: BTreeSet<Address>) -> Self {
        let state = State {
            moves: MoveStats::new(settings.window_s * 1_000_000_000),
            priced: BTreeMap::new(),
            live: BTreeMap::new(),
            senders: vec![0; settings.max_senders],
            next_arm_id: 0,
        };
        Self {
            cycles,
            bids: Arc::new(|_, _| Vec::new()),
            contest: None,
            bid_model: BidModel::BestBid,
            settings,
            unarmable,
            state: Mutex::new(state),
            arms: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            priced: AtomicU64::new(0),
            armable: AtomicU64::new(0),
            last_tick_us: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn with_bids(mut self, bids: RouteBids) -> Self {
        self.bids = bids;
        self
    }

    #[must_use]
    pub fn with_contest(mut self, contest: Option<Arc<ShareSample>>, model: BidModel) -> Self {
        self.contest = contest;
        self.bid_model = model;
        self
    }

    /// The settings the engine was built with.
    pub fn settings_for_test(&self) -> &SizedSettings {
        &self.settings
    }

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
            ..EngineStats::default()
        }
    }

    /// The plan terms and lender for `c`: the size cap, or the largest
    /// holding when no lender holds it.
    fn terms_for(&self, c: &Cycle, px: &Pricing, l1_upper_wei: u128) -> (SizedTerms, Lean2Lender) {
        let s = self.settings;
        let max = s.max_size(c.start);
        let (cap, lender, extra) = match px.lenders {
            Some(l) => match l.for_size(max) {
                Some(choice) => (max, choice, choice.extra_gas),
                None => {
                    let (held, choice) = [(l.morpho.unwrap_or_default(), crate::live::lenders::MORPHO), (l.balancer, crate::live::lenders::BALANCER)]
                        .into_iter()
                        .max_by_key(|(h, _)| *h)
                        .unwrap_or((U256::zero(), crate::live::lenders::MORPHO));
                    (held.min(max), choice, choice.extra_gas)
                }
            },
            None => (max, crate::live::lenders::MORPHO, 0),
        };
        let lender = if lender.id == crate::live::lenders::MORPHO_FLASH { Lean2Lender::Morpho } else { Lean2Lender::Balancer };
        let read = SIZED.read.of(c.legs[0].venue) + SIZED.read.of(c.legs[1].venue);
        let swap = LEAN.hop.of(c.legs[0].venue) + LEAN.hop.of(c.legs[1].venue) + extra;
        // The plan's L1 bound is twice the oracle's; the reservation's never
        // less than the configured bound (`worst`).
        let terms = SizedTerms {
            cap,
            rate: px.rate,
            overhead: u32::try_from(SIZED.overhead).unwrap_or(u32::MAX),
            swap_gas: u32::try_from(swap).unwrap_or(u32::MAX),
            l1_bound: u64::try_from(l1_upper_wei.saturating_mul(2).max(1)).unwrap_or(u64::MAX),
            min_net: s.min_net_wei,
            max_steps: s.max_steps,
            read_gas: read,
        };
        (terms, lender)
    }

    /// The band, of [`BAND_WIDTHS`] within the cap, with the best chance of a
    /// fire inside it: the move must reach a threshold the band's width
    /// raises, while the other pool stays inside it.
    fn band_for(&self, moves: &MoveStats, c: &Cycle, leg: usize, step_at_5: u32) -> u32 {
        let (pool, other) = (c.legs[leg].pool, c.legs[1 - leg].pool);
        let up = trigger_up(c, leg);
        BAND_WIDTHS
            .into_iter()
            .filter(|b| *b <= self.settings.band_cap_bps)
            .map(|b| {
                let fire = moves.p_at_least(spread_key(pool, other), up, step_at_5 + b - BAND_WIDTHS[0]);
                let leaves = (moves.p_at_least(other, true, b) + moves.p_at_least(other, false, b)).min(1.0);
                (b, fire * (1.0 - leaves))
            })
            .max_by(|x, y| x.1.total_cmp(&y.1).then(y.0.cmp(&x.0)))
            .map_or(BAND_WIDTHS[0], |(b, _)| b)
    }

    fn price_slot(&self, c: &Cycle, leg: u8, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>, px: &Pricing, l1_upper_wei: u128, moves: &MoveStats) -> Option<Priced> {
        let pair = [Arc::clone(snapshot.get(&c.legs[0].pool)?), Arc::clone(snapshot.get(&c.legs[1].pool)?)];
        let pxc = px.for_cycle(c)?;
        let (terms, lender) = self.terms_for(c, &pxc, l1_upper_wei);
        let winners = (self.bids)(c.legs[0].pool, c.legs[1].pool);
        let inputs = BidInputs { winners: &winners, sample: self.contest.as_deref(), model: self.bid_model };
        let cap = self.settings.max_priority_wei_per_gas;
        let l = usize::from(leg);
        let at5 = sized_points(c, &pair, l, &pxc, &terms, BAND_WIDTHS[0], self.settings.tier2_multiple, &inputs, cap);
        let band = at5.first().map_or(BAND_WIDTHS[0], |p| self.band_for(moves, c, l, p.step));
        let points =
            if band == BAND_WIDTHS[0] { at5 } else { sized_points(c, &pair, l, &pxc, &terms, band, self.settings.tier2_multiple, &inputs, cap) };
        Some(Priced { seqs: [pair[0].seq, pair[1].seq], at_ns: 0, points, terms, lender })
    }

    /// One pass: observe moves, reprice stale slots, choose by marginal net,
    /// arm the chosen and renew the unchanged. `budget_left_wei` is what the
    /// day's budget has left for pending worst cases; `l1_upper_wei` the
    /// oracle's L1 fee bound for a plan's transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &self,
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        head: u64,
        base_fee_wei: u128,
        px: &Pricing,
        now_ns: u64,
        budget_left_wei: u128,
        l1_upper_wei: u128,
    ) -> Vec<ArmedLine> {
        let started = std::time::Instant::now();
        let s = self.settings;
        let window_ns = s.window_s * 1_000_000_000;
        let mut guard = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = &mut *guard;
        for p in snapshot.values() {
            st.moves.observe(p.spec.pool, position(p), now_ns);
        }
        for c in self.cycles.values() {
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            let (x, y) = (position(a), position(b));
            st.moves.observe(spread_key(a.spec.pool, b.spec.pool), x - y, now_ns);
            st.moves.observe(spread_key(b.spec.pool, a.spec.pool), y - x, now_ns);
        }

        // Reprice: the changed first, then the oldest, while the pass has time.
        let mut stale: Vec<((RouteId, u8), (bool, u64))> = Vec::new();
        for (id, c) in &self.cycles {
            if c.legs.iter().any(|l| self.unarmable.contains(&l.pool)) {
                continue;
            }
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            for leg in 0..2u8 {
                let k = st.priced.get(&(*id, leg));
                let current = k.is_some_and(|k| k.seqs == [a.seq, b.seq]);
                let fresh = current && k.is_some_and(|k| now_ns < k.at_ns.saturating_add(window_ns));
                if !fresh {
                    stale.push(((*id, leg), (current, k.map_or(0, |k| k.at_ns))));
                }
            }
        }
        stale.sort_by_key(|(_, order)| *order);
        let work: Vec<(RouteId, u8)> = stale.into_iter().map(|(slot, _)| slot).collect();
        // On the pass's threads, while it has time; the first slot always, or
        // a book too slow for the time would never be priced at all.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let moves = &st.moves;
        let price = || {
            let mut done = Vec::new();
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= work.len() || (i > 0 && started.elapsed() >= s.reprice_for) {
                    return done;
                }
                let (id, leg) = work[i];
                if let Some(mut p) = self.price_slot(&self.cycles[&id], leg, snapshot, px, l1_upper_wei, moves) {
                    p.at_ns = now_ns;
                    done.push(((id, leg), p));
                }
            }
        };
        let threads = s.workers.clamp(1, work.len().max(1));
        let done: Vec<((RouteId, u8), Priced)> = if threads == 1 {
            price()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads).map(|_| scope.spawn(price)).collect();
                // A thread that panicked loses its slots to the next pass.
                handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
            })
        };
        self.priced.fetch_add(done.len() as u64, Relaxed);
        st.priced.extend(done);
        self.armable.store(st.priced.values().filter(|p| !p.points.is_empty()).count() as u64, Relaxed);

        // Candidates: every point of every slot priced at this book.
        let mut cands: Vec<Candidate> = Vec::new();
        for ((id, leg), p) in &st.priced {
            let c = &self.cycles[id];
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            if p.seqs != [a.seq, b.seq] {
                continue;
            }
            let l = usize::from(*leg);
            let (pool, other) = (c.legs[l].pool, c.legs[1 - l].pool);
            let up = trigger_up(c, l);
            let chance = |step: u32| st.moves.p_at_least(spread_key(pool, other), up, step).max(P_MOVE_FLOOR);
            for point in &p.points {
                cands.push(Candidate {
                    key: (*id, *leg, point.tier),
                    group: (pool, up),
                    ev: self.value(point, p, base_fee_wei, &chance),
                    worst_wei: self.worst(point, base_fee_wei, l1_upper_wei),
                });
            }
        }
        // The set to hold this window: the arms already pending are in it, on
        // their own senders and reservations, so the fill counts them all.
        let chosen = {
            let priced = &st.priced;
            let moves = &st.moves;
            fill(&cands, s.max_senders, budget_left_wei.max(1), |cand, in_group| {
                self.marginal(cand, in_group.iter().map(|i| &cands[*i]), priced, moves, snapshot, px, base_fee_wei)
            })
        };

        for busy in &mut st.senders {
            if *busy <= head {
                *busy = 0;
            }
        }
        let to_block = head + s.window_blocks();
        let mut out = Vec::new();
        for i in chosen {
            let key = cands[i].key;
            let (id, leg, tier) = key;
            let c = &self.cycles[&id];
            let Some(p) = st.priced.get(&(id, leg)) else { continue };
            let Some(point) = p.points.iter().find(|x| x.tier == tier) else { continue };
            let fingerprint = format!(
                "{}:{}:{:#x}:{:#x}:{}",
                point.tier,
                point.step,
                point.edges[0].state.sqrt_price_x96,
                point.edges[1].state.sqrt_price_x96,
                point.bid.sent.per_gas
            );
            if let Some(live) = st.live.get_mut(&key) {
                if live.fingerprint == fingerprint {
                    if live.arm.to_block <= head {
                        let deadline = now_ns / 1_000_000_000 + s.window_s + 1;
                        let Ok(call) = sized_call(c, snapshot, &p.terms, s.executor3, s.chain_id, deadline, p.lender, &px.gas) else { continue };
                        let calldata = format!("0x{}", alloy_primitives::hex::encode(call.data()));
                        live.arm.to_block = to_block;
                        live.arm.deadline = deadline;
                        live.arm.calldata.clone_from(&calldata);
                        live.arm.calldata_hash = format!("{:#x}", keccak256(call.data()));
                        st.senders[usize::from(live.arm.sender_slot)] = to_block;
                        self.renewals.fetch_add(1, Relaxed);
                        out.push(ArmedLine::Renew { arm_id: live.arm.arm_id, to_block, at_ns: now_ns, deadline: Some(deadline), calldata: Some(calldata) });
                    }
                    continue;
                }
                if live.arm.to_block > head {
                    continue;
                }
            }
            let Some(sender) = st.senders.iter().position(|b| *b == 0) else { break };
            let Some(arm) = self.arm(c, leg, point, p, snapshot, (head, to_block), base_fee_wei, px, now_ns, sender, st.next_arm_id) else { continue };
            st.next_arm_id += 1;
            st.senders[sender] = to_block;
            self.arms.fetch_add(1, Relaxed);
            out.push(ArmedLine::Arm(Box::new(arm.clone())));
            st.live.insert(key, Live { arm, fingerprint });
        }
        self.last_tick_us.store(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX), Relaxed);
        out
    }

    /// A candidate's expected net, in wei: its net across the move, times its
    /// chance of winning, less what its `NoTrade`s and reverts are expected to
    /// cost.
    fn value(&self, point: &SizedArmPoint, p: &Priced, base_fee_wei: u128, chance: &impl Fn(u32) -> f64) -> f64 {
        let s = self.settings;
        let won = point.bid.sent.p_win.max(P_WIN_FLOOR) * expected_net(&point.grid, chance);
        let price = (base_fee_wei + point.bid.sent.per_gas) as f64;
        let no_trade = p.terms.read_gas as f64 * price + p.terms.l1_bound as f64;
        let revert = (p.terms.read_gas + u64::from(p.terms.swap_gas)) as f64 * price + p.terms.l1_bound as f64;
        let costs = f64::from(s.p_no_trade_ppm) / 1e6 * no_trade + f64::from(s.p_revert_ppm) / 1e6 * revert;
        won - chance(point.step) * costs
    }

    fn worst(&self, point: &SizedArmPoint, base_fee_wei: u128, l1_upper_wei: u128) -> u128 {
        let fee = base_fee_wei.saturating_mul(2).saturating_add(point.bid.sent.per_gas.min(self.settings.max_priority_wei_per_gas));
        u128::from(SIZED.limit).saturating_mul(fee).saturating_add(self.settings.l1_fee_bound_wei.max(l1_upper_wei.saturating_mul(2)))
    }

    /// What `cand` adds once `chosen` (its group's arms, in the order chosen)
    /// are armed. Its own tier 1 chosen: a tier 2 adds only the chance its
    /// higher bid wins and tier 1's would not. Another route chosen: replay
    /// that route's trade at its reference move on the book, and scale the
    /// candidate by what its net keeps there.
    #[allow(clippy::too_many_arguments)]
    fn marginal<'a>(
        &self,
        cand: &Candidate,
        chosen: impl Iterator<Item = &'a Candidate>,
        priced: &BTreeMap<(RouteId, u8), Priced>,
        moves: &MoveStats,
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        px: &Pricing,
        base_fee_wei: u128,
    ) -> f64 {
        let (id, leg, tier) = cand.key;
        let Some(p) = priced.get(&(id, leg)) else { return 0.0 };
        let Some(point) = p.points.iter().find(|x| x.tier == tier) else { return 0.0 };
        let c = &self.cycles[&id];
        let l = usize::from(leg);
        let (pool, other) = (c.legs[l].pool, c.legs[1 - l].pool);
        let up = trigger_up(c, l);
        let chance = |step: u32| moves.p_at_least(spread_key(pool, other), up, step).max(P_MOVE_FLOOR);
        let mut ev = self.value(point, p, base_fee_wei, &chance);
        // The trigger, moved as far as the reference move takes it.
        let reference = point.step + TIER1_BID_STEPS;
        let Some(trigger) = snapshot.get(&pool).and_then(|t| t.moved(super::threshold::move_of(t, up, reference))) else { return 0.0 };
        let mut after = trigger.clone();
        for prior in chosen {
            let (pid, pleg, ptier) = prior.key;
            if (pid, pleg) == (id, leg) {
                // Its own tier 1: only what the higher bid wins over it.
                if ptier < tier {
                    let one = p.points.iter().find(|x| x.tier == ptier).map_or(0.0, |x| x.bid.sent.p_win.max(P_WIN_FLOOR));
                    let two = point.bid.sent.p_win.max(P_WIN_FLOOR);
                    ev *= ((two - one) / two).max(0.0);
                }
                continue;
            }
            let pc = &self.cycles[&pid];
            let Some(pp) = priced.get(&(pid, pleg)) else { continue };
            let pl = usize::from(pleg);
            let (Some(o), Some(pxc)) = (snapshot.get(&pc.legs[1 - pl].pool), px.for_cycle(pc)) else { continue };
            let mut book = BTreeMap::new();
            book.insert(pc.legs[pl].pool, Arc::new(after.clone()));
            book.insert(o.spec.pool, Arc::clone(o));
            let price = U256::from(pxc.costs.wei_per_gas);
            if let SizedOutcome::Trade { size, mid, .. } = sized_trade(pc, &book, &pp.terms, price, &pxc.gas) {
                // The trigger is the prior route's hop `pl`: its input there is
                // the size on hop 0, and what hop 0 returned on hop 1.
                let input = if pl == 0 { size } else { mid };
                if let Some(next) = after.swapped(input, pc.legs[pl].zero_for_one) {
                    after = next;
                }
            }
        }
        if ev <= 0.0 {
            return ev;
        }
        let alone = self.reference_net(c, l, &trigger, snapshot, px);
        let left = self.reference_net(c, l, &after, snapshot, px);
        if alone <= 0.0 {
            return 0.0;
        }
        ev * (left / alone).clamp(0.0, 1.0)
    }

    /// The gross `c` trades with its trigger at `trigger`, in wei.
    fn reference_net(&self, c: &Cycle, leg: usize, trigger: &PoolSnapshot, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>, px: &Pricing) -> f64 {
        let Some(pxc) = px.for_cycle(c) else { return 0.0 };
        let Some(other) = snapshot.get(&c.legs[1 - leg].pool) else { return 0.0 };
        let mut book = BTreeMap::new();
        book.insert(c.legs[leg].pool, Arc::new(trigger.clone()));
        book.insert(other.spec.pool, Arc::clone(other));
        let (terms, _) = self.terms_for(c, &pxc, 0);
        match sized_trade(c, &book, &terms, U256::from(pxc.costs.wei_per_gas), &pxc.gas) {
            SizedOutcome::Trade { gross, .. } => pxc.in_wei(gross).as_u128() as f64,
            SizedOutcome::NoTrade(_) => 0.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn arm(
        &self,
        c: &Cycle,
        leg: u8,
        point: &SizedArmPoint,
        p: &Priced,
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
        let mut predicates = predicate::trigger(trigger_pool, &point.trigger, trigger_up(c, t))?;
        predicates.extend(predicate::band(other_pool, &point.edges, s.liquidity_floor_bps)?);
        predicates.push(predicate::expiry(to_block));
        let deadline = now_ns / 1_000_000_000 + s.window_s + 1;
        let pxc = px.for_cycle(c)?;
        let call = sized_call(c, snapshot, &p.terms, s.executor3, s.chain_id, deadline, p.lender, &px.gas).ok()?;
        let lender = match call.plan() {
            apex_exec::call::CallPlan::Lean3(plan) if plan.lender == Lean2Lender::PoolManager => "pool_manager",
            _ if p.lender == Lean2Lender::Morpho => "morpho",
            _ => "balancer",
        };
        // The engine's prediction at the threshold: logged, never enforced.
        let mut at = snapshot.clone();
        at.insert(c.legs[t].pool, Arc::new(point.trigger.clone()));
        let price = U256::from(base_fee_wei.saturating_add(point.bid.sent.per_gas));
        let (size, gross) = match sized_trade(c, &at, &p.terms, price, &pxc.gas) {
            SizedOutcome::Trade { size, gross, .. } => (size, gross),
            SizedOutcome::NoTrade(_) => (U256::zero(), U256::zero()),
        };
        let start_ticks = [trigger_pool, other_pool].iter().filter(|q| q.reserves.is_none()).map(|q| (format!("{:#x}", q.spec.pool), q.state.tick)).collect();
        Some(Arm {
            arm_id,
            at_ns: now_ns,
            armed_block: head,
            to_block,
            route_hash: format!("{:#x}", c.commitment.route_hash),
            trigger_leg: leg,
            rung: point.tier,
            step: point.step,
            pools: [format!("{:#x}", c.legs[0].pool), format!("{:#x}", c.legs[1].pool)],
            venues: [format!("{:?}", c.legs[0].venue), format!("{:?}", c.legs[1].venue)],
            predicates,
            start_words: [words_of(trigger_pool), words_of(other_pool)].concat(),
            start_ticks,
            size_wei: size.to_string(),
            mid: "0".into(),
            output_wei: size.saturating_add(gross).to_string(),
            surplus_wei: point.grid[0].1.max(0.0).to_string(),
            cost_wei: "0".into(),
            gas_expected: point.gas,
            gas_limit: SIZED.limit,
            base_fee_wei: base_fee_wei.to_string(),
            other_wei: pxc.costs.other_wei.to_string(),
            deadline,
            calldata: format!("0x{}", alloy_primitives::hex::encode(call.data())),
            calldata_hash: format!("{:#x}", keccak256(call.data())),
            sender_slot: u16::try_from(sender).unwrap_or(u16::MAX),
            lender: lender.into(),
            bid_wei_per_gas: point.bid.sent.per_gas.to_string(),
            p_win: point.bid.sent.p_win,
            renewals: Vec::new(),
            start: (c.start != crate::live::frontier::WETH).then_some(c.start),
            executor: Some(s.executor3),
            start_per_eth: pxc.rate.map(|(num, den)| [num.to_string(), den.to_string()]),
            bid_model: Some(point.bid.choice.label()),
            alt_bid_wei_per_gas: point.bid.alt.map(|b| b.per_gas.to_string()),
            alt_p_win: point.bid.alt.map(|b| b.p_win),
            model_version: self.contest.as_ref().map(|x| x.model_version.clone()),
            start_at: if t == 0 { [trigger_pool, other_pool] } else { [other_pool, trigger_pool] }
                .iter()
                .map(|q| StartAt { pool: format!("{:#x}", q.spec.pool), block: q.through.map_or(q.block, |x| x.0), log_index: q.through.map(|x| x.1) })
                .collect(),
            sized: Some(SizedLine {
                tier: point.tier,
                cap: p.terms.cap.to_string(),
                rate: p.terms.plan_rate().unwrap_or_default().to_string(),
                gas_overhead: p.terms.overhead,
                swap_gas: p.terms.swap_gas,
                l1_bound: p.terms.l1_bound,
                min_net: p.terms.min_net,
                max_steps: p.terms.max_steps,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::econ::RouteCosts;
    use crate::live::inventory::{PoolSpec, Venue};
    use alloy_primitives::address;
    use apex_math::cl_math::get_sqrt_ratio_at_tick;
    use apex_math::cl_state::ClPoolState;
    use apex_math::cl_swap::TickLadder;

    const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
    const CAKE: Address = address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38");
    const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
    const L: u128 = 1_400_000_000_000_000_000_000;

    fn pool(addr: Address, venue: Venue, liquidity: u128) -> PoolSnapshot {
        let tick = -197_350;
        PoolSnapshot {
            spec: PoolSpec { pool: addr, venue, token0: crate::live::frontier::WETH, token1: USDC, fee_ppm: 100, depth_usd: 5e6, v4: None },
            state: ClPoolState {
                sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap_or_default(),
                liquidity,
                tick,
                tick_spacing: 10,
                fee_ppm: 100,
                balance0: Some(U256::from(10u128.pow(24))),
                balance1: Some(U256::from(10u128.pow(16))),
            },
            ladder: TickLadder::new(vec![], tick - 30_000, tick + 30_000),
            decimals: (18, 6),
            factory: venue.factory(),
            code_hash: Default::default(),
            block: 100,
            last_log: None,
            through: None,
            dynamic_fee: None,
            seq: 1,
            reserves: None,
        }
    }

    /// Two routes share the trigger, Uniswap's pool, in one direction: the
    /// first's trade, replayed on the book, moves it back, so the second is
    /// worth less with the first armed than alone, and more than nothing when
    /// its own pool is deeper.
    #[test]
    fn a_second_route_counts_only_what_the_first_leaves() {
        let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
            (UNI, pool(UNI, Venue::UniswapV3, L)),
            (CAKE, pool(CAKE, Venue::PancakeV3, L)),
            (SLIP, pool(SLIP, Venue::PancakeV3, L * 4)),
        ]
        .into_iter()
        .map(|(a, p)| (a, Arc::new(p)))
        .collect();
        let (_, cycles) = crate::live::frontier::build(apex_types::ids::ChainId(8453), crate::live::frontier::WETH, &snap);
        let settings = SizedSettings {
            max_senders: 64,
            window_s: 10,
            block_time_ms: 200,
            band_cap_bps: 5,
            liquidity_floor_bps: 9_800,
            tier2_multiple: 10,
            executor3: Address::repeat_byte(0xe3),
            chain_id: 8453,
            max_size_wei: 10u128.pow(24),
            max_size_usdc: 12_000_000_000,
            max_priority_wei_per_gas: 20_000_000_000,
            min_net_wei: 1,
            max_steps: 8,
            p_no_trade_ppm: 0,
            p_revert_ppm: 0,
            reprice_for: Duration::MAX,
            workers: 1,
            l1_fee_bound_wei: 1_000_000_000_000,
        };
        let px = Pricing {
            costs: RouteCosts { other_wei: 0, wei_per_gas: 1 },
            gas: LEAN,
            lenders: None,
            margin_wei: U256::zero(),
            start_per_eth: None,
            rate: None,
            usdc_lenders: None,
            pool_manager: None,
        };
        let e = SizedEngine::new(cycles, settings, BTreeSet::new());
        e.tick(&snap, 1_000, 1, &px, 1, u128::MAX / 4, 0);
        let st = e.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // The slots whose trigger is Uniswap's pool, in one direction, one per other pool.
        let mut by_other: BTreeMap<Address, Candidate> = BTreeMap::new();
        for ((id, leg), p) in &st.priced {
            let c = &e.cycles[id];
            let l = usize::from(*leg);
            if c.legs[l].pool != UNI || !trigger_up(c, l) {
                continue;
            }
            if let Some(point) = p.points.first() {
                let chance = |s: u32| st.moves.p_at_least(spread_key(UNI, c.legs[1 - l].pool), true, s).max(P_MOVE_FLOOR);
                let ev = e.value(point, p, 1, &chance);
                by_other.insert(c.legs[1 - l].pool, Candidate { key: (*id, *leg, 1), group: (UNI, true), ev, worst_wei: 1 });
            }
        }
        let (Some(first), Some(second)) = (by_other.get(&CAKE).cloned(), by_other.get(&SLIP).cloned()) else {
            panic!("both routes priced: {:?}", by_other.keys().collect::<Vec<_>>())
        };
        let alone = e.marginal(&second, std::iter::empty(), &st.priced, &st.moves, &snap, &px, 1);
        let after = e.marginal(&second, std::iter::once(&first), &st.priced, &st.moves, &snap, &px, 1);
        assert!(alone > 0.0);
        assert!(after < alone, "{after} after the first against {alone} alone");
        assert!(after > 0.0, "a deeper second pool keeps some of the move");
    }
}
