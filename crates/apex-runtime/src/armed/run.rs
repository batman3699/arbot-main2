//! `apex armed-eval` (R26): every closed arm judged against the chain, and a
//! verdict per arm, written to the verdict ledger.
//!
//! One run takes arms whose window closed at least [`SETTLE_BLOCKS`] ago and
//! after the cursor (`armed-eval.cursor` beside the verdicts). For each it:
//! 1. rebuilds the watched words from one `eth_getLogs` over the run's pools;
//! 2. finds the fire point and the rival (`eval::judge`);
//! 3. quotes the fire exactly: both pools loaded at the fire block's parent,
//!    that block's writes replayed up to the trigger, then the armed plan's
//!    hops as the lean executor runs them (`plan::armed_gross`);
//! 4. checks the quote with `eth_simulateV1`: the fire block's earlier
//!    transactions on those pools, then the armed call from the trader;
//! 5. places both in their flashblocks (the flashblock ledger) and judges the
//!    landing for every bid fraction and the arm's own bid: a fire that closed
//!    its flashblock is contested in the next one, by fee, and a rival that
//!    is the arm's own send is armed-live's landing (R37).
//!
//! The lean executor sells all hop 1 returns, so a fire leaves no intermediate
//! residue: `residue` is absent from verdicts since R36's lean quote.

use super::eval::{self, bid_wei_per_gas, judge, landing, merge_renewals, net_wei, own_landing, Landing, Point, TxEffect, Write, F_PERMILLE};
use super::lines::{Arm, ArmedLine};
use super::predicate::{layout, price_mask};
use crate::live::book::{LiquidityWrite, PoolBook, StateWrite, SwapApplied, SwapWrite, SyncWrite};
use crate::live::feed::{
    decode_liquidity, decode_modify_liquidity, decode_swap, decode_sync, decode_v4_swap, BURN, MINT, MODIFY_LIQUIDITY, PANCAKE_SWAP, SWAP,
    SYNC, V4_SWAP,
};
use crate::live::v4::{key_address, liquidity_slot, state_slot, HookAllowlist, POOL_MANAGER};
use crate::live::frontier::{self, Cycle, WETH};
use crate::live::inventory::{self, PoolSpec, Venue};
use crate::live::reads::ChainReads;
use crate::shadow::config::ShadowConfig;
use crate::shadow::ledger;
use alloy_primitives::{keccak256, Address, B256};
use apex_chain::base::flashblock_stream::FlashblockSeen;
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::ws::{parse_log, RawLog};
use apex_chain::rpc::RpcTransport;
use apex_search::frontier::RouteId;
use apex_types::compat::u256_to_ethers;
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Blocks a window must have closed for before its arm is judged: a minute.
pub const SETTLE_BLOCKS: u64 = 30;
/// Blocks per `eth_getLogs`.
const LOG_CHUNK: u64 = 500;

fn topic_i32(topic: &B256) -> i32 {
    i32::from_be_bytes(topic.0[28..32].try_into().unwrap_or([0; 4]))
}

fn data_u128(data: &[u8], i: usize) -> Option<u128> {
    let w = data.get(i * 32..i * 32 + 32)?;
    Some(u128::from_be_bytes(w[16..32].try_into().ok()?))
}

/// A transaction's hash, and its writes keyed by log index.
type TxWrites = (String, Vec<(u64, Write)>);

/// The watched pools' logs as per-transaction effects, in chain order. A
/// removed log, an unknown pool or an undecodable log adds nothing.
pub fn effects_of(logs: &[RawLog], venues: &BTreeMap<Address, Venue>) -> Vec<TxEffect> {
    let mut by_tx: BTreeMap<(u64, u64), TxWrites> = BTreeMap::new();
    for l in logs.iter().filter(|l| !l.removed) {
        let (Some(block), Some(txi), Some(li)) = (l.block_number, l.transaction_index, l.log_index) else { continue };
        let writes = if l.address == POOL_MANAGER {
            let Some(w) = v4_writes(l, venues) else { continue };
            w
        } else {
            let Some(venue) = venues.get(&l.address) else { continue };
            pool_writes(l, *venue)
        };
        let tx = l.transaction_hash.map_or_else(String::new, |h| format!("{h:#x}"));
        let entry = by_tx.entry((block, txi)).or_insert_with(|| (tx, Vec::new()));
        entry.1.extend(writes.into_iter().map(|w| (li, w)));
    }
    by_tx
        .into_iter()
        .map(|((block, tx_index), (tx, mut writes))| {
            writes.sort_by_key(|(li, _)| *li);
            TxEffect { block, tx_index, tx, writes: writes.into_iter().map(|(_, w)| w).collect() }
        })
        .collect()
}

/// A watched v4 pool's Swap or ModifyLiquidity, as writes on PoolManager's
/// slots (R37); its tick, under the pool's key address. None for a pool not
/// watched.
fn v4_writes(l: &RawLog, venues: &BTreeMap<Address, Venue>) -> Option<Vec<Write>> {
    let id = *l.topics.get(1)?;
    let key = key_address(id);
    if venues.get(&key) != Some(&Venue::UniswapV4) {
        return None;
    }
    let slot = |b: B256| U256::from_big_endian(b.as_slice());
    let (pm, k) = (format!("{POOL_MANAGER:#x}"), format!("{key:#x}"));
    let ls = slot(liquidity_slot(id));
    let mut writes = Vec::new();
    match l.topics.first() {
        Some(t) if *t == V4_SWAP => {
            if let Some(s) = decode_v4_swap(l) {
                writes.push(Write::Set { address: pm.clone(), slot: slot(state_slot(id)), value: u256_to_ethers(s.sqrt_price_x96) });
                writes.push(Write::Set { address: pm, slot: ls, value: U256::from(s.liquidity) });
                writes.push(Write::Tick { address: k, tick: s.tick });
            }
        }
        Some(t) if *t == MODIFY_LIQUIDITY => {
            if let Some(p) = decode_modify_liquidity(l) {
                writes.push(Write::Liquidity { address: pm, slot: ls, pool: k, lower: p.tick_lower, upper: p.tick_upper, delta: p.delta });
            }
        }
        _ => {}
    }
    Some(writes)
}

/// A pool's own log, as writes on its own slots.
fn pool_writes(l: &RawLog, venue: Venue) -> Vec<Write> {
    let lay = layout(venue, l.address);
    let a = format!("{:#x}", l.address);
    let mut writes = Vec::new();
    match l.topics.first() {
        Some(t) if *t == SWAP || *t == PANCAKE_SWAP => {
            if let (Some(s), Some(ps), Some(ls)) = (decode_swap(l), lay.price_slot, lay.liquidity_slot) {
                writes.push(Write::Set { address: a.clone(), slot: ps, value: u256_to_ethers(s.sqrt_price_x96) });
                writes.push(Write::Set { address: a.clone(), slot: ls, value: U256::from(s.liquidity) });
                writes.push(Write::Tick { address: a.clone(), tick: s.tick });
            }
        }
        Some(t) if *t == SYNC => {
            if let (Some(s), Some((s0, s1))) = (decode_sync(l), lay.reserve_slots) {
                writes.push(Write::Set { address: a.clone(), slot: s0, value: u256_to_ethers(s.reserve0) });
                writes.push(Write::Set { address: a.clone(), slot: s1, value: u256_to_ethers(s.reserve1) });
            }
        }
        // Mint(sender, amount, amount0, amount1) and Burn(amount, amount0,
        // amount1), with the tick range in topics 2 and 3.
        Some(t) if (*t == MINT || *t == BURN) && l.topics.len() == 4 => {
            let amount = if *t == MINT { data_u128(&l.data, 1) } else { data_u128(&l.data, 0) };
            if let (Some(amount), Some(ls)) = (amount, lay.liquidity_slot) {
                let delta = i128::try_from(amount).unwrap_or(i128::MAX);
                writes.push(Write::Liquidity {
                    address: a.clone(),
                    slot: ls,
                    pool: a.clone(),
                    lower: topic_i32(&l.topics[2]),
                    upper: topic_i32(&l.topics[3]),
                    delta: if *t == MINT { delta } else { -delta },
                });
            }
        }
        _ => {}
    }
    writes
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerF {
    pub f_permille: u32,
    pub bid_wei_per_gas: String,
    pub landing: Landing,
    /// Net in wei when landed, from the exact quote; `None` when it did not price.
    pub net_wei: Option<String>,
}

/// R37: the arm's own bid, the one armed-live sends, judged like a fraction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnBid {
    pub bid_wei_per_gas: String,
    pub landing: Landing,
    pub net_wei: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimCheck {
    pub ok: bool,
    pub gross_wei: Option<String>,
    pub gas_used: u64,
    pub agrees: bool,
    /// The replay wrote the arm's pools differently from the chain (R27). The
    /// simulation is then checked against a quote on the replay's own state.
    #[serde(default)]
    pub replay_diverged: bool,
}

/// How long the evaluator waits for one answer: a busy block replayed up to a
/// fire is a heavy simulation (block 52,526,060's 902 transactions outlasted
/// the default 3 s), and a timed-out one would leave its fire unchecked.
pub const EVAL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A fire or rival position: block, transaction index, hash, flashblock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub block: u64,
    pub tx_index: u64,
    pub tx: String,
    pub flashblock: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub at_ns: u64,
    pub arm_id: u64,
    pub route_hash: String,
    pub trigger_leg: u8,
    pub rung: u8,
    pub fired: bool,
    /// The trigger was met in the window (by itself): unfired, the band missed.
    /// Absent from verdicts written before it was recorded.
    #[serde(default)]
    pub trigger_met: Option<bool>,
    /// A higher rung of the same slot fired on the same transaction: it, not
    /// this one, would execute first.
    pub shadowed: bool,
    pub fire: Option<Position>,
    /// The fire was its flashblock's last transaction: we could land in the
    /// next flashblock at the earliest. Absent when the ledger did not place
    /// it, and from verdicts before it was judged (R37).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_closes_flashblock: Option<bool>,
    pub rival: Option<Position>,
    /// The rival's effective priority fee per gas.
    pub rival_priority_wei: Option<String>,
    /// The rival was this arm's own send: armed-live landed it (R37).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ours: bool,
    /// The arm's own bid, judged (R37). Absent for an arm without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<OwnBid>,
    /// WETH gross of the fire (hop 2's output less the loan).
    pub gross_wei: Option<String>,
    /// Hop 1's output beyond `mid`, left in the R26 executor. The lean
    /// executor (R29) sells it in hop 2: absent since R36's lean quote.
    pub residue: Option<String>,
    pub quote_error: Option<String>,
    pub sim: Option<SimCheck>,
    pub base_fee_wei: String,
    pub per_f: Vec<PerF>,
    pub weth_usd: f64,
    /// R37: the fire's start token when not WETH. Its gross is then
    /// `gross_start` of that token's units, and `gross_wei` their worth at
    /// the run's WETH price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gross_start: Option<String>,
    /// R37: the gross in USD: USDC at 1:1, WETH at the run's price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gross_usd: Option<f64>,
}

/// The arm's surplus in wei, which its bids per gas are swept on: a USDC
/// arm's USDC units at the run's WETH price (R37), and nothing without one.
pub fn surplus_in_wei(arm: &Arm, weth_usd: f64) -> U256 {
    let surplus = U256::from_dec_str(&arm.surplus_wei).unwrap_or_default();
    match arm.start {
        None => surplus,
        Some(_) => fire_gross(arm.start, surplus, weth_usd).wei.unwrap_or_default(),
    }
}

/// A fire's gross, in its start token's units, in wei, and in USD (R37).
#[derive(Clone, Debug, PartialEq)]
pub struct FireGross {
    /// A USDC fire's: its USDC units. None for WETH, whose units are wei.
    pub start_units: Option<U256>,
    /// None for a USDC fire without a WETH price.
    pub wei: Option<U256>,
    pub usd: Option<f64>,
}

/// `gross` of the start token `start` (None: WETH) valued: USDC at a dollar,
/// and in ETH at `weth_usd`; WETH at `weth_usd`. Reporting only: no arm is
/// sized or sent on it.
pub fn fire_gross(start: Option<Address>, gross: U256, weth_usd: f64) -> FireGross {
    let priced = weth_usd.is_finite() && weth_usd > 0.0;
    let units = gross.low_u128() as f64;
    match start {
        None => FireGross { start_units: None, wei: Some(gross), usd: priced.then(|| units * weth_usd / 1e18) },
        Some(_) => FireGross {
            start_units: Some(gross),
            wei: priced.then(|| U256::from((units * 1e12 / weth_usd) as u128)),
            usd: Some(units / 1e6),
        },
    }
}

#[derive(Debug, Default, Serialize)]
pub struct RunReport {
    pub judged: usize,
    pub fired: usize,
    pub quoted: usize,
    pub simulated: usize,
    pub sim_agreed: usize,
    pub cursor: u64,
}

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn hex_u128(v: &Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

/// The JSON lines of a daily ledger for `days` days up to `now`.
fn read_lines<T: serde::de::DeserializeOwned>(base: &Path, now: u64, days: u64) -> Vec<T> {
    let mut out = Vec::new();
    for d in (0..days).rev() {
        let path: PathBuf = ledger::daily_path(base, now.saturating_sub(d * 86_400_000_000_000));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        out.extend(text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()));
    }
    out
}

/// Judge every arm closed since the last run.
pub async fn run(config: &ShadowConfig) -> Result<RunReport, String> {
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let rpc: Arc<dyn RpcTransport> = Arc::new(
        FailoverTransport::connect(
            &urls,
            config.chain_id,
            FailoverSettings { request_timeout: EVAL_REQUEST_TIMEOUT, ..FailoverSettings::default() },
        )
            .await
            .map_err(|e| e.to_string())?,
    );
    drop(urls);
    let reads = ChainReads::new(Arc::clone(&rpc));
    let head = reads.head().await.map_err(|e| e.to_string())?;
    let dir = config.armed.verdicts.parent().map(Path::to_path_buf).unwrap_or_default();
    let cursor_path = dir.join("armed-eval.cursor");
    let cursor: u64 = std::fs::read_to_string(&cursor_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let now = now_ns();

    // Today's and yesterday's arms: a window is seconds long.
    let lines: Vec<ArmedLine> = read_lines(&config.armed.ledger, now, 2);
    let due: Vec<Arm> = merge_renewals(lines)
        .into_iter()
        .filter(|a| a.to_block > cursor && a.to_block + SETTLE_BLOCKS <= head)
        .collect();
    let mut report = RunReport { cursor, ..Default::default() };
    if due.is_empty() {
        return Ok(report);
    }
    let new_cursor = due.iter().map(|a| a.to_block).max().unwrap_or(cursor);
    let pools: BTreeSet<Address> = due.iter().flat_map(|a| a.pools.iter().filter_map(|p| p.parse().ok())).collect();
    // R36: an armed pool may sit below the floor (a census target): every pool
    // that was armed is read whatever its depth.
    let hooks = config.universe.hooks()?;
    let specs = inventory::load(&config.inventory, config.universe.filter(), &pools, &hooks).map_err(|e| e.to_string())?;
    let venues: BTreeMap<Address, Venue> = specs.iter().map(|s| (s.pool, s.venue)).collect();
    let v4_ids: Vec<B256> = specs.iter().filter_map(|s| s.v4.map(|k| k.pool_id)).collect();
    let from = due.iter().map(|a| a.armed_block + 1).min().unwrap_or(head);
    let to = due.iter().map(|a| a.to_block).max().unwrap_or(head);
    let logs = get_logs(&*rpc, &pools, &v4_ids, from, to).await?;
    let effects = effects_of(&logs, &venues);
    let flashblocks: Vec<FlashblockSeen> = config.flashblocks.as_ref().map(|f| read_lines(&f.ledger, now, 2)).unwrap_or_default();
    let weth_usd = weth_usd(&reads, head).await.unwrap_or(0.0);

    let mut judged: Vec<(Arm, eval::Judged)> = Vec::new();
    for arm in due {
        let own: Vec<TxEffect> = effects
            .iter()
            .filter(|e| {
                // A v4 pool's words are PoolManager's: its writes carry the
                // pool's key address in a Tick or a Liquidity's `pool` (R37).
                e.writes.iter().any(|w| match w {
                    Write::Set { address, .. } | Write::Tick { address, .. } => arm.pools.contains(address),
                    Write::Liquidity { address, pool, .. } => arm.pools.contains(address) || arm.pools.contains(pool),
                })
            })
            .cloned()
            .collect();
        if let Some(j) = judge(&arm, &own) {
            judged.push((arm, j));
        }
    }
    // A slot's rungs that fire on one transaction: the highest executes first.
    let mut top: BTreeMap<(String, u8, u64, u64), (u8, u64)> = BTreeMap::new();
    for (a, j) in &judged {
        if let Some(f) = &j.fire {
            let e = top.entry((a.route_hash.clone(), a.trigger_leg, f.block, f.tx_index)).or_insert((a.rung, a.arm_id));
            if a.rung > e.0 {
                *e = (a.rung, a.arm_id);
            }
        }
    }
    let ctx = Ctx { reads: &reads, rpc: &*rpc, config, specs: &specs, hooks: &hooks, logs: &logs, flashblocks: &flashblocks, weth_usd, now };
    let shadowed: Vec<bool> = judged
        .iter()
        .map(|(arm, j)| {
            j.fire.as_ref().is_some_and(|f| {
                top.get(&(arm.route_hash.clone(), arm.trigger_leg, f.block, f.tx_index)).is_some_and(|(_, id)| *id != arm.arm_id)
            })
        })
        .collect();
    // R37: a sample of the run's fires is simulated; every one is quoted.
    let fired: Vec<(usize, (u64, String))> = judged
        .iter()
        .enumerate()
        .filter_map(|(i, (arm, j))| j.fire.as_ref().filter(|_| !shadowed[i]).map(|f| (i, (arm.arm_id, f.tx.clone()))))
        .collect();
    let keys: Vec<(u64, String)> = fired.iter().map(|(_, k)| k.clone()).collect();
    let simulate: BTreeSet<usize> =
        sample_for_simulation(&keys, config.armed.eval_simulations_per_run).into_iter().map(|k| fired[k].0).collect();
    for (i, (arm, j)) in judged.iter().enumerate() {
        report.judged += 1;
        let v = verdict(&ctx, arm, j, shadowed[i], simulate.contains(&i), &mut report).await;
        ledger::append_line(&config.armed.verdicts, now, &v).map_err(|e| e.to_string())?;
    }
    report.cursor = new_cursor;
    std::fs::write(&cursor_path, report.cursor.to_string()).map_err(|e| e.to_string())?;
    Ok(report)
}

struct Ctx<'a> {
    reads: &'a ChainReads,
    rpc: &'a dyn RpcTransport,
    config: &'a ShadowConfig,
    specs: &'a [PoolSpec],
    hooks: &'a HookAllowlist,
    logs: &'a [RawLog],
    flashblocks: &'a [FlashblockSeen],
    weth_usd: f64,
    now: u64,
}

/// The `n` fires (arm ID, fire transaction) a run simulates, by index: the
/// `n` whose keccak is lowest, so the sample is spread evenly and fixed by the
/// fires themselves, whatever their order.
pub fn sample_for_simulation(fires: &[(u64, String)], n: usize) -> BTreeSet<usize> {
    let mut ranked: Vec<(B256, usize)> = fires
        .iter()
        .enumerate()
        .map(|(i, (arm_id, tx))| (keccak256([arm_id.to_be_bytes().as_slice(), tx.as_bytes()].concat()), i))
        .collect();
    ranked.sort();
    ranked.into_iter().take(n).map(|(_, i)| i).collect()
}

async fn verdict(ctx: &Ctx<'_>, arm: &Arm, j: &eval::Judged, shadowed: bool, simulate_it: bool, report: &mut RunReport) -> Verdict {
    let mut v = Verdict {
        at_ns: ctx.now,
        arm_id: arm.arm_id,
        route_hash: arm.route_hash.clone(),
        trigger_leg: arm.trigger_leg,
        rung: arm.rung,
        fired: j.fire.is_some(),
        trigger_met: Some(j.trigger_met),
        shadowed,
        fire: None,
        fire_closes_flashblock: None,
        rival: None,
        rival_priority_wei: None,
        ours: false,
        own: None,
        gross_wei: None,
        residue: None,
        quote_error: None,
        sim: None,
        base_fee_wei: arm.base_fee_wei.clone(),
        per_f: Vec::new(),
        weth_usd: ctx.weth_usd,
        start: arm.start,
        gross_start: None,
        gross_usd: None,
    };
    let Some(fire) = &j.fire else { return v };
    report.fired += 1;
    // The block's transactions only for a simulation: it replays them.
    let block = block_object(ctx.rpc, fire.block, simulate_it).await;
    let base_fee = block.as_ref().and_then(|b| hex_u128(b.get("baseFeePerGas")?)).unwrap_or(0);
    v.base_fee_wei = base_fee.to_string();

    // The exact quote at the fire, in its start token's units (R37).
    let mut quoted = None;
    match quote_at_fire(ctx.reads, ctx.specs, ctx.hooks, arm, fire, ctx.logs).await {
        Ok(gross) => {
            report.quoted += 1;
            let g = fire_gross(arm.start, gross, ctx.weth_usd);
            v.gross_wei = g.wei.map(|w| w.to_string());
            v.gross_start = g.start_units.map(|u| u.to_string());
            v.gross_usd = g.usd;
            quoted = Some(gross.to_string());
        }
        Err(e) => v.quote_error = Some(e),
    }
    // The simulation check, on the run's sample.
    if let Some(b) = block.as_ref().filter(|_| simulate_it) {
        if let Some((sim, replay)) = simulate(ctx.rpc, ctx.config, arm, fire, b).await {
            report.simulated += 1;
            // A replayed transaction can run differently from the chain (its
            // amounts can hang on one that fails in replay). The quote is the
            // chain's; the simulation is held to a quote on its own state.
            let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
            let diverged = replay_diverged(ctx.logs, &replay, fire.block, fire.tx_index, &pools);
            // Both in the start token's units: the executor's Profit is.
            let expected = if diverged {
                quote_at_fire(ctx.reads, ctx.specs, ctx.hooks, arm, fire, &replay).await.ok().map(|g| g.to_string())
            } else {
                quoted.clone()
            };
            let agrees = expected.is_some() && expected.as_deref() == sim.gross_wei.as_deref();
            report.sim_agreed += usize::from(agrees);
            v.sim = Some(SimCheck { agrees, replay_diverged: diverged, ..sim });
        }
    }
    // Positions and the rival's bid. Flashblocks when the ledger places both;
    // otherwise whole blocks (index 0), where a rival in the fire's own block
    // competes by fee and the pessimistic case gives it the block.
    let fire_fb = locate(ctx.flashblocks, fire.block, fire.tx_index);
    let rival_fb = j.rival.as_ref().map(|r| locate(ctx.flashblocks, r.block, r.tx_index));
    let granular = fire_fb.is_some() && rival_fb.is_none_or(|x| x.is_some());
    v.fire = Some(Position { block: fire.block, tx_index: fire.tx_index, tx: fire.tx.clone(), flashblock: fire_fb });
    v.fire_closes_flashblock = closes_flashblock(ctx.flashblocks, fire.block, fire.tx_index);
    let fire_at = (fire.block, if granular { fire_fb.unwrap_or(0) } else { 0 });
    let closes = granular && v.fire_closes_flashblock == Some(true);
    let mut rival_at = None;
    let mut rival_bid = 0u128;
    if let Some(r) = &j.rival {
        let r_fb = rival_fb.flatten();
        let tx = rival_tx(ctx.rpc, r).await;
        rival_bid = tx.as_ref().map_or(0, |t| t.priority);
        let executor = arm.executor.unwrap_or(ctx.config.executor.address);
        v.ours = tx.as_ref().is_some_and(|t| is_own_send(arm, executor, t.to, &t.input));
        v.rival = Some(Position { block: r.block, tx_index: r.tx_index, tx: r.tx.clone(), flashblock: r_fb });
        v.rival_priority_wei = Some(rival_bid.to_string());
        rival_at = Some((r.block, if granular { r_fb.unwrap_or(0) } else { 0 }));
    }
    let judge_bid = |bid: u128| if v.ours { own_landing(bid, rival_bid) } else { landing(fire_at, closes, rival_at, bid, rival_bid) };
    let surplus = surplus_in_wei(arm, ctx.weth_usd);
    let gross = v.gross_wei.as_deref().and_then(|g| U256::from_dec_str(g).ok());
    let other: u128 = arm.other_wei.parse().unwrap_or(0);
    let net = |bid: u128| gross.map(|g| net_wei(g, arm.gas_expected, base_fee, bid, other).to_string());
    for f in F_PERMILLE {
        let bid = bid_wei_per_gas(surplus, arm.gas_expected, f);
        v.per_f.push(PerF { f_permille: f, bid_wei_per_gas: bid.to_string(), landing: judge_bid(bid), net_wei: net(bid) });
    }
    if let Ok(bid) = arm.bid_wei_per_gas.parse::<u128>() {
        v.own = Some(OwnBid { bid_wei_per_gas: bid.to_string(), landing: judge_bid(bid), net_wei: net(bid) });
    }
    v
}

/// The book of `pools` as it stood after transaction `tx_index` of `block`: the
/// pools at `block`'s parent, then that block's writes to them through that
/// transaction ([`apply_through`]).
pub async fn book_after(
    reads: &ChainReads,
    specs: &[PoolSpec],
    hooks: &HookAllowlist,
    pools: &[Address],
    block: u64,
    tx_index: u64,
    logs: &[RawLog],
) -> Result<Arc<BTreeMap<Address, Arc<crate::live::book::PoolSnapshot>>>, String> {
    let mine: Vec<PoolSpec> = specs.iter().filter(|s| pools.contains(&s.pool)).cloned().collect();
    let (book, unloaded) = PoolBook::load_with_hooks(reads, &mine, block - 1, hooks).await.map_err(|e| e.to_string())?;
    if !unloaded.is_empty() {
        return Err("a pool did not load at the block's parent".into());
    }
    apply_through(&book, pools, block, tx_index, logs)?;
    Ok(book.snapshot())
}

/// `block`'s writes to `pools` through transaction `tx_index`, applied to
/// `book`: swaps, syncs, and Mints and Burns as the live book applies them
/// (R28.1). A position change the book cannot apply exactly refuses, because
/// the book would need a read at the block to price it. A v4 pool's are
/// PoolManager's logs, keyed by its pool ID (R37).
pub fn apply_through(book: &PoolBook, pools: &[Address], block: u64, tx_index: u64, logs: &[RawLog]) -> Result<(), String> {
    let pool_of = |l: &RawLog| if l.address == POOL_MANAGER { l.topics.get(1).map(|id| key_address(*id)) } else { Some(l.address) };
    let mut writes = Vec::new();
    for l in logs.iter().filter(|l| {
        !l.removed
            && l.block_number == Some(block)
            && pool_of(l).is_some_and(|p| pools.contains(&p))
            && l.transaction_index.is_some_and(|i| i <= tx_index)
    }) {
        let at = (block, l.log_index.unwrap_or(0));
        match l.topics.first() {
            Some(t) if (*t == SWAP || *t == PANCAKE_SWAP) && l.address != POOL_MANAGER => {
                let s = decode_swap(l).ok_or("an undecodable swap")?;
                writes.push(StateWrite::Swap(SwapWrite { pool: s.pool, sqrt_price_x96: s.sqrt_price_x96, liquidity: s.liquidity, tick: s.tick, at }));
            }
            Some(t) if *t == V4_SWAP && l.address == POOL_MANAGER => {
                let s = decode_v4_swap(l).ok_or("an undecodable v4 swap")?;
                writes.push(StateWrite::Swap(SwapWrite { pool: s.pool, sqrt_price_x96: s.sqrt_price_x96, liquidity: s.liquidity, tick: s.tick, at }));
            }
            Some(t) if *t == MODIFY_LIQUIDITY && l.address == POOL_MANAGER => {
                let p = decode_modify_liquidity(l).ok_or("a v4 position change past 128 bits")?;
                writes.push(StateWrite::Liquidity(LiquidityWrite {
                    pool: p.pool,
                    tick_lower: p.tick_lower,
                    tick_upper: p.tick_upper,
                    delta: p.delta,
                    at,
                }));
            }
            Some(t) if *t == SYNC && l.address != POOL_MANAGER => {
                let s = decode_sync(l).ok_or("an undecodable sync")?;
                writes.push(StateWrite::Sync(SyncWrite { pool: s.pool, reserve0: s.reserve0, reserve1: s.reserve1, at }));
            }
            Some(t) if (*t == MINT || *t == BURN) && l.address != POOL_MANAGER => {
                let p = decode_liquidity(l).ok_or("an undecodable mint or burn")?;
                writes.push(StateWrite::Liquidity(LiquidityWrite {
                    pool: p.pool,
                    tick_lower: p.tick_lower,
                    tick_upper: p.tick_upper,
                    delta: p.delta,
                    at,
                }));
            }
            _ => {}
        }
    }
    let applied = book.apply_writes(&writes);
    if writes.iter().zip(&applied).any(|(w, a)| matches!(w, StateWrite::Liquidity(_)) && *a == SwapApplied::NeedsReload) {
        return Err("a mint or burn before the trigger the book cannot apply exactly".into());
    }
    Ok(())
}

/// The fire, quoted exactly: both pools at the fire block's parent, that
/// block's writes to them up to and including the trigger, then the armed
/// plan's hops. The gross, in the start token's units (R37).
async fn quote_at_fire(
    reads: &ChainReads,
    specs: &[PoolSpec],
    hooks: &HookAllowlist,
    arm: &Arm,
    fire: &Point,
    logs: &[RawLog],
) -> Result<U256, String> {
    let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
    let snap = book_after(reads, specs, hooks, &pools, fire.block, fire.tx_index, logs).await?;
    let (_, cycles) = frontier::build_starts(ChainId::BASE, &[WETH, apex_exec::lean2::USDC], &snap);
    let cycle = cycle_through(&cycles, &arm.pools).ok_or("the route is not in the reloaded pools")?;
    let wei = |s: &str| U256::from_dec_str(s).map_err(|e| e.to_string());
    super::plan::armed_gross(cycle, &snap, wei(&arm.size_wei)?, wei(&arm.mid)?, wei(&arm.output_wei)?)
}

/// The cycle through `pools` in leg order. Not by route hash: that carries
/// each pool's fee, and a Slipstream fee moves with the tick, while the armed
/// plan names only the pools.
pub fn cycle_through<'a>(cycles: &'a BTreeMap<RouteId, Cycle>, pools: &[String; 2]) -> Option<&'a Cycle> {
    let pools: Vec<Address> = pools.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
    cycles.values().find(|c| c.legs[0].pool == pools[0] && c.legs[1].pool == pools[1])
}

/// The armed call simulated where it would have run: the fire block's earlier
/// transactions on the arm's pools replayed, then the call from the trader.
/// Call objects as `live::sim` sends them (`data`, proven on BlockPI).
/// Every transaction of `block` up to and including `fire_tx_index`, in order,
/// as `eth_simulateV1` calls (a creation without `to`). The whole block so far,
/// not only what touched the arm's pools: a transaction that touched them can
/// depend on one that did not, and replaying only the first made 22 exact fires
/// revert on 2026-10-12. `None` when the block has no transaction list.
pub fn replay_calls(block: &Value, fire_tx_index: u64) -> Option<Vec<Value>> {
    let calls = block
        .get("transactions")?
        .as_array()?
        .iter()
        .filter(|t| t.get("transactionIndex").and_then(hex_u128).is_some_and(|i| i <= u128::from(fire_tx_index)))
        .map(|t| {
            let mut c = json!({"from": t["from"], "data": t["input"], "value": t["value"], "gas": t["gas"]});
            if let Some(to) = t.get("to").filter(|to| !to.is_null()) {
                c["to"] = to.clone();
            }
            c
        })
        .collect();
    Some(calls)
}

/// The `eth_simulateV1` request for `arm` firing at `fire`: on the parent
/// block's state, the fire's block so far ([`replay_calls`]), then the
/// transaction that would execute in that block ([`Arm::calldata_at`]) from
/// `signer` at the arm's gas limit. With `pre`, the executor is simulated
/// before its deploy (R37): its code put at `executor`, and its constructor's
/// work done by overrides (`executors[signer]`, slot 1's mapping; a recipient
/// in slot 2) and by its approvals to Morpho Blue, called before ours.
pub fn simulate_request(arm: &Arm, fire: &Point, block: &Value, signer: Address, executor: Address, pre: Option<&PreDeploy>) -> Option<Value> {
    let mut calls = replay_calls(block, fire.tx_index)?;
    if pre.is_some() {
        let approve = format!("0x095ea7b3{:0>64}{}", alloy_primitives::hex::encode(crate::live::lenders::MORPHO_BLUE), "f".repeat(64));
        for token in [WETH, apex_exec::lean2::USDC] {
            calls.push(json!({"from": format!("{executor:#x}"), "to": format!("{token:#x}"), "data": approve}));
        }
    }
    calls.push(json!({
        "from": format!("{signer:#x}"),
        "to": format!("{executor:#x}"),
        "data": arm.calldata_at(fire.block),
        "gas": format!("{:#x}", arm.gas_limit),
    }));
    let mut state = json!({
        "blockOverrides": {
            "number": format!("{:#x}", fire.block),
            "time": block["timestamp"],
            "baseFeePerGas": block["baseFeePerGas"],
        },
        "calls": calls,
    });
    if let Some(p) = pre {
        let mut key = [0u8; 64];
        key[12..32].copy_from_slice(signer.as_slice());
        key[63] = 1;
        state["stateOverrides"] = json!({ format!("{executor:#x}"): {
            "code": p.code,
            "stateDiff": {
                format!("{:#x}", keccak256(key)): format!("0x{:064x}", 1),
                format!("0x{:064x}", 2): format!("0x{:0>64}", alloy_primitives::hex::encode(signer)),
            },
        }});
    }
    Some(json!([{ "blockStateCalls": [state], "validation": false }, format!("{:#x}", fire.block.checked_sub(1)?)]))
}

/// R37: `LeanExecutor2`'s runtime code, simulated at its address before it
/// is deployed (`armed.executor2.runtime_code`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreDeploy {
    pub code: String,
}

/// The simulation's check, and the logs its replay wrote to the arm's pools.
async fn simulate(
    rpc: &dyn RpcTransport,
    config: &ShadowConfig,
    arm: &Arm,
    fire: &Point,
    block: &Value,
) -> Option<(SimCheck, Vec<RawLog>)> {
    // R37: the executor the arm was built for; LeanExecutor2 before its
    // deploy runs its configured code. Code that does not read is no check.
    let executor = arm.executor.unwrap_or(config.executor.address);
    let pre = match &config.armed.executor2 {
        Some(e) if e.address == executor => match &e.runtime_code {
            Some(path) => Some(PreDeploy { code: format!("0x{}", std::fs::read_to_string(path).ok()?.trim().trim_start_matches("0x")) }),
            None => None,
        },
        _ => None,
    };
    let params = simulate_request(arm, fire, block, config.signer.address, executor, pre.as_ref())?;
    let out = rpc.call("eth_simulateV1", params).await.ok()?;
    let calls = out.get(0)?.get("calls")?.as_array()?;
    let last = calls.last()?.clone();
    let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
    let replayed = calls.len() - 1 - if pre.is_some() { 2 } else { 0 };
    let replay = replayed_logs(block, fire.tx_index, &calls[..replayed], &pools);
    let ok = last.get("status").and_then(Value::as_str) == Some("0x1");
    let gas_used = last.get("gasUsed").and_then(hex_u128).map_or(0, |g| u64::try_from(g).unwrap_or(u64::MAX));
    let gross = simulated_gross(&last);
    Some((SimCheck { ok, gross_wei: gross.map(|g| g.to_string()), gas_used, agrees: false, replay_diverged: false }, replay))
}

/// The gross a simulated call's executor reported: the lean executor's
/// `Profit(uint256 gross)` (R29).
pub fn simulated_gross(call: &Value) -> Option<U256> {
    let topic = keccak256(b"Profit(uint256)");
    call.get("logs").and_then(Value::as_array).and_then(|logs| {
        logs.iter().find_map(|l| {
            let t0: B256 = l.get("topics")?.get(0)?.as_str()?.parse().ok()?;
            let data = l.get("data")?.as_str()?;
            (t0 == topic).then(|| U256::from_str_radix(data.get(2..66)?, 16).ok()).flatten()
        })
    })
}

/// What the replay itself wrote to `pools`, as the book applies it (R27):
/// each Swap, Sync, Mint or Burn from a replayed transaction, under that
/// transaction's real index and hash, in order. `results` are the replay's
/// calls, as many as `block` has transactions up to `tx_index`.
pub fn replayed_logs(block: &Value, tx_index: u64, results: &[Value], pools: &[Address]) -> Vec<RawLog> {
    let number = block.get("number").and_then(hex_u128).and_then(|n| u64::try_from(n).ok());
    let txs: Vec<(u64, Option<B256>)> = block
        .get("transactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let i = u64::try_from(t.get("transactionIndex").and_then(hex_u128)?).ok()?;
            (i <= tx_index).then(|| (i, t.get("hash").and_then(|h| h.as_str()?.parse().ok())))
        })
        .collect();
    let applied = [SWAP, PANCAKE_SWAP, SYNC, MINT, BURN];
    let mut out = Vec::new();
    let mut log_index = 0u64;
    for ((index, hash), r) in txs.iter().zip(results) {
        for l in r.get("logs").and_then(Value::as_array).into_iter().flatten() {
            let Some(address) = l.get("address").and_then(|a| a.as_str()?.parse::<Address>().ok()) else { continue };
            let topics: Vec<B256> =
                l.get("topics").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.as_str()?.parse().ok()).collect();
            let Some(data) = l.get("data").and_then(Value::as_str).and_then(|d| alloy_primitives::hex::decode(d).ok()) else { continue };
            if !pools.contains(&address) || !topics.first().is_some_and(|t| applied.contains(t)) {
                continue;
            }
            out.push(RawLog {
                pending: false,
                removed: false,
                address,
                topics,
                data,
                block_number: number,
                transaction_hash: *hash,
                log_index: Some(log_index),
                transaction_index: Some(*index),
            });
            log_index += 1;
        }
    }
    out
}

/// Whether the replay's writes to `pools` in `block`, through `tx_index`,
/// differ from the chain's (R27).
pub fn replay_diverged(chain: &[RawLog], replay: &[RawLog], block: u64, tx_index: u64, pools: &[Address]) -> bool {
    let writes = |logs: &[RawLog]| {
        logs.iter()
            .filter(|l| {
                !l.removed
                    && l.block_number == Some(block)
                    && pools.contains(&l.address)
                    && l.transaction_index.is_some_and(|i| i <= tx_index)
            })
            .map(|l| (l.address, l.transaction_index, l.topics.first().copied(), l.data.clone()))
            .collect::<Vec<_>>()
    };
    writes(chain) != writes(replay)
}

/// Whether a provider refused a log query for returning too many results
/// (BlockPI: "query exceeds max results 20000"; geth-style: "query returned
/// more than 10000 results"): a narrower range will do.
fn too_many_logs(error: &str) -> bool {
    error.contains("exceeds max results") || error.contains("returned more than")
}

/// The watched pools' logs over `from..=to`, in block order, `LOG_CHUNK`
/// blocks a query. A range with more logs than the provider returns at once is
/// halved until it fits: a failed run never advances the cursor, so one busy
/// stretch would otherwise stall the verdicts for good. Any other failure is
/// returned at once.
/// R37: and PoolManager's Swap and ModifyLiquidity for the v4 pools `v4_ids`,
/// by pool ID in topic 1. In chain order.
pub async fn get_logs(rpc: &dyn RpcTransport, pools: &BTreeSet<Address>, v4_ids: &[B256], from: u64, to: u64) -> Result<Vec<RawLog>, String> {
    let addresses: Vec<String> = pools.iter().map(|a| format!("{a:#x}")).collect();
    let topics: Vec<String> = [SWAP, PANCAKE_SWAP, SYNC, MINT, BURN].iter().map(|t| format!("{t:#x}")).collect();
    let mut out = logs_of(rpc, &json!({"address": addresses, "topics": [topics]}), from, to).await?;
    if !v4_ids.is_empty() {
        let topics0: Vec<String> = [V4_SWAP, MODIFY_LIQUIDITY].iter().map(|t| format!("{t:#x}")).collect();
        let ids: Vec<String> = v4_ids.iter().map(|i| format!("{i:#x}")).collect();
        out.extend(logs_of(rpc, &json!({"address": [format!("{POOL_MANAGER:#x}")], "topics": [topics0, ids]}), from, to).await?);
        out.sort_by_key(|l| (l.block_number, l.transaction_index, l.log_index));
    }
    Ok(out)
}

/// `filter`'s logs from `from` to `to`, a range too full for one query split
/// until it fits.
async fn logs_of(rpc: &dyn RpcTransport, filter: &Value, from: u64, to: u64) -> Result<Vec<RawLog>, String> {
    let mut out = Vec::new();
    // Ranges still to query, the next one last.
    let mut todo: Vec<(u64, u64)> = Vec::new();
    let mut start = to.saturating_add(1);
    while start > from {
        let first = start.saturating_sub(LOG_CHUNK).max(from);
        todo.push((first, start - 1));
        start = first;
    }
    while let Some((start, end)) = todo.pop() {
        let mut q = filter.clone();
        q["fromBlock"] = json!(format!("{start:#x}"));
        q["toBlock"] = json!(format!("{end:#x}"));
        let answer = rpc.call("eth_getLogs", json!([q])).await.map_err(|e| e.to_string());
        match answer {
            Ok(v) => out.extend(v.as_array().ok_or("eth_getLogs: not an array")?.iter().filter_map(|l| parse_log(l, false))),
            Err(e) if end > start && too_many_logs(&e) => {
                let mid = start + (end - start) / 2;
                todo.push((mid + 1, end));
                todo.push((start, mid));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

async fn block_object(rpc: &dyn RpcTransport, n: u64, full: bool) -> Option<Value> {
    rpc.call("eth_getBlockByNumber", json!([format!("{n:#x}"), full])).await.ok().filter(|v| !v.is_null())
}

/// The rival transaction: its effective priority fee per gas, and what it
/// called, to know our own send (R37).
struct RivalTx {
    priority: u128,
    to: Option<Address>,
    input: String,
}

async fn rival_tx(rpc: &dyn RpcTransport, r: &Point) -> Option<RivalTx> {
    let t = rpc.call("eth_getTransactionByHash", json!([r.tx])).await.ok()?;
    let b = block_object(rpc, r.block, false).await?;
    let base = hex_u128(b.get("baseFeePerGas")?)?;
    let priority = match (t.get("maxPriorityFeePerGas").and_then(hex_u128), t.get("maxFeePerGas").and_then(hex_u128)) {
        (Some(p), Some(m)) => p.min(m.saturating_sub(base)),
        _ => hex_u128(t.get("gasPrice")?)?.saturating_sub(base),
    };
    let to = t.get("to").and_then(Value::as_str).and_then(|a| a.parse().ok());
    let input = t.get("input").and_then(Value::as_str).unwrap_or_default().to_string();
    Some(RivalTx { priority, to, input })
}

/// The rival is this arm's own send: a call to `executor` with the arm's
/// calldata or one of its renewals'. Only the executor's own senders can run
/// it, and a sibling arm's send carries its own calldata.
pub fn is_own_send(arm: &Arm, executor: Address, to: Option<Address>, input: &str) -> bool {
    to == Some(executor)
        && std::iter::once(&arm.calldata).chain(arm.renewals.iter().map(|r| &r.calldata)).any(|c| c.eq_ignore_ascii_case(input))
}

/// Whether transaction `tx_index` of `block` was its flashblock's last, from
/// the ledger; `None` when the ledger does not place it.
pub fn closes_flashblock(fbs: &[FlashblockSeen], block: u64, tx_index: u64) -> Option<bool> {
    fbs.iter().filter(|f| f.block == block).find_map(|f| {
        let first = u64::from(f.first_tx?);
        let end = first + u64::from(f.tx_count);
        (tx_index >= first && tx_index < end).then_some(tx_index + 1 == end)
    })
}

/// The flashblock that added `tx_index` to `block`, from the ledger.
fn locate(fbs: &[FlashblockSeen], block: u64, tx_index: u64) -> Option<u32> {
    fbs.iter()
        .filter(|f| f.block == block)
        .find(|f| {
            f.first_tx
                .is_some_and(|first| tx_index >= u64::from(first) && tx_index < u64::from(first) + u64::from(f.tx_count))
        })
        .map(|f| f.index)
}

/// WETH in USD from the deep Uniswap WETH/USDC pool's price at `head`.
async fn weth_usd(reads: &ChainReads, head: u64) -> Option<f64> {
    let pool: Address = "0xd0b53D9277642d899DF5C87A3966A349A798F224".parse().ok()?;
    let w = reads.storage_at(pool, U256::zero(), head).await.ok()?;
    let sqrt = (w & price_mask()).as_u128() as f64 / 2f64.powi(96);
    Some(sqrt * sqrt * 1e12)
}

/// One UTC day's figures for one bid fraction.
#[derive(Debug, Default, Serialize)]
pub struct DayF {
    pub fires: u64,
    pub wins_optimistic: u64,
    pub wins_pessimistic: u64,
    pub net_usd_optimistic: f64,
    pub net_usd_pessimistic: f64,
    /// Fires that would have landed and reverted: their fee is a cost.
    pub included_reverts: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// By UTC day, then bid fraction per mille.
    pub days: BTreeMap<String, BTreeMap<u32, DayF>>,
    pub arms_judged: u64,
    /// Arms that never fired: their trigger never met in the window, or met
    /// with the other pool outside its band. Rung steps against band widths.
    pub unfired_trigger_unmet: u64,
    pub unfired_band_missed: u64,
    pub simulated: u64,
    pub sim_agreed: u64,
    /// R37: by UTC day, at the arms' own bids, the ones armed-live sends.
    pub own: BTreeMap<String, DayF>,
    /// R37: fires the arm's own send took: armed-live's landings.
    pub ours: u64,
}

/// The bid curve so far, from every verdict file in `dir`. Reads files only.
pub fn summary(dir: &Path) -> Result<Summary, String> {
    let mut s = Summary::default();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("armed-verdicts-") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    for f in files {
        let day = f
            .file_stem()
            .and_then(|n| n.to_str())
            .map(|n| n.trim_start_matches("armed-verdicts-").to_string())
            .unwrap_or_default();
        for line in std::fs::read_to_string(&f).map_err(|e| e.to_string())?.lines() {
            let Ok(v) = serde_json::from_str::<Verdict>(line) else { continue };
            s.arms_judged += 1;
            match (v.fired, v.trigger_met) {
                (false, Some(false)) => s.unfired_trigger_unmet += 1,
                (false, Some(true)) => s.unfired_band_missed += 1,
                _ => {}
            }
            if let Some(sim) = &v.sim {
                s.simulated += 1;
                s.sim_agreed += u64::from(sim.agrees);
            }
            if !v.fired || v.shadowed {
                continue;
            }
            let reverts = v.quote_error.as_deref().is_some_and(|e| e.contains("revert"));
            let usd_of = |net: &Option<String>| net.as_deref().and_then(|n| n.parse::<f64>().ok()).map_or(0.0, |n| n / 1e18 * v.weth_usd);
            s.ours += u64::from(v.ours);
            if let Some(o) = &v.own {
                let d = s.own.entry(day.clone()).or_default();
                d.fires += 1;
                if o.landing.optimistic {
                    d.wins_optimistic += 1;
                    d.net_usd_optimistic += usd_of(&o.net_wei);
                    d.included_reverts += u64::from(reverts);
                }
                if o.landing.pessimistic {
                    d.wins_pessimistic += 1;
                    d.net_usd_pessimistic += usd_of(&o.net_wei);
                }
            }
            for p in &v.per_f {
                let d = s.days.entry(day.clone()).or_default().entry(p.f_permille).or_default();
                d.fires += 1;
                let usd = usd_of(&p.net_wei);
                if p.landing.optimistic {
                    d.wins_optimistic += 1;
                    d.net_usd_optimistic += usd;
                    d.included_reverts += u64::from(reverts);
                }
                if p.landing.pessimistic {
                    d.wins_pessimistic += 1;
                    d.net_usd_pessimistic += usd;
                }
            }
        }
    }
    Ok(s)
}
