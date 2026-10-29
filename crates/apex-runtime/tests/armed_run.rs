//! R26: `apex armed-eval`'s reading of the chain: logs into ordered effects on
//! the words the arms watch.

use alloy_primitives::{address, Address, B256};
use apex_chain::rpc::ws::RawLog;
use apex_runtime::armed::eval::Write;
use apex_runtime::armed::run::{effects_of, get_logs, replay_calls, simulate_request, summary};
use apex_runtime::live::feed::{BURN, MINT, SWAP, SYNC};
use apex_runtime::live::inventory::Venue;
use ethers_core::types::U256;
use std::collections::BTreeMap;

const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const AERO: Address = address!("01784ef301d79e4b2df3a21ad9a536d4cf09a5ce");

/// A signed 256-bit ABI word.
fn word(v: i128) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn log(address: Address, topics: Vec<B256>, data: Vec<[u8; 32]>, tx_index: u64, log_index: u64) -> RawLog {
    RawLog {
        pending: false,
        removed: false,
        address,
        topics,
        data: data.concat(),
        block_number: Some(100),
        transaction_hash: Some(B256::repeat_byte(tx_index as u8)),
        log_index: Some(log_index),
        transaction_index: Some(tx_index),
    }
}

/// A Swap sets the price word, the liquidity word and the tick; a Mint or Burn
/// moves liquidity over its range; a Sync sets both reserves; one
/// transaction's logs are one effect, in log order.
#[test]
fn logs_become_ordered_effects_on_the_watched_words() {
    let venues: BTreeMap<Address, Venue> = [(POOL, Venue::UniswapV3), (AERO, Venue::AerodromeV2)].into_iter().collect();
    let tick = |t: i32| B256::from(word(i128::from(t)));
    let swap = log(POOL, vec![SWAP, B256::ZERO, B256::ZERO], vec![word(-5), word(7), word(1 << 96), word(1_000), word(-12)], 4, 9);
    let mint = log(POOL, vec![MINT, B256::ZERO, tick(-60), tick(60)], vec![word(0), word(500), word(1), word(1)], 4, 10);
    let burn = log(POOL, vec![BURN, B256::ZERO, tick(-60), tick(60)], vec![word(200), word(1), word(1)], 6, 2);
    let sync = log(AERO, vec![SYNC], vec![word(11), word(22)], 6, 3);
    let mut removed = log(POOL, vec![BURN, B256::ZERO, tick(-60), tick(60)], vec![word(999), word(1), word(1)], 6, 4);
    removed.removed = true;
    let effects = effects_of(&[sync, burn, swap, removed, mint], &venues);
    assert_eq!(effects.len(), 2, "two transactions");
    assert_eq!((effects[0].tx_index, effects[1].tx_index), (4, 6), "chain order");
    let a = format!("{POOL:#x}");
    let b = format!("{AERO:#x}");
    assert_eq!(effects[0].writes, vec![
        Write::Set { address: a.clone(), slot: U256::from(0), value: U256::one() << 96 },
        Write::Set { address: a.clone(), slot: U256::from(4), value: U256::from(1_000u64) },
        Write::Tick { address: a.clone(), tick: -12 },
        Write::Liquidity { address: a.clone(), slot: U256::from(4), pool: format!("{POOL:#x}"), lower: -60, upper: 60, delta: 500 },
    ]);
    assert_eq!(effects[1].writes, vec![
        Write::Liquidity { address: a, slot: U256::from(4), pool: format!("{POOL:#x}"), lower: -60, upper: 60, delta: -200 },
        Write::Set { address: b.clone(), slot: U256::from(20), value: U256::from(11u64) },
        Write::Set { address: b, slot: U256::from(21), value: U256::from(22u64) },
    ], "the removed log is ignored");
}

/// One verdict line: the fields every verdict has, then `extra`.
fn verdict(arm_id: u64, fired: bool, extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({
        "at_ns": 0, "arm_id": arm_id, "route_hash": "0xroute", "trigger_leg": 0, "rung": 1,
        "fired": fired, "shadowed": false, "base_fee_wei": "5000000", "per_f": [], "weth_usd": 2500.0,
    });
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v.to_string()
}

fn per_f(f: u32, optimistic: bool, pessimistic: bool, net_wei: &str) -> serde_json::Value {
    serde_json::json!({
        "f_permille": f, "bid_wei_per_gas": "1",
        "landing": { "optimistic": optimistic, "pessimistic": pessimistic }, "net_wei": net_wei,
    })
}

/// The bid curve sums landed fires per day and bid fraction, in dollars at
/// each verdict's WETH price; a shadowed fire is not counted (its higher rung
/// is), a landed revert is; unfired arms split by what missed.
#[test]
fn the_summary_sums_the_bid_curve_and_splits_the_misses() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        verdict(1, true, serde_json::json!({
            "trigger_met": true,
            "sim": { "ok": true, "gross_wei": "1", "gas_used": 1, "agrees": true },
            // 0.002 WETH and 0.001 WETH at $2,500.
            "per_f": [per_f(100, true, false, "2000000000000000"), per_f(500, true, true, "1000000000000000")],
        })),
        verdict(2, true, serde_json::json!({ "shadowed": true, "per_f": [per_f(100, true, true, "9000000000000000000")] })),
        verdict(3, false, serde_json::json!({ "trigger_met": false })),
        verdict(4, false, serde_json::json!({ "trigger_met": true })),
        verdict(5, false, serde_json::json!({})),
        verdict(6, true, serde_json::json!({
            "quote_error": "execution reverted: min out",
            "per_f": [per_f(100, true, false, "-50000000000000")],
        })),
    ];
    std::fs::write(dir.path().join("armed-verdicts-2026-10-13.jsonl"), lines.join("\n") + "\n").unwrap();
    std::fs::write(dir.path().join("armed-2026-10-13.jsonl"), verdict(9, true, serde_json::json!({}))).unwrap();

    let s = summary(dir.path()).unwrap();
    assert_eq!(s.arms_judged, 6, "the arms ledger beside it is not read");
    assert_eq!((s.unfired_trigger_unmet, s.unfired_band_missed), (1, 1), "a verdict without the field counts in neither");
    assert_eq!((s.simulated, s.sim_agreed), (1, 1));
    let day = &s.days["2026-10-13"];
    let f100 = &day[&100];
    assert_eq!((f100.fires, f100.wins_optimistic, f100.wins_pessimistic, f100.included_reverts), (2, 2, 0, 1));
    assert_eq!(f100.net_usd_optimistic, 5.0 - 0.125);
    let f500 = &day[&500];
    assert_eq!((f500.fires, f500.wins_optimistic, f500.wins_pessimistic), (1, 1, 1));
    assert_eq!((f500.net_usd_optimistic, f500.net_usd_pessimistic), (2.5, 2.5));
    assert_eq!(day.len(), 2);
}

/// The simulation replays every transaction of the fire's block up to and
/// including the fire, in order, whatever it touched: one that touched our
/// pools can depend on one that did not (an approval, a transfer, a nonce), and
/// replaying only the first made 22 exact fires revert on 2026-10-12.
#[test]
fn a_fire_is_simulated_after_its_whole_block_so_far() {
    let tx = |i: u64, to: Option<&str>| {
        serde_json::json!({
            "transactionIndex": format!("{i:#x}"), "hash": format!("0x{i:064x}"), "from": "0x00000000000000000000000000000000000000f0",
            "to": to, "input": format!("0x{i:02x}"), "value": "0x0", "gas": "0x5208",
        })
    };
    let block = serde_json::json!({ "transactions": [
        tx(0, Some("0x4200000000000000000000000000000000000015")),
        tx(1, None),
        tx(2, Some("0x00000000000000000000000000000000000000aa")),
        tx(3, Some("0x00000000000000000000000000000000000000aa")),
    ]});
    let calls = replay_calls(&block, 2).unwrap();
    assert_eq!(calls.len(), 3, "transactions 0 to 2: the fire's own included, the one after it not");
    assert_eq!(calls.iter().map(|c| c["data"].as_str().unwrap()).collect::<Vec<_>>(), ["0x00", "0x01", "0x02"]);
    assert!(calls[1].get("to").is_none(), "a creation replays as one");
    assert_eq!(calls[2]["to"], "0x00000000000000000000000000000000000000aa");
    assert_eq!(calls[0]["gas"], "0x5208");
    assert!(replay_calls(&serde_json::json!({}), 2).is_none());
}

/// The simulated transaction is the one that would execute in the fire's
/// block: a renewal's, past the window before it, signed with its own deadline.
#[test]
fn a_fire_after_a_renewal_simulates_the_renewal() {
    use apex_runtime::armed::eval::Point;
    use apex_runtime::armed::lines::{Arm, Renewal};
    let text = r#"{"arm_id":7,"at_ns":0,"armed_block":100,"to_block":110,"route_hash":"0xr","trigger_leg":0,"rung":1,"step":3,
        "pools":["0xaa","0xbb"],"venues":["UniswapV3","UniswapV3"],"predicates":[],"start_words":[],"start_ticks":[],
        "size_wei":"1","mid":"1","output_wei":"2","surplus_wei":"1","cost_wei":"0","gas_expected":1,"gas_limit":900000,
        "base_fee_wei":"5000000","other_wei":"0","deadline":11,"calldata":"0xfirst","calldata_hash":"0x","sender_slot":0}"#;
    let mut arm: Arm = serde_json::from_str(text).unwrap();
    assert!(arm.renewals.is_empty(), "an arm line carries no renewals");
    arm.renewals.push(Renewal { after_block: 105, to_block: 110, deadline: 23, calldata: "0xrenewed".into() });
    let block = serde_json::json!({"timestamp": "0x10", "baseFeePerGas": "0x4c4b40", "transactions": [
        {"transactionIndex": "0x0", "from": "0x01", "to": "0x02", "input": "0x", "value": "0x0", "gas": "0x1"},
    ]});
    let signer: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
    let executor: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
    let at = |block: u64| Point { block, tx_index: 0, tx: "0x".into() };
    let ours = |p: serde_json::Value| p[0]["blockStateCalls"][0]["calls"].as_array().unwrap().last().unwrap().clone();
    let first = simulate_request(&arm, &at(104), &block, signer, executor, None).unwrap();
    assert_eq!(ours(first.clone())["data"], "0xfirst");
    assert_eq!(first[1], "0x67", "on the parent block's state");
    assert_eq!(first[0]["blockStateCalls"][0]["calls"].as_array().unwrap().len(), 2, "the block so far, then ours");
    let renewed = ours(simulate_request(&arm, &at(108), &block, signer, executor, None).unwrap());
    assert_eq!(renewed["data"], "0xrenewed");
    assert_eq!(renewed["gas"], "0xdbba0");
    assert_eq!(renewed["from"], format!("{signer:#x}"));
}

/// A provider that answers at most `cap` blocks of logs a query, one log a
/// block, and refuses wider ranges the way BlockPI does; or fails outright.
struct Capped {
    cap: u64,
    outright: bool,
    asked: std::sync::Mutex<Vec<(u64, u64)>>,
}

#[async_trait::async_trait]
impl apex_chain::rpc::RpcTransport for Capped {
    async fn call(&self, _method: &str, params: serde_json::Value) -> Result<serde_json::Value, apex_chain::rpc::RpcError> {
        let q = |k: &str| u64::from_str_radix(params[0][k].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
        let (from, to) = (q("fromBlock"), q("toBlock"));
        self.asked.lock().unwrap().push((from, to));
        let refuse = |last: &str| apex_chain::rpc::RpcError::Exhausted { method: "eth_getLogs".into(), endpoints: 1, last: last.into() };
        if self.outright {
            return Err(refuse("https://base.blockpi.network/***: error sending request: connection refused"));
        }
        if to - from + 1 > self.cap {
            return Err(refuse(&format!("https://base.blockpi.network/***: node error -32602: query exceeds max results 20000, retry with the range {from}-{}", from + self.cap - 1)));
        }
        Ok(serde_json::Value::Array(
            (from..=to)
                .map(|b| serde_json::json!({
                    "address": format!("{POOL:#x}"), "topics": [format!("{SWAP:#x}")], "data": "0x",
                    "blockNumber": format!("{b:#x}"), "transactionHash": format!("0x{b:064x}"),
                    "logIndex": "0x0", "transactionIndex": "0x0",
                }))
                .collect(),
        ))
    }
}

/// A range with more logs than the provider returns at once is split until it
/// fits, in order, instead of failing the run: a failed run never advances the
/// cursor, so one busy quarter hour (2026-10-13 00:00 UTC, 20,000 logs in 450
/// blocks) would have stalled the verdicts for good. Any other failure is
/// returned at once, without splitting.
#[tokio::test]
async fn a_range_too_full_for_one_query_is_split_until_it_fits() {
    let pools: std::collections::BTreeSet<Address> = [POOL].into_iter().collect();
    let rpc = Capped { cap: 70, outright: false, asked: Default::default() };
    let logs = get_logs(&rpc, &pools, &[], 1_000, 1_499).await.unwrap();
    assert_eq!(logs.iter().map(|l| l.block_number.unwrap()).collect::<Vec<_>>(), (1_000..=1_499).collect::<Vec<_>>(), "every block once, in order");
    assert!(rpc.asked.lock().unwrap().iter().all(|(f, t)| t >= f));

    let down = Capped { cap: 70, outright: true, asked: Default::default() };
    assert!(get_logs(&down, &pools, &[], 1_000, 1_499).await.unwrap_err().contains("connection refused"));
    assert_eq!(down.asked.lock().unwrap().len(), 1, "a provider that is down is not asked 500 more times");

    // A single block too full for one query cannot be split: its error returns.
    let full = Capped { cap: 0, outright: false, asked: Default::default() };
    assert!(get_logs(&full, &pools, &[], 1_000, 1_001).await.unwrap_err().contains("exceeds max results"));
    assert_eq!(*full.asked.lock().unwrap(), vec![(1_000, 1_001), (1_000, 1_000)]);
}

/// R27: what the replay itself wrote to the arm's pools, as the book applies
/// it: each log on a watched pool from a replayed transaction, under that
/// transaction's real index and hash, in order; nothing from other addresses.
/// A replay diverges from the chain when those differ from the chain's own
/// logs before the fire: at block 52,526,060 three swaps on the 0.05% pool
/// ran with other amounts, and a quote meant to match the simulation must be
/// taken on the replay's state.
#[test]
fn the_replays_own_logs_and_whether_they_diverge() {
    use apex_runtime::armed::run::{replay_diverged, replayed_logs};
    let other = address!("00000000000000000000000000000000000000cc");
    let block = serde_json::json!({ "number": "0x64", "transactions": [
        {"transactionIndex": "0x0", "hash": format!("0x{:064x}", 10)},
        {"transactionIndex": "0x1", "hash": format!("0x{:064x}", 11)},
        {"transactionIndex": "0x2", "hash": format!("0x{:064x}", 12)},
    ]});
    let swap_log = |addr: Address, data: &str| serde_json::json!({
        "address": format!("{addr:#x}"), "topics": [format!("{SWAP:#x}")], "data": data,
    });
    let results = vec![
        serde_json::json!({"status": "0x1", "logs": [
            swap_log(POOL, "0x01"),
            swap_log(other, "0x02"),
            // An event the book does not apply (a Collect): not a write.
            serde_json::json!({"address": format!("{POOL:#x}"), "topics": [format!("{:#x}", B256::repeat_byte(0x70))], "data": "0x05"}),
        ]}),
        serde_json::json!({"status": "0x0", "logs": []}),
        serde_json::json!({"status": "0x1", "logs": [swap_log(POOL, "0x03")]}),
        // Our own call, after the replay: not part of it.
        serde_json::json!({"status": "0x1", "logs": [swap_log(POOL, "0x04")]}),
    ];
    let replay = replayed_logs(&block, 2, &results, &[POOL]);
    assert_eq!(replay.len(), 2);
    assert_eq!((replay[0].transaction_index, replay[1].transaction_index), (Some(0), Some(2)));
    assert_eq!(replay[1].transaction_hash, Some(format!("0x{:064x}", 12).parse::<B256>().unwrap()));
    assert_eq!((replay[0].data.clone(), replay[1].data.clone()), (vec![1u8], vec![3u8]));
    assert!(replay.iter().all(|l| l.block_number == Some(100)), "the block's own number");

    let chain = |data: &[u8], tx: u64| apex_chain::rpc::ws::RawLog { data: data.to_vec(), transaction_index: Some(tx), ..replay[0].clone() };
    let same = vec![chain(&[1], 0), chain(&[3], 2)];
    let at = replay[0].block_number.unwrap();
    assert!(!replay_diverged(&same, &replay, at, 2, &[POOL]));
    assert!(replay_diverged(&[chain(&[1], 0), chain(&[9], 2)], &replay, at, 2, &[POOL]), "an amount differs");
    assert!(replay_diverged(&[chain(&[1], 0)], &replay, at, 2, &[POOL]), "a swap is missing");
    // The chain's logs after the fire, or on other pools, are not compared.
    let later = vec![chain(&[1], 0), chain(&[3], 2), chain(&[7], 3)];
    assert!(!replay_diverged(&later, &replay, at, 2, &[POOL]));
}

/// R29: a simulated fire's gross is the lean executor's `Profit(uint256)`; the
/// Phase 5 executor's `ProfitRealised` is not read as one.
#[test]
fn a_simulated_fires_gross_is_the_lean_executors_profit() {
    use apex_runtime::armed::run::simulated_gross;
    let profit = format!("{:#x}", alloy_primitives::keccak256(b"Profit(uint256)"));
    let realised = format!("{:#x}", alloy_primitives::keccak256(b"ProfitRealised(address,uint256,uint256)"));
    let word = format!("0x{:064x}", 180_256_364_565_428u128);
    let call = |topic: &str| serde_json::json!({ "logs": [{ "topics": [topic], "data": word }] });
    assert_eq!(simulated_gross(&call(&profit)), Some(ethers_core::types::U256::from(180_256_364_565_428u128)));
    assert_eq!(simulated_gross(&call(&realised)), None);
    assert_eq!(simulated_gross(&serde_json::json!({ "logs": [] })), None);
}

const L: u128 = 1_400_000_000_000_000_000;

/// A WETH/USDC Uniswap pool read at block 99, liquidity `L` over ±2,000 ticks.
fn read_pool() -> apex_runtime::live::book::PoolSnapshot {
    use apex_math::cl_math::get_sqrt_ratio_at_tick;
    use apex_math::cl_state::ClPoolState;
    use apex_math::cl_swap::TickLadder;
    let (tick, lower, upper) = (-197_350, -199_350, -195_350);
    apex_runtime::live::book::PoolSnapshot {
        spec: apex_runtime::live::inventory::PoolSpec {
            pool: POOL,
            venue: Venue::UniswapV3,
            token0: apex_runtime::live::frontier::WETH,
            token1: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
            fee_ppm: 500,
            depth_usd: 5_000_000.0,
            v4: None,
        },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: Some(U256::from(10u128.pow(21))),
            balance1: Some(U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: Venue::UniswapV3.factory(),
        code_hash: Default::default(),
        block: 99,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

/// A Mint or Burn in the fire's block before the trigger is applied to the
/// book the fire is quoted on, as the live book applies it (R28.1); one after
/// the trigger is not. The evaluator refused every fire with one, two of the
/// seventeen since the lean deploy.
#[test]
fn a_position_change_before_the_trigger_is_applied_to_the_fire_s_book() {
    use apex_runtime::armed::run::apply_through;
    use apex_runtime::live::book::PoolBook;
    use apex_types::state::ReconstructionStatus;
    let tick = |t: i32| B256::from(word(i128::from(t)));
    let book = PoolBook::from_snapshots([read_pool()], ReconstructionStatus::Verified);
    let mint = log(POOL, vec![MINT, B256::ZERO, tick(-197_400), tick(-197_300)], vec![word(0), word(500), word(1), word(1)], 3, 1);
    let after = log(POOL, vec![BURN, B256::ZERO, tick(-197_400), tick(-197_300)], vec![word(200), word(1), word(1)], 6, 2);
    apply_through(&book, &[POOL], 100, 5, &[mint, after]).unwrap();
    assert_eq!(book.get(POOL).unwrap().state.liquidity, L + 500, "the mint, in range; not the burn after the trigger");
}

/// A position change the book cannot apply exactly would leave it needing a
/// read at the block: no quote, rather than one on the wrong liquidity.
#[test]
fn a_position_change_the_book_cannot_apply_refuses_the_quote() {
    use apex_runtime::armed::run::apply_through;
    use apex_runtime::live::book::PoolBook;
    use apex_types::state::ReconstructionStatus;
    let tick = |t: i32| B256::from(word(i128::from(t)));
    let book = PoolBook::from_snapshots([read_pool()], ReconstructionStatus::Verified);
    let over = log(POOL, vec![BURN, B256::ZERO, tick(-197_400), tick(-197_300)], vec![word(L as i128 + 1), word(1), word(1)], 3, 1);
    let err = apply_through(&book, &[POOL], 100, 5, &[over]).unwrap_err();
    assert!(err.contains("mint or burn"), "{err}");
}

/// The fire's route is the cycle through the arm's two pools in its leg order.
/// Its route hash carries each pool's fee, and a Slipstream fee moves with the
/// tick: reloaded at the fire, the route can hash differently from when it
/// armed, though the plan, which names only pools, is the same.
#[test]
fn a_fire_s_route_is_found_by_its_pools_whatever_its_fee() {
    use apex_runtime::armed::run::cycle_through;
    use apex_runtime::live::frontier;
    use apex_types::ids::ChainId;
    use std::sync::Arc;
    const SLIP: Address = address!("3fe04a59ebd38cf06080a6f60a98d124eb59392a");
    let slip = |fee| {
        let mut p = read_pool();
        p.spec.pool = SLIP;
        p.spec.venue = Venue::SlipstreamV3;
        p.factory = Venue::SlipstreamV3.factory();
        p.state.fee_ppm = fee;
        p
    };
    let book = |fee| [(POOL, Arc::new(read_pool())), (SLIP, Arc::new(slip(fee)))].into_iter().collect::<BTreeMap<_, _>>();
    let (_, armed) = frontier::build(ChainId(8453), frontier::WETH, &book(400));
    let (_, fired) = frontier::build(ChainId(8453), frontier::WETH, &book(1_234));
    let route = armed.values().find(|c| c.legs[0].pool == POOL).unwrap().commitment.route_hash;
    assert!(fired.values().all(|c| c.commitment.route_hash != route), "the fee moved the hash");
    let pools = [format!("{POOL:#x}"), format!("{SLIP:#x}")];
    let c = cycle_through(&fired, &pools).expect("found by its pools");
    assert_eq!((c.legs[0].pool, c.legs[1].pool, c.legs[1].fee_ppm), (POOL, SLIP, 1_234));
    let back = cycle_through(&fired, &[pools[1].clone(), pools[0].clone()]).unwrap();
    assert_eq!((back.legs[0].pool, back.legs[1].pool), (SLIP, POOL), "the other direction is the other cycle");
    assert!(cycle_through(&fired, &[pools[0].clone(), format!("{AERO:#x}")]).is_none());
}

/// R37: a v4 pool's words are PoolManager's, at 256-bit slots; a write there
/// is judged at its whole slot, never its low 64 bits.
#[test]
fn a_v4_write_lands_on_its_256_bit_slot() {
    use apex_runtime::armed::eval::Words;
    use apex_runtime::armed::predicate::{Op, Predicate};
    use apex_runtime::live::v4::{liquidity_slot, state_slot, POOL_MANAGER};
    let id = alloy_primitives::b256!("c19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa");
    let pm = format!("{POOL_MANAGER:#x}");
    let price = U256::from_big_endian(state_slot(id).as_slice());
    let liquidity = U256::from_big_endian(liquidity_slot(id).as_slice());
    let at = |slot: U256, op: Op, v: u64| Predicate::Storage { address: pm.clone(), slot: format!("{slot:#x}"), mask: None, op, value: format!("{v:#x}") };
    let mut w = Words::default();
    w.apply(&Write::Set { address: pm.clone(), slot: price, value: U256::from(5u64) });
    w.apply(&Write::Set { address: pm.clone(), slot: liquidity, value: U256::from(900u64) });
    assert!(w.holds(&[at(price, Op::AtLeast, 5), at(liquidity, Op::AtLeast, 900)], 1));
    assert!(!w.holds(&[at(U256::from(price.low_u64()), Op::AtLeast, 5)], 1), "the low 64 bits are another slot");
    w.apply(&Write::Tick { address: pm.clone(), tick: 0 });
    w.apply(&Write::Liquidity { address: pm.clone(), slot: liquidity, pool: pm.clone(), lower: -10, upper: 10, delta: -100 });
    assert!(w.holds(&[at(liquidity, Op::AtMost, 800)], 1), "in range: the liquidity word moves");
}

// ------------------------------------------------------------------ R37: v4

const AXIS_ID: B256 = alloy_primitives::b256!("c19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa");

fn word_u(v: U256) -> [u8; 32] {
    let mut w = [0u8; 32];
    v.to_big_endian(&mut w);
    w
}

/// PoolManager's Swap for the AXIS pool: the swapper's deltas, then the
/// pool's price, liquidity, tick and fee.
fn v4_swap(id: B256, tick: i32, liquidity: u128, tx_index: u64, log_index: u64) -> RawLog {
    use apex_math::cl_math::get_sqrt_ratio_at_tick;
    use apex_runtime::live::feed::V4_SWAP;
    use apex_runtime::live::v4::POOL_MANAGER;
    let data = vec![word(-1_000), word(250), word_u(get_sqrt_ratio_at_tick(tick).unwrap()), word(liquidity as i128), word(i128::from(tick)), word(100)];
    log(POOL_MANAGER, vec![V4_SWAP, id, B256::repeat_byte(9)], data, tx_index, log_index)
}

fn v4_modify(id: B256, lower: i32, upper: i32, delta: i128, tx_index: u64, log_index: u64) -> RawLog {
    use apex_runtime::live::feed::MODIFY_LIQUIDITY;
    use apex_runtime::live::v4::POOL_MANAGER;
    let data = vec![word(i128::from(lower)), word(i128::from(upper)), word(delta), word(0)];
    log(POOL_MANAGER, vec![MODIFY_LIQUIDITY, id, B256::repeat_byte(9)], data, tx_index, log_index)
}

/// A v4 pool's Swap and ModifyLiquidity are writes on PoolManager's slots; the
/// tick a position's range is held against is the pool's own, by its key
/// address. Another v4 pool's logs add nothing.
#[test]
fn v4_logs_become_writes_on_pool_manager_slots() {
    use apex_math::cl_math::get_sqrt_ratio_at_tick;
    use apex_runtime::live::v4::{key_address, liquidity_slot, state_slot, POOL_MANAGER};
    let key = key_address(AXIS_ID);
    let venues: BTreeMap<Address, Venue> = [(key, Venue::UniswapV4)].into_iter().collect();
    let other = B256::repeat_byte(7);
    let logs = [v4_swap(AXIS_ID, -289_950, 4_000, 2, 1), v4_modify(AXIS_ID, -289_960, -289_940, 500, 2, 2), v4_swap(other, 1, 1, 3, 1)];
    let effects = effects_of(&logs, &venues);
    assert_eq!(effects.len(), 1, "the other pool's swap is not watched");
    let (pm, k) = (format!("{POOL_MANAGER:#x}"), format!("{key:#x}"));
    let slot = |b: B256| U256::from_big_endian(b.as_slice());
    assert_eq!(effects[0].writes, vec![
        Write::Set { address: pm.clone(), slot: slot(state_slot(AXIS_ID)), value: get_sqrt_ratio_at_tick(-289_950).unwrap() },
        Write::Set { address: pm.clone(), slot: slot(liquidity_slot(AXIS_ID)), value: U256::from(4_000u64) },
        Write::Tick { address: k.clone(), tick: -289_950 },
        Write::Liquidity { address: pm, slot: slot(liquidity_slot(AXIS_ID)), pool: k, lower: -289_960, upper: -289_940, delta: 500 },
    ]);
}

/// The AXIS v4 pool, read at block 99.
fn v4_read_pool() -> apex_runtime::live::book::PoolSnapshot {
    use apex_runtime::live::v4::{key_address, V4Key, POOL_MANAGER};
    let mut p = read_pool();
    let (tick, lower, upper) = (-289_960, -291_960, -287_960);
    p.spec.pool = key_address(AXIS_ID);
    p.spec.venue = Venue::UniswapV4;
    p.spec.token0 = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
    p.spec.v4 = Some(V4Key { pool_id: AXIS_ID, fee: 100, tick_spacing: 1, hooks: Address::ZERO });
    p.state.sqrt_price_x96 = apex_math::cl_math::get_sqrt_ratio_at_tick(tick).unwrap();
    p.state.tick = tick;
    p.state.tick_spacing = 1;
    p.state.balance0 = Some(U256::exp10(30));
    p.ladder = apex_math::cl_swap::TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000);
    p.factory = POOL_MANAGER;
    p
}

/// The fire's book takes the block's v4 swaps and positions through the
/// trigger, from PoolManager's logs, and none after it.
#[test]
fn a_v4_fire_s_book_applies_its_swaps_and_positions() {
    use apex_runtime::armed::run::apply_through;
    use apex_runtime::live::book::PoolBook;
    use apex_runtime::live::v4::key_address;
    use apex_types::state::ReconstructionStatus;
    let key = key_address(AXIS_ID);
    let book = PoolBook::from_snapshots([v4_read_pool()], ReconstructionStatus::Verified);
    let logs = [v4_swap(AXIS_ID, -289_950, L, 2, 1), v4_modify(AXIS_ID, -289_960, -289_940, 500, 3, 2), v4_swap(AXIS_ID, -289_000, L, 6, 3)];
    apply_through(&book, &[key], 100, 5, &logs).unwrap();
    let p = book.get(key).unwrap();
    assert_eq!((p.state.tick, p.state.liquidity), (-289_950, L + 500), "the swap, then the position in range; not the swap after the trigger");
}

/// Before `LeanExecutor2` is deployed, a fire is simulated against its code
/// put at its address: the constructor's work done by overrides (the
/// trader allowed, slot 1's mapping; a recipient, slot 2) and by the
/// executor's own approvals to Morpho Blue, called before ours.
#[test]
fn the_pre_deploy_simulation_overrides_code_and_trader() {
    use apex_runtime::armed::eval::Point;
    use apex_runtime::armed::lines::Arm;
    use apex_runtime::armed::run::PreDeploy;
    let text = r#"{"arm_id":7,"at_ns":0,"armed_block":100,"to_block":110,"route_hash":"0xr","trigger_leg":0,"rung":1,"step":3,
        "pools":["0xaa","0xbb"],"venues":["UniswapV4","UniswapV3"],"predicates":[],"start_words":[],"start_ticks":[],
        "size_wei":"1","mid":"1","output_wei":"2","surplus_wei":"1","cost_wei":"0","gas_expected":1,"gas_limit":900000,
        "base_fee_wei":"5000000","other_wei":"0","deadline":11,"calldata":"0xours","calldata_hash":"0x","sender_slot":0}"#;
    let arm: Arm = serde_json::from_str(text).unwrap();
    let block = serde_json::json!({"timestamp": "0x10", "baseFeePerGas": "0x4c4b40", "transactions": [
        {"transactionIndex": "0x0", "from": "0x01", "to": "0x02", "input": "0x", "value": "0x0", "gas": "0x1"},
    ]});
    let signer: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
    let executor2: Address = address!("00000000000000000000000000000000000e2e02");
    let fire = Point { block: 104, tx_index: 0, tx: "0x".into() };
    let pre = PreDeploy { code: "0x6080604052".into() };
    let p = simulate_request(&arm, &fire, &block, signer, executor2, Some(&pre)).unwrap();
    let state = &p[0]["blockStateCalls"][0]["stateOverrides"][format!("{executor2:#x}")];
    assert_eq!(state["code"], "0x6080604052");
    let mut key = [0u8; 64];
    key[12..32].copy_from_slice(signer.as_slice());
    key[63] = 1;
    let allowed = format!("{:#x}", alloy_primitives::keccak256(key));
    assert_eq!(state["stateDiff"][allowed], format!("0x{:064x}", 1), "executors[trader] = true");
    assert_eq!(state["stateDiff"][format!("0x{:064x}", 2)], format!("0x{:0>64}", alloy_primitives::hex::encode(signer)), "a recipient");
    let calls = p[0]["blockStateCalls"][0]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 4, "the block so far, two approvals, ours");
    let morpho_max = format!("0x095ea7b3{:0>64}{}", "bbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb", "f".repeat(64));
    for (c, token) in calls[1..3].iter().zip(["0x4200000000000000000000000000000000000006", "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"]) {
        assert_eq!((c["from"].as_str(), c["to"].as_str(), c["data"].as_str()), (Some(format!("{executor2:#x}").as_str()), Some(token), Some(morpho_max.as_str())));
    }
    assert_eq!((calls[3]["data"].as_str(), calls[3]["to"].as_str()), (Some("0xours"), Some(format!("{executor2:#x}").as_str())));
    let deployed = simulate_request(&arm, &fire, &block, signer, executor2, None).unwrap();
    assert!(deployed[0]["blockStateCalls"][0].get("stateOverrides").is_none(), "a deployed executor runs its own code");
    assert_eq!(deployed[0]["blockStateCalls"][0]["calls"].as_array().unwrap().len(), 2);
}

/// A USDC fire's gross is USDC units: worth as many micro-dollars, and in
/// ETH by the run's WETH price. A WETH fire's is wei.
#[test]
fn a_usdc_fire_s_gross_is_reported_in_usdc_and_usd() {
    use apex_runtime::armed::run::fire_gross;
    let usdc = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    let g = fire_gross(Some(usdc), U256::from(2_500_000u64), 2_500.0);
    assert_eq!((g.start_units, g.wei, g.usd), (Some(U256::from(2_500_000u64)), Some(U256::exp10(15)), Some(2.5)));
    let w = fire_gross(None, U256::exp10(15), 2_500.0);
    assert_eq!((w.start_units, w.wei, w.usd), (None, Some(U256::exp10(15)), Some(2.5)));
    assert_eq!(fire_gross(Some(usdc), U256::from(1u64), 0.0).wei, None, "no WETH price: no ETH value");
}

/// A recorder of `eth_getLogs` filters, answering none.
#[derive(Default)]
struct Filters(std::sync::Mutex<Vec<serde_json::Value>>);

#[async_trait::async_trait]
impl apex_chain::rpc::RpcTransport for Filters {
    async fn call(&self, _method: &str, params: serde_json::Value) -> Result<serde_json::Value, apex_chain::rpc::RpcError> {
        self.0.lock().unwrap().push(params[0].clone());
        Ok(serde_json::json!([]))
    }
}

/// v4 pools' logs are PoolManager's, filtered by pool ID in topic 1.
#[tokio::test]
async fn get_logs_reads_pool_manager_by_v4_pool_id() {
    use apex_runtime::live::feed::{MODIFY_LIQUIDITY, V4_SWAP};
    use apex_runtime::live::v4::POOL_MANAGER;
    let pools: std::collections::BTreeSet<Address> = [POOL].into_iter().collect();
    let rpc = Filters::default();
    get_logs(&rpc, &pools, &[AXIS_ID], 1_000, 1_001).await.unwrap();
    let asked = rpc.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "the pools', then PoolManager's");
    assert_eq!(asked[1]["address"], serde_json::json!([format!("{POOL_MANAGER:#x}")]));
    assert_eq!(asked[1]["topics"], serde_json::json!([[format!("{V4_SWAP:#x}"), format!("{MODIFY_LIQUIDITY:#x}")], [format!("{AXIS_ID:#x}")]]));
    let none = Filters::default();
    get_logs(&none, &pools, &[], 1_000, 1_001).await.unwrap();
    assert_eq!(none.0.lock().unwrap().len(), 1, "no v4 pool, no PoolManager query");
}

/// R37: a USDC arm's surplus is USDC units; the evaluator sweeps its bids
/// per gas on that surplus in wei, at the run's WETH price, as the engine bid
/// on it converted.
#[test]
fn a_usdc_fire_s_bids_are_swept_on_its_surplus_in_wei() {
    use apex_runtime::armed::lines::Arm;
    use apex_runtime::armed::run::surplus_in_wei;
    let text = r#"{"arm_id":7,"at_ns":0,"armed_block":100,"to_block":110,"route_hash":"0xr","trigger_leg":0,"rung":1,"step":3,
        "pools":["0xaa","0xbb"],"venues":["UniswapV4","UniswapV3"],"predicates":[],"start_words":[],"start_ticks":[],
        "size_wei":"1","mid":"1","output_wei":"2","surplus_wei":"2500000","cost_wei":"0","gas_expected":1,"gas_limit":900000,
        "base_fee_wei":"5000000","other_wei":"0","deadline":11,"calldata":"0x","calldata_hash":"0x","sender_slot":0}"#;
    let weth: Arm = serde_json::from_str(text).unwrap();
    assert_eq!(surplus_in_wei(&weth, 2_500.0), U256::from(2_500_000u64), "a WETH arm's surplus is wei");
    let usdc = Arm { start: Some(address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")), ..weth };
    assert_eq!(surplus_in_wei(&usdc, 2_500.0), U256::exp10(15), "$2.50 at $2,500 an ETH");
    assert_eq!(surplus_in_wei(&usdc, 0.0), U256::zero(), "no price: no bid");
}
