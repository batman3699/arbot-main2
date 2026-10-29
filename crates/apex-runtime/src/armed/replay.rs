//! `apex armed-replay` (R36, lever 1): each recorded fire's route quoted on the
//! fire's own book at other sizes. The book is the evaluator's: both pools at
//! the fire block's parent, that block's writes through the trigger. Whether a
//! smaller size, one crossing no tick, would have paid more per gas than the
//! size armed, and so bid past the rival who took the fire.
//!
//! Each size is the cycle's chained quote: a plan built for that size, with
//! minimums from its own threshold. An Aerodrome v2 hop pays exactly its
//! minimum, so for it the chained output is the most such a plan could take.

use super::eval::merge_renewals;
use super::lines::{Arm, ArmedLine};
use super::run::{book_after, cycle_through, get_logs, Verdict};
use crate::live::frontier::{self, WETH};
use crate::live::gas::{self, SettlementGas};
use crate::live::inventory;
use crate::live::pricing::LiveCycle;
use crate::live::reads::ChainReads;
use crate::shadow::config::ShadowConfig;
use alloy_primitives::Address;
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One size on the fire's book: what the cycle returns, the ticks each hop
/// crosses, and the gas the lean model expects for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizePoint {
    pub size: U256,
    pub output: U256,
    pub crossed: [u32; 2],
    pub gas: u64,
}

/// `None` when either hop cannot fill `size`.
pub fn size_point(live: &LiveCycle, gas: &SettlementGas, size: U256) -> Option<SizePoint> {
    let q = live.quote(size)?;
    Some(SizePoint { size, output: q.outputs[1], crossed: q.hops.map(|h| h.crossed), gas: gas.estimate(&q.hops).expected.0 })
}

/// The largest size up to `cap` at which neither hop crosses an initialized
/// tick; zero when every size does. Crossings only grow with size.
pub fn single_tick_size(live: &LiveCycle, cap: U256) -> U256 {
    let clear = |x: U256| live.quote(x).is_some_and(|q| q.hops.iter().all(|h| h.crossed == 0));
    if clear(cap) {
        return cap;
    }
    let (mut lo, mut hi) = (U256::zero(), cap);
    while hi - lo > U256::one() {
        let mid = lo + (hi - lo) / 2;
        if clear(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The sizes replayed: the armed size times 2^(k/2) for k from -12 to 4, the
/// armed size itself, and the single-tick size; ascending, each once.
pub fn sizes(armed: U256, single_tick: U256) -> Vec<U256> {
    // √2 to sixteen places.
    let (root2, one) = (U256::from(14_142_135_623_730_951u64), U256::from(10_000_000_000_000_000u64));
    let mut out: Vec<U256> = (-12i32..=4)
        .map(|k| {
            let half = k.div_euclid(2);
            let whole = if half >= 0 { armed << half.unsigned_abs() } else { armed >> half.unsigned_abs() };
            if k.rem_euclid(2) == 1 {
                whole * root2 / one
            } else {
                whole
            }
        })
        .chain([single_tick])
        .filter(|s| !s.is_zero())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every fire in `dirs`, one JSON object each. A directory holds one fired
/// arm's lines (with its renewals) and its verdict, as `armed-eval` writes
/// them; a fire that cannot be replayed says why.
pub async fn replay(config: &ShadowConfig, dirs: &[PathBuf]) -> Result<Vec<Value>, String> {
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let rpc: Arc<dyn RpcTransport> =
        Arc::new(FailoverTransport::connect(&urls, config.chain_id, FailoverSettings::default()).await.map_err(|e| e.to_string())?);
    drop(urls);
    let reads = ChainReads::new(Arc::clone(&rpc));
    let mut out = Vec::new();
    for dir in dirs {
        out.push(match replay_one(&reads, &*rpc, config, dir).await {
            Ok(v) => v,
            Err(e) => json!({ "dir": dir, "error": e }),
        });
    }
    Ok(out)
}

async fn replay_one(reads: &ChainReads, rpc: &dyn RpcTransport, config: &ShadowConfig, dir: &Path) -> Result<Value, String> {
    let (arm, verdict) = read_fire(dir)?;
    let fire = verdict.fire.as_ref().ok_or("the verdict has no fire")?;
    let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
    let set: BTreeSet<Address> = pools.iter().copied().collect();
    let hooks = config.universe.hooks()?;
    let specs = inventory::load(&config.inventory, config.universe.filter(), &set, &hooks).map_err(|e| e.to_string())?;
    let v4_ids: Vec<alloy_primitives::B256> = specs.iter().filter_map(|s| s.v4.map(|k| k.pool_id)).collect();
    let logs = get_logs(rpc, &set, &v4_ids, fire.block, fire.block).await?;
    let snap = book_after(reads, &specs, &hooks, &pools, fire.block, fire.tx_index, &logs).await?;
    let (_, cycles) = frontier::build_starts(ChainId::BASE, &[WETH, apex_exec::lean2::USDC], &snap);
    let cycle = cycle_through(&cycles, &arm.pools).ok_or("the route is not in the reloaded pools")?;
    let live = LiveCycle::new(cycle, &snap).ok_or("a pool left its ladder")?;
    let armed = U256::from_dec_str(&arm.size_wei).map_err(|e| e.to_string())?;
    let single = single_tick_size(&live, armed * 4);
    let points: Vec<Value> = sizes(armed, single)
        .into_iter()
        .filter_map(|s| size_point(&live, &gas::LEAN, s))
        .map(|p| json!({ "size_wei": p.size.to_string(), "output_wei": p.output.to_string(), "crossed": p.crossed, "gas": p.gas }))
        .collect();
    Ok(json!({
        "arm_id": arm.arm_id,
        "rung": arm.rung,
        "step": arm.step,
        "venues": arm.venues,
        "fire": fire,
        "armed_size_wei": arm.size_wei,
        "armed_gas_expected": arm.gas_expected,
        "other_wei": arm.other_wei,
        "single_tick_size_wei": single.to_string(),
        "gross_wei": verdict.gross_wei,
        "sim": verdict.sim,
        "rival_priority_wei": verdict.rival_priority_wei,
        "base_fee_wei": verdict.base_fee_wei,
        "weth_usd": verdict.weth_usd,
        "points": points,
    }))
}

/// The one arm in `dir` (its renewals merged) and its fired verdict.
fn read_fire(dir: &Path) -> Result<(Arm, Verdict), String> {
    let (mut lines, mut verdict) = (Vec::new(), None);
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir).map_err(|e| e.to_string())?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    names.sort();
    for path in names {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        if !name.starts_with("armed-") || !name.ends_with(".jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        for line in text.lines() {
            if name.starts_with("armed-verdicts") {
                if let Ok(v) = serde_json::from_str::<Verdict>(line) {
                    if v.fire.is_some() && verdict.is_none() {
                        verdict = Some(v);
                    }
                }
            } else if let Ok(l) = serde_json::from_str::<ArmedLine>(line) {
                lines.push(l);
            }
        }
    }
    let mut arms = merge_renewals(lines);
    let arm = match (arms.pop(), arms.is_empty()) {
        (Some(a), true) => a,
        _ => return Err(format!("{} does not hold exactly one arm", dir.display())),
    };
    Ok((arm, verdict.ok_or("no fired verdict")?))
}
