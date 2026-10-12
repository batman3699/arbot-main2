//! The arming engine (R26): for every cycle and trigger pool, the rungs and
//! their predicates and calldata; the best `max_arms` armed, one virtual
//! sender each; a line per arm and per renewal. Pure: the shadow worker calls
//! [`Engine::tick`] and writes what it returns.

use super::lines::{Arm, ArmedLine, Word};
use super::moves::MoveStats;
use super::plan::armed_call;
use super::predicate::{self, layout};
use super::threshold::{band, rungs, trigger_up, Band, Pricing, Rung};
use crate::live::book::PoolSnapshot;
use crate::live::frontier::Cycle;
use crate::live::reads::ChainReads;
use alloy_primitives::{keccak256, Address};
use apex_search::frontier::RouteId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Slots repriced per tick at most, in the shadow: the rest wait for the next
/// 200 ms. About 16 ms a slot in a release build.
pub const REPRICE_BUDGET: usize = 24;

#[derive(Clone, Copy, Debug)]
pub struct EngineSettings {
    pub max_arms: usize,
    pub rungs: u8,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
    pub executor: Address,
    pub chain_id: u64,
    /// Slots repriced per tick at most ([`REPRICE_BUDGET`] in the shadow).
    pub reprice_budget: usize,
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
    rungs: Vec<(Rung, Band)>,
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
}

pub struct Engine {
    cycles: BTreeMap<RouteId, Cycle>,
    settings: EngineSettings,
    unarmable: BTreeSet<Address>,
    state: Mutex<State>,
    arms: AtomicU64,
    renewals: AtomicU64,
    priced: AtomicU64,
    armable: AtomicU64,
    last_tick_us: AtomicU64,
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
    let l = layout(p.spec.venue);
    let a = format!("{:#x}", p.spec.pool);
    let w = |slot: u64, v: U256| Word { address: a.clone(), slot: format!("{slot:#x}"), value: format!("{v:#x}") };
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
        Self {
            cycles,
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

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            arms: self.arms.load(Relaxed),
            renewals: self.renewals.load(Relaxed),
            slots_priced: self.priced.load(Relaxed),
            slots_armable: self.armable.load(Relaxed),
            unarmable_pools: self.unarmable.len() as u64,
            last_tick_us: self.last_tick_us.load(Relaxed),
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

        // Reprice stale slots, stalest first, within the budget. A slot is
        // stale when either pool changed or its window has passed.
        let mut stale: Vec<((RouteId, u8), u64)> = Vec::new();
        for (id, c) in &self.cycles {
            if c.legs.iter().any(|l| self.unarmable.contains(&l.pool)) {
                continue;
            }
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            for leg in 0..2u8 {
                let k = st.computed.get(&(*id, leg));
                let fresh = k.is_some_and(|k| k.seqs == [a.seq, b.seq] && now_ns < k.at_ns.saturating_add(window_ns));
                if !fresh {
                    stale.push(((*id, leg), k.map_or(0, |k| k.at_ns)));
                }
            }
        }
        stale.sort_by_key(|(_, age)| *age);
        for ((id, leg), _) in stale.into_iter().take(s.reprice_budget) {
            let c = &self.cycles[&id];
            let pair = [Arc::clone(&snapshot[&c.legs[0].pool]), Arc::clone(&snapshot[&c.legs[1].pool])];
            let found: Vec<(Rung, Band)> = rungs(c, &pair, usize::from(leg), px, s.rungs)
                .into_iter()
                .filter_map(|r| band(c, &pair, usize::from(leg), &r, px, s.band_cap_bps).map(|b| (r, b)))
                .collect();
            self.priced.fetch_add(1, Relaxed);
            st.computed.insert((id, leg), Computed { seqs: [pair[0].seq, pair[1].seq], at_ns: now_ns, rungs: found });
        }
        self.armable.store(st.computed.values().filter(|k| !k.rungs.is_empty()).count() as u64, Relaxed);

        // Rank every rung priced at this book: P(move ≥ step within the
        // window) × the surplus its plan expects. A rung priced before either
        // pool changed is left out until it is repriced: its trigger and band
        // are prices the pools may have left, so it is neither armed nor renewed.
        let moves = &st.moves;
        let mut ranked: Vec<(f64, Key)> = Vec::new();
        for ((id, leg), k) in &st.computed {
            let c = &self.cycles[id];
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            if k.seqs != [a.seq, b.seq] {
                continue;
            }
            let pool = c.legs[usize::from(*leg)].pool;
            for (r, b) in &k.rungs {
                let p = moves.p_at_least(pool, trigger_up(c, usize::from(*leg)), r.step);
                ranked.push((p * b.point.surplus.as_u128() as f64, (*id, *leg, r.index)));
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
            let Some((rung, band)) = st.computed[&(id, leg)].rungs.iter().find(|(r, _)| r.index == index) else { continue };
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
                        live.arm.to_block = to_block;
                        st.senders[usize::from(live.arm.sender_slot)] = to_block;
                        self.renewals.fetch_add(1, Relaxed);
                        out.push(ArmedLine::Renew { arm_id: live.arm.arm_id, to_block, at_ns: now_ns });
                    }
                    continue;
                }
                if live.arm.to_block > head {
                    continue; // changed, but its sender is busy until its window ends
                }
            }
            let Some(sender) = st.senders.iter().position(|b| *b == 0) else { break };
            let Some(arm) = self.arm(c, leg, rung, band, snapshot, (head, to_block), base_fee_wei, px, now_ns, sender, st.next_arm_id) else {
                continue;
            };
            st.next_arm_id += 1;
            st.senders[sender] = to_block;
            self.arms.fetch_add(1, Relaxed);
            out.push(ArmedLine::Arm(Box::new(arm.clone())));
            st.live.insert(key, Live { arm, fingerprint });
        }
        self.last_tick_us.store(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX), Relaxed);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn arm(
        &self,
        c: &Cycle,
        leg: u8,
        rung: &Rung,
        band: &Band,
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
        let deadline = now_ns / 1_000_000_000 + s.window_s + 1;
        let pools = [&**snapshot.get(&c.legs[0].pool)?, &**snapshot.get(&c.legs[1].pool)?];
        let point = &band.point;
        let call = armed_call(c, pools, point.size, point.mid, s.executor, s.chain_id, deadline).ok()?;
        let data = call.data();
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
            gas_limit: point.gas.ceiling.0.saturating_mul(5) / 4,
            base_fee_wei: base_fee_wei.to_string(),
            other_wei: px.costs.other_wei.to_string(),
            deadline,
            calldata: format!("0x{}", alloy_primitives::hex::encode(data)),
            calldata_hash: format!("{:#x}", keccak256(data)),
            sender_slot: u16::try_from(sender).unwrap_or(u16::MAX),
        })
    }
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
        let l = layout(p.spec.venue);
        let a = p.spec.pool;
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
            bad.insert(a);
        }
    }
    bad
}
