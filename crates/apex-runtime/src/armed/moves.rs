//! How far each pool's price moves within an arming window (R26), learned
//! from the book as it runs: the ranking's `P(move ≥ m within W)`.
//!
//! A pool's position is its tick (1 tick ≈ 1 bp) or, for a constant-product
//! pool, its log-price in bps. Statistics only; nothing priced reads them.
//!
//! R36: the closed windows are saved and reloaded across restarts
//! ([`MovesSnapshot`]), and a day of the pools' own logs can seed them
//! ([`seed`]). Learned in memory alone, they began again at every restart, and
//! while young the floor of 1/(windows + 2) gave the far rungs every sender.

use crate::live::feed::{decode_swap, decode_sync, PANCAKE_SWAP, SWAP, SYNC};
use alloy_primitives::{keccak256, Address};
use apex_chain::rpc::ws::{parse_log, RawLog};
use apex_chain::rpc::RpcTransport;
use serde_json::{json, Value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::Path;

/// A day of 10-second windows.
const KEEP: usize = 8_640;

/// How fast large moves thin out: P(move >= m) falls as m^-2.87, the log-log
/// slope for m from 10 to 100 ticks over 33,917 five-block windows on 38 pools
/// (2026-10-12/13). Used only past the largest move a pool has been seen to make.
pub const TAIL_EXPONENT: f64 = 2.87;

#[derive(Debug, Default)]
struct PoolMoves {
    window_start_ns: u64,
    start: f64,
    max_up: f64,
    max_down: f64,
    windows: VecDeque<(f64, f64)>,
    /// Restored with no open window: the next observation opens one.
    fresh: bool,
}

#[derive(Debug)]
pub struct MoveStats {
    window_ns: u64,
    pools: BTreeMap<Address, PoolMoves>,
}

impl MoveStats {
    pub fn new(window_ns: u64) -> Self {
        Self { window_ns: window_ns.max(1), pools: BTreeMap::new() }
    }

    pub fn observe(&mut self, pool: Address, position: f64, now_ns: u64) {
        let w = self.window_ns;
        let p = self
            .pools
            .entry(pool)
            .or_insert_with(|| PoolMoves { window_start_ns: now_ns, start: position, ..Default::default() });
        if p.fresh {
            *p = PoolMoves { window_start_ns: now_ns, start: position, windows: std::mem::take(&mut p.windows), ..Default::default() };
        }
        if now_ns >= p.window_start_ns.saturating_add(w) {
            p.windows.push_back((p.max_up, p.max_down));
            if p.windows.len() > KEEP {
                p.windows.pop_front();
            }
            let windows = std::mem::take(&mut p.windows);
            *p = PoolMoves { window_start_ns: now_ns, start: position, windows, ..Default::default() };
        }
        p.max_up = p.max_up.max(position - p.start);
        p.max_down = p.max_down.max(p.start - position);
    }

    /// The share of closed windows in which `pool` moved at least `step`
    /// that way: (hits + 1) / (windows + 2), for a move no larger than the
    /// pool has been seen to make. Past that, the share at the largest seen,
    /// thinned by the measured tail ([`TAIL_EXPONENT`]): a uniform prior gave
    /// rungs for 2% moves in ten seconds every sender (the smoke run,
    /// 2026-10-12 UTC), and none at all left spike rungs without one (R26.3).
    /// A pool seen not to move that way gets nothing.
    pub fn p_at_least(&self, pool: Address, up: bool, step: u32) -> f64 {
        let Some(p) = self.pools.get(&pool) else { return 0.5 };
        let way = |(u, d): &(f64, f64)| if up { *u } else { *d };
        let n = p.windows.len() as f64 + 2.0;
        let share = |m: f64| (p.windows.iter().filter(|w| way(w) >= m).count() as f64 + 1.0) / n;
        let step = f64::from(step);
        let seen = p.windows.iter().map(way).fold(way(&(p.max_up, p.max_down)), f64::max);
        // The two agree at `seen`, and a pool seen not to move gets 0^k = 0.
        if step <= seen {
            share(step)
        } else {
            share(seen) * (seen / step).powf(TAIL_EXPONENT)
        }
    }
}

/// The closed windows, as saved (R36). The open window is not kept: a boot
/// opens a new one at its first observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MovesSnapshot {
    pub window_ns: u64,
    pub saved_at_ns: u64,
    /// Each pool's closed windows, oldest first: its largest move up and down.
    pub pools: BTreeMap<Address, Vec<(f64, f64)>>,
}

impl MovesSnapshot {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Staged beside `path`, synced, then renamed over it: a crash leaves the
    /// old snapshot or the new one, never half of one.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let staged = path.with_extension("json.saving");
        let text = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let mut f = std::fs::File::create(&staged)?;
        f.write_all(&text)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&staged, path)
    }
}

impl MoveStats {
    /// The closed windows of every pool, as of `now_ns`. A window with a
    /// move that is not a number (a reserve of zero) is left out.
    pub fn snapshot(&self, now_ns: u64) -> MovesSnapshot {
        let pools = self
            .pools
            .iter()
            .map(|(a, p)| (*a, p.windows.iter().copied().filter(|(u, d)| u.is_finite() && d.is_finite()).collect()))
            .collect();
        MovesSnapshot { window_ns: self.window_ns, saved_at_ns: now_ns, pools }
    }

    /// Take a snapshot's windows, the latest day of each pool's. Refused for
    /// windows of another length: their shares would not be this engine's.
    pub fn restore(&mut self, snap: &MovesSnapshot) -> Result<usize, String> {
        if snap.window_ns != self.window_ns {
            return Err(format!("the snapshot's window is {} ns and the engine's {} ns", snap.window_ns, self.window_ns));
        }
        for (pool, windows) in &snap.pools {
            let keep = windows.len().saturating_sub(KEEP);
            self.pools.insert(*pool, PoolMoves { windows: windows[keep..].iter().copied().collect(), fresh: true, ..Default::default() });
        }
        Ok(snap.pools.len())
    }
}

/// A pool's position at a time, read from one of its logs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observation {
    pub pool: Address,
    pub at_ns: u64,
    pub position: f64,
}

/// The position a pool's state log carries, as the engine reads that pool: a
/// `Swap`'s tick, or a `Sync`'s log-price in bps. `None` for any other log,
/// and for a reserve of zero.
pub fn observation_of(log: &RawLog, at_ns: u64) -> Option<Observation> {
    if let Some(s) = decode_swap(log) {
        return Some(Observation { pool: s.pool, at_ns, position: f64::from(s.tick) });
    }
    let s = decode_sync(log)?;
    if s.reserve0.is_zero() || s.reserve1.is_zero() {
        return None;
    }
    let (r0, r1) = (s.reserve0.saturating_to::<u128>() as f64, s.reserve1.saturating_to::<u128>() as f64);
    Some(Observation { pool: s.pool, at_ns, position: (r1 / r0).ln() * 10_000.0 })
}

/// The series of a route's spread with `trigger` as its trigger pool: the
/// trigger's position less the other's (R36). A fire is this gap opening;
/// the market moving both pools moves neither it nor a fire.
pub fn spread_key(trigger: Address, other: Address) -> Address {
    let mut bytes = b"apex-spread".to_vec();
    bytes.extend_from_slice(trigger.as_slice());
    bytes.extend_from_slice(other.as_slice());
    Address::from_slice(&keccak256(bytes)[12..])
}

/// Statistics replayed from logs, as the engine would have learned them by
/// watching: each pool observed at every `step_ns` (a block) from its first
/// log to `until_ns`, holding its last position between logs, so a quiet
/// window counts as one with no move. Each of `pairs` (trigger, other) is
/// observed as its spread, after all the logs of a moment: one transaction
/// moving both pools leaves no gap to fire on.
pub fn seed(window_ns: u64, observations: &[Observation], pairs: &[(Address, Address)], step_ns: u64, until_ns: u64) -> MoveStats {
    let mut obs = observations.to_vec();
    obs.sort_by_key(|o| o.at_ns);
    let mut m = MoveStats::new(window_ns);
    let Some(first) = obs.first() else { return m };
    let mut last: BTreeMap<Address, f64> = BTreeMap::new();
    let spreads = |m: &mut MoveStats, last: &BTreeMap<Address, f64>, at: u64| {
        for (a, b) in pairs {
            if let (Some(x), Some(y)) = (last.get(a), last.get(b)) {
                m.observe(spread_key(*a, *b), x - y, at);
            }
        }
    };
    let (mut i, mut t) = (0, first.at_ns);
    while t <= until_ns {
        while i < obs.len() && obs[i].at_ns <= t {
            let at = obs[i].at_ns;
            while i < obs.len() && obs[i].at_ns == at {
                m.observe(obs[i].pool, obs[i].position, at);
                last.insert(obs[i].pool, obs[i].position);
                i += 1;
            }
            spreads(&mut m, &last, at);
        }
        for (pool, position) in &last {
            m.observe(*pool, *position, t);
        }
        spreads(&mut m, &last, t);
        t = t.saturating_add(step_ns.max(1));
    }
    m
}

/// The narrowest range a seed asks for before it gives up on one.
pub const SMALLEST_RANGE: u64 = 10;

/// What a seed read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedRead {
    pub logs: usize,
    pub observations: usize,
    /// The last block's time: the statistics run to here.
    pub until_ns: u64,
}

/// Statistics seeded from blocks `from..=to` of `pools`' state logs (each
/// venue's `Swap`, or Aerodrome v2's `Sync`), read up to `chunk` blocks at a
/// time, each log timed between its chunk's first and last blocks'
/// timestamps, the pools named `addresses_per_call` at a time (PublicNode's
/// limit is 10). A range the endpoint refuses (free plans cap it: drpc's at
/// 100 blocks, Blast's at 10, PublicNode's at 20,000 results) is halved, down
/// to [`SMALLEST_RANGE`], and grown back after each answer.
#[allow(clippy::too_many_arguments)]
pub async fn seed_from_logs(
    rpc: &dyn RpcTransport,
    pools: &[Address],
    pairs: &[(Address, Address)],
    from: u64,
    to: u64,
    chunk: u64,
    addresses_per_call: usize,
    window_ns: u64,
    step_ns: u64,
) -> Result<(MoveStats, SeedRead), String> {
    let addresses: Vec<String> = pools.iter().map(|p| format!("{p:#x}")).collect();
    let groups: Vec<&[String]> = addresses.chunks(addresses_per_call.max(1)).collect();
    let topics: Vec<String> = [SWAP, PANCAKE_SWAP, SYNC].iter().map(|t| format!("{t:#x}")).collect();
    let chunk = chunk.max(1);
    let (mut obs, mut logs, mut until_ns, mut lo, mut size) = (Vec::new(), 0usize, 0u64, from, chunk);
    while lo <= to {
        let hi = lo.saturating_add(size - 1).min(to);
        let mut answers = Vec::with_capacity(groups.len());
        let mut refused = None;
        for group in &groups {
            let filter = json!([{ "fromBlock": format!("{lo:#x}"), "toBlock": format!("{hi:#x}"), "address": group, "topics": [topics] }]);
            match rpc.call("eth_getLogs", filter).await {
                Ok(a) => answers.push(a),
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        match refused {
            None => {}
            Some(_) if size > SMALLEST_RANGE => {
                size = (size / 2).max(SMALLEST_RANGE);
                continue;
            }
            Some(e) => return Err(format!("eth_getLogs {lo}..{hi}: {e}")),
        }
        let (t_lo, t_hi) = (block_time_s(rpc, lo).await?, block_time_s(rpc, hi).await?);
        let mut list = Vec::new();
        for answer in &answers {
            list.extend(answer.as_array().ok_or_else(|| format!("eth_getLogs {lo}..{hi} answered no list"))?.iter());
        }
        for v in list {
            let Some(log) = parse_log(v, false).filter(|l| !l.removed) else { continue };
            logs += 1;
            let b = log.block_number.unwrap_or(lo).clamp(lo, hi);
            let at_s = if hi > lo { t_lo as f64 + (b - lo) as f64 * (t_hi - t_lo) as f64 / (hi - lo) as f64 } else { t_lo as f64 };
            if let Some(o) = observation_of(&log, (at_s * 1e9) as u64) {
                obs.push(o);
            }
        }
        until_ns = t_hi.saturating_mul(1_000_000_000);
        lo = hi + 1;
        size = size.saturating_mul(2).min(chunk);
    }
    let m = seed(window_ns, &obs, pairs, step_ns, until_ns);
    Ok((m, SeedRead { logs, observations: obs.len(), until_ns }))
}

async fn block_time_s(rpc: &dyn RpcTransport, block: u64) -> Result<u64, String> {
    let b = rpc.call("eth_getBlockByNumber", json!([format!("{block:#x}"), false])).await.map_err(|e| format!("block {block}: {e}"))?;
    b.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("block {block} has no timestamp"))
}
