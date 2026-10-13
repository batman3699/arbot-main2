//! R27's proof, on Base, through the deployed executor: an ERC-3156 loan from
//! `MorphoFlashLender` settles correctly, and a real cycle above Balancer's
//! 19 WETH settles to its exact quote. The lender is the one the shadow's config
//! names under `lenders.morpho` once deployed (its code checked against the
//! build), or a fixed address before; the historical checks inject its code.
//!
//! cargo build of the contract first: `forge build`. Then, in the main tree:
//! APEX_SECRET_BLOCKPI_KEY=… APEX_SECRET_TRADER_KEY=0x…01 \
//!   cargo test -j 3 --offline -p apex-runtime --test morpho_lender_proof -- --ignored --nocapture

use alloy_primitives::{address, keccak256, Address, U256};
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_config::Env;
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_runtime::armed::run::{book_after, get_logs, replay_calls, replay_diverged, replayed_logs};
use apex_runtime::live::calls::encode_step;
use apex_runtime::live::frontier::{self, BALANCER_VAULT, WETH};
use apex_runtime::live::inventory::Venue;
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::live::reads::ChainReads;
use apex_runtime::shadow::config::ShadowConfig;
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::ids::ChainId;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const TRADER: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
/// Where the lender's code is injected when it is not yet deployed.
const INJECTED: Address = address!("000000000000000000000000000000000000beef");
/// The 21:00 UTC spike on 2026-10-12: ticket 51's trigger, route WETH -> USDC
/// on Uniswap's 0.3% pool -> WETH on its 0.05% pool (expected $32 at 17.88 WETH).
const BLOCK: u64 = 52_526_060;
const TRIGGER: &str = "0xd67a03d51f84c001b5e7b71f2c4b102c20d165a5991a1e8ed4b285f8118be84e";
const ROUTE: &str = "0xb257fc492c3711d488ec5032d116214b87590c08fa8f08ac769d32380694e2dc";
const POOLS: [Address; 2] =
    [address!("6c561b446416e1a00e8e93e221854d6ea4171372"), address!("d0b53d9277642d899df5c87a3966a349a798f224")];

fn hexq(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn weth_slot(holder: Address) -> String {
    let mut k = [0u8; 64];
    k[12..32].copy_from_slice(holder.as_slice());
    k[63] = 3;
    format!("{:#x}", keccak256(k))
}

/// WETH9's `allowance[owner][spender]`, at slot 4.
fn weth_allowance_slot(owner: Address, spender: Address) -> String {
    let mut inner = [0u8; 64];
    inner[12..32].copy_from_slice(owner.as_slice());
    inner[63] = 4;
    let mut outer = [0u8; 64];
    outer[12..32].copy_from_slice(spender.as_slice());
    outer[32..].copy_from_slice(keccak256(inner).as_slice());
    format!("{:#x}", keccak256(outer))
}

fn balance_call(holder: Address) -> Value {
    let mut data = vec![0x70, 0xa0, 0x82, 0x31];
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(holder.as_slice());
    json!({"from": format!("{TRADER:#x}"), "to": format!("{WETH:#x}"), "data": format!("0x{}", alloy_primitives::hex::encode(data))})
}

fn call_data(loan: Loan, steps: Vec<apex_exec::commitment::Step>, deadline: u64) -> String {
    let plan = PlanEncoder::build(loan, steps, 0, U256::from(1), 8453, deadline);
    let declared = plan_commitment(&plan, 8453, EXECUTOR);
    let call = ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, EXECUTOR).unwrap());
    format!("0x{}", alloy_primitives::hex::encode(call.data()))
}

fn lender_code() -> String {
    let path = format!("{}/../../out/MorphoFlashLender.sol/MorphoFlashLender.json", env!("CARGO_MANIFEST_DIR"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("forge build first")).unwrap();
    v["deployedBytecode"]["object"].as_str().unwrap().to_string()
}

/// Simulate `calls` at the state after `parent`, under `overrides`; the results.
async fn simulate(rpc: &dyn RpcTransport, parent: u64, block: &Value, overrides: Value, calls: Vec<Value>) -> Vec<Value> {
    let params = json!([{
        "blockStateCalls": [{
            "blockOverrides": { "number": format!("{:#x}", parent + 1), "time": block["timestamp"], "baseFeePerGas": block["baseFeePerGas"] },
            "stateOverrides": overrides,
            "calls": calls,
        }],
        "validation": false,
    }, format!("{parent:#x}")]);
    rpc.call("eth_simulateV1", params).await.unwrap()[0]["calls"].as_array().unwrap().clone()
}

fn gross_of(result: &Value) -> Option<U256> {
    let topic = format!("{:#x}", keccak256(b"ProfitRealised(address,uint256,uint256)"));
    result["logs"].as_array()?.iter().find(|l| l["topics"][0].as_str() == Some(&topic)).map(|l| {
        U256::from_str_radix(&l["data"].as_str().unwrap()[2..66], 16).unwrap()
    })
}


/// Each Uniswap-style Swap in `result`'s logs: (pool, amount0, amount1).
fn swaps_in(result: &Value) -> Vec<(String, i128, i128)> {
    let topic = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
    let signed = |w: &str| {
        let v = U256::from_str_radix(w, 16).unwrap();
        if v.bit(255) { -((U256::MAX - v + U256::from(1)).to::<u128>() as i128) } else { v.to::<u128>() as i128 }
    };
    result["logs"].as_array().into_iter().flatten()
        .filter(|l| l["topics"][0].as_str() == Some(topic))
        .map(|l| {
            let d = &l["data"].as_str().unwrap()[2..];
            (l["address"].as_str().unwrap().to_lowercase(), signed(&d[0..64]), signed(&d[64..128]))
        })
        .collect()
}

#[tokio::test]
#[ignore = "network: Base through BlockPI; run with --ignored in the main tree"]
async fn a_morpho_loan_settles_through_the_deployed_executor() {
    let path = format!("{}/../../ops/shadow.base.yaml", env!("CARGO_MANIFEST_DIR"));
    let config = ShadowConfig::load(&path, &Env::secrets_only()).unwrap();
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    // A spike block replayed up to its trigger is a heavy simulation: minutes,
    // not BlockPI's usual quarter second.
    let settings = FailoverSettings { request_timeout: std::time::Duration::from_secs(60), ..FailoverSettings::default() };
    let rpc: Arc<dyn RpcTransport> = Arc::new(FailoverTransport::connect(&urls, 8453, settings).await.unwrap());
    let reads = ChainReads::new(Arc::clone(&rpc));
    // The lender the config names once deployed, or a fixed address before.
    // Checks 2 and 3 run at block 52,526,060, before any deployment, so the
    // lender's code is injected there either way: the build's, which must be
    // the deployed code. With it, the standing approval a deployed lender keeps
    // after its first loan: the steady state trades pay.
    let code = lender_code();
    let lender = match &config.lenders.morpho {
        Some(m) => {
            let built = keccak256(alloy_primitives::hex::decode(&code).unwrap());
            assert_eq!(built, m.code_hash, "the build is the deployed lender's code");
            m.address
        }
        None => INJECTED,
    };
    let overrides = json!({
        format!("{lender:#x}"): { "code": code },
        format!("{WETH:#x}"): { "stateDiff": { weth_allowance_slot(lender, MORPHO): format!("{:#066x}", U256::MAX) } },
    });
    let ten = format!("{:#066x}", U256::from(10u64) * U256::from(10u64).pow(U256::from(18)));
    let one = U256::from(10u64).pow(U256::from(18));

    // 1. A 1 WETH loan with no steps, the executor holding 10 WETH: zero profit,
    // so it must revert, as Balancer's does and Aave's does not.
    let latest = rpc.call("eth_getBlockByNumber", json!(["latest", false])).await.unwrap();
    let now = hexq(&latest["timestamp"]);
    let parent = hexq(&latest["number"]) - 1;
    let mut rich = overrides.clone();
    if rich.get(format!("{WETH:#x}")).is_none() {
        rich[format!("{WETH:#x}")] = json!({ "stateDiff": {} });
    }
    rich[format!("{WETH:#x}")]["stateDiff"][weth_slot(EXECUTOR)] = json!(ten);
    let empty = |provider, addr| call_data(Loan { token: WETH, amount: one, provider, provider_addr: addr }, vec![], now + 120);
    let ours = |data: String| json!({"from": format!("{TRADER:#x}"), "to": format!("{EXECUTOR:#x}"), "data": data, "gas": "0x2dc6c0"});
    let r = simulate(&*rpc, parent, &latest, rich.clone(), vec![ours(empty(LoanProvider::Erc3156, lender))]).await;
    assert_eq!(r[0]["status"], "0x0", "an empty Morpho loan must revert for zero profit: {}", r[0]);
    let r = simulate(&*rpc, parent, &latest, rich.clone(), vec![ours(empty(LoanProvider::Balancer, BALANCER_VAULT))]).await;
    assert_eq!(r[0]["status"], "0x0", "the Balancer control reverts too");
    println!("PROOF 1 ok: an empty ERC-3156 loan reverts for zero profit");

    // 2. Ticket 51's cycle after its trigger, sized above Balancer's holding.
    let receipt = rpc.call("eth_getTransactionReceipt", json!([TRIGGER])).await.unwrap();
    let tx_index = hexq(&receipt["transactionIndex"]);
    let block = rpc.call("eth_getBlockByNumber", json!([format!("{BLOCK:#x}"), true])).await.unwrap();
    std::env::set_current_dir(format!("{}/../..", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let venues = [Venue::UniswapV3, Venue::Slipstream, Venue::PancakeV3, Venue::SlipstreamV3, Venue::AerodromeV2];
    let specs = apex_runtime::shadow::universe(&config.inventory, &venues, config.universe.filter()).unwrap();
    let pools: BTreeSet<Address> = POOLS.into_iter().collect();
    // The chain's own logs agree with the replay's on every swap before the
    // trigger or not; the quote is taken on the replay's, which the trade meets.
    let chain_logs = get_logs(&*rpc, &pools, BLOCK, BLOCK).await.unwrap();
    let replay = simulate(&*rpc, BLOCK - 1, &block, overrides.clone(), replay_calls(&block, tx_index).unwrap()).await;
    let logs = replayed_logs(&block, tx_index, &replay, &POOLS);
    let differing = replay_diverged(&chain_logs, &logs, BLOCK, tx_index, &POOLS);
    println!("PROOF 2 replay: {} pool logs, {} the chain's", logs.len(), if differing { "some differing from" } else { "all equal to" });
    let snap = book_after(&reads, &specs, &POOLS, BLOCK, tx_index, &logs).await.unwrap();
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    let cycle = cycles.values().find(|c| format!("{:#x}", c.commitment.route_hash) == ROUTE).expect("ticket 51's route");
    let live = LiveCycle::new(cycle, &snap).unwrap();
    let size = U256::from(20u64) * one;
    let [mid, out] = live.hop_outputs(u256_to_ethers(size)).expect("prices at 20 WETH").map(u256_to_alloy);
    assert!(out > size, "20 WETH pays after the trigger: {out} for {size}");
    let deadline = hexq(&block["timestamp"]) + 120;
    let steps = |s: U256, m: U256| {
        vec![
            encode_step(&cycle.legs[0], &snap[&cycle.legs[0].pool], s, m, EXECUTOR, deadline).unwrap(),
            encode_step(&cycle.legs[1], &snap[&cycle.legs[1].pool], m, s + U256::from(1), EXECUTOR, deadline).unwrap(),
        ]
    };
    let data = call_data(Loan { token: WETH, amount: size, provider: LoanProvider::Erc3156, provider_addr: lender }, steps(size, mid), deadline);
    let mut calls = replay_calls(&block, tx_index).unwrap();
    let before = calls.len();
    calls.push(balance_call(MORPHO));
    calls.push(balance_call(lender));
    calls.push(ours(data));
    calls.push(balance_call(lender));
    calls.push(balance_call(MORPHO));
    let r = simulate(&*rpc, BLOCK - 1, &block, overrides.clone(), calls).await;
    let (morpho_before, lender_before, settled, lender_after, morpho_after) =
        (&r[before], &r[before + 1], &r[before + 2], &r[before + 3], &r[before + 4]);
    assert_eq!(settled["status"], "0x1", "the 20 WETH Morpho-funded cycle settles: {settled}");
    for (pool, a0, a1) in swaps_in(settled) {
        println!("PROOF 2 swap {pool}: amount0 {a0}, amount1 {a1}");
    }
    println!("PROOF 2 quote: hop 1 {mid} (mid), hop 2 {out}; gross on chain {:?}", gross_of(settled));
    assert_eq!(gross_of(settled), Some(out - size), "gross is the exact quote, to the wei");
    // The address may hold dust it was sent (0x…beef does); the loan leaves it as it was.
    assert_eq!(lender_before["returnData"], lender_after["returnData"], "the loan leaves the lender holding what it held");
    assert_eq!(morpho_before["returnData"], morpho_after["returnData"], "Morpho is repaid in full");
    let morpho_gas = hexq(&settled["gasUsed"]);
    println!("PROOF 2 ok: 20 WETH from Morpho, gross {} wei = quote, gas {morpho_gas}", out - size);

    // 3. The same cycle at 15 WETH from each lender: the gas Morpho adds.
    let small = U256::from(15u64) * one;
    let [mid15, out15] = live.hop_outputs(u256_to_ethers(small)).unwrap().map(u256_to_alloy);
    assert!(out15 > small);
    // Twice: with the lender's and the executor's own WETH as it was at that
    // block (none), and with 1 wei in each, as the owner left them on
    // 2026-10-13. A balance written from zero costs more.
    let mut dusted = overrides.clone();
    for holder in [lender, EXECUTOR] {
        dusted[format!("{WETH:#x}")]["stateDiff"][weth_slot(holder)] = json!(format!("{:#066x}", U256::from(1u64)));
    }
    let mut extra = Vec::new();
    for (state, name) in [(&overrides, "no WETH in either"), (&dusted, "1 wei of WETH in each")] {
        let mut gas = Vec::new();
        for (provider, addr) in [(LoanProvider::Balancer, BALANCER_VAULT), (LoanProvider::Erc3156, lender)] {
            let mut calls = replay_calls(&block, tx_index).unwrap();
            calls.push(ours(call_data(Loan { token: WETH, amount: small, provider, provider_addr: addr }, steps(small, mid15), deadline)));
            let r = simulate(&*rpc, BLOCK - 1, &block, state.clone(), calls).await;
            let last = r.last().unwrap();
            assert_eq!(last["status"], "0x1", "{provider:?} at 15 WETH, {name}: {last}");
            assert_eq!(gross_of(last), Some(out15 - small), "{provider:?}, {name}: profit is counted from the starting balance");
            gas.push(hexq(&last["gasUsed"]));
        }
        println!("PROOF 3 ok: 15 WETH, {name}: Balancer gas {}, Morpho gas {}, extra_gas={}", gas[0], gas[1], gas[1] - gas[0]);
        extra.push(gas[1] - gas[0]);
    }
    // The gas model's two figures are these, rounded up to the thousand.
    use apex_runtime::live::lenders::{MORPHO_EXTRA_GAS, MORPHO_EXTRA_GAS_DUSTED};
    for (measured, model) in [(extra[0], MORPHO_EXTRA_GAS), (extra[1], MORPHO_EXTRA_GAS_DUSTED)] {
        assert!(measured <= model && model < measured + 1_000, "measured {measured}, modelled {model}");
    }
}
