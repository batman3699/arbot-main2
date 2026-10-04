//! Task 8.5 R7 — Tier 2 simulation (`apex_runtime::live::sim`).
//!
//! The answers are real: `fixtures/simulate_v1_fork.json` holds what
//! `eth_simulateV1` returned on a local anvil fork of Base, with the Phase 5
//! executor deployed by `DeployAndConfigure.s.sol`, for six plans built by
//! `LiveCallBuilder` — one profitable, one expired, one whose first hop's
//! minimum the chain could not meet, one from a caller that is not an executor,
//! one through an adapter the deployment never registered, and one aimed at an
//! address with no code.

mod support;

use alloy_primitives::{address, hex, keccak256, Address, U256};
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, PlanV2};
use apex_exec::sign::SignedPlan;
use apex_runtime::live::sim::{classify, read_simulation, revert, LiveSimulator};
use apex_runtime::plane::{Decline, Simulator};
use apex_types::cost::GasLimit;
use apex_types::ids::ChainId;
use apex_types::sim::{RevertClass, SimulationTier};
use apex_types::time::DurationNanos;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const BASE: ChainId = ChainId(8453);

fn fixture() -> Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/simulate_v1_fork.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    serde_json::from_str(&text).unwrap()
}

fn executor(f: &Value) -> Address {
    f["executor"].as_str().unwrap().parse().unwrap()
}

fn read(case: &str) -> apex_types::sim::SimulationResult {
    let f = fixture();
    read_simulation(BASE, executor(&f), &f["cases"][case]["response"], DurationNanos(1)).expect("an answer")
}

/// **A profitable plan simulates as a success, and leaves the executor flat.**
/// The loan in and out, the first hop's output spent whole by the second, the
/// profit paid to the recipient: every transfer the simulation traced nets to
/// zero on the executor, so there is no delta and no residue. And the call
/// returned the gross profit the book predicted, to the wei.
#[test]
fn a_profitable_plan_simulates_as_a_success_and_leaves_the_executor_flat() {
    let f = fixture();
    let r = read("success");
    assert!(r.success && r.revert.is_none());
    assert!(r.loan_repaid && r.profit_invariant_held);
    assert_eq!(r.tier, SimulationTier::Tier2FullEvm);
    assert_eq!(r.gas_used, 0x720ea);
    assert!(r.balance_deltas.is_empty(), "{:?}", r.balance_deltas);
    assert!(r.token_residues.is_empty());
    assert_eq!(r.result_hash, r.canonical_hash());

    let returned = hex::decode(f["cases"]["success"]["response"][0]["calls"][0]["returnData"].as_str().unwrap()).unwrap();
    let predicted: U256 = f["expected_output_success"].as_str().unwrap().parse::<U256>().unwrap()
        - f["input"].as_str().unwrap().parse::<U256>().unwrap();
    assert_eq!(U256::from_be_slice(&returned), predicted);

    // The state it ran against, and after: the simulated block and its parent.
    let block = &f["cases"]["success"]["response"][0];
    let b256 = |k: &str| block[k].as_str().unwrap().parse::<alloy_primitives::B256>().unwrap();
    assert_eq!(r.state_after.block_hash_if_available, Some(b256("hash")));
    assert_eq!(r.simulated_at_state.parent_block_hash, b256("parentHash"));
    assert_eq!(r.simulated_at_state.confirmed_block_number + 1, r.state_after.confirmed_block_number);
}

/// **Each recorded revert, by its cause.** A failure repays nothing and holds
/// no invariant: the executor's revert unwinds the loan with everything else.
#[test]
fn each_recorded_revert_is_classified_by_its_cause() {
    for (case, class) in [
        ("unauthorized", RevertClass::Unauthorized),
        ("expired", RevertClass::Expired),
        ("min_out", RevertClass::MinOutNotMet),
        ("unregistered_adapter", RevertClass::Unauthorized),
    ] {
        let r = read(case);
        assert!(!r.success, "{case}");
        assert_eq!(r.revert.as_ref().map(|(c, _)| *c), Some(class), "{case}");
        assert!(!r.loan_repaid && !r.profit_invariant_held, "{case}");
        assert!(r.balance_deltas.is_empty(), "{case}");
    }
}

fn blockpi(case: &str) -> Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/simulate_v1_blockpi.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    serde_json::from_str::<Value>(&text).unwrap()[case]["response"].clone()
}

/// **On BlockPI a revert's data is its error's.** BlockPI's `eth_simulateV1`
/// leaves `returnData` empty on a revert and returns the bytes as `error.data`
/// (recorded on Base: a router's unmet minimum). Read from `returnData` alone,
/// as anvil returns it, every revert the shadow run saw was `Unknown`.
#[test]
fn on_blockpi_a_reverts_data_is_its_errors() {
    let answer = blockpi("router_min_out");
    let call = &answer[0]["calls"][0];
    assert_eq!(call["returnData"], json!("0x"), "the recording is BlockPI's shape");
    let data = hex::decode(call["error"]["data"].as_str().unwrap()).unwrap();

    let r = read_simulation(BASE, Address::repeat_byte(1), &answer, DurationNanos(1)).expect("an answer");
    assert!(!r.success);
    assert_eq!(r.revert, Some((RevertClass::MinOutNotMet, data)));
}

/// **PancakeSwap's minimum fails with nothing to classify.** Its SmartRouter's
/// check is a bare `require`: no data in either place (recorded). So is an
/// inner out-of-gas, and so is the executor's own profit floor — an empty
/// revert says none of them, and is filed `Unknown`.
#[test]
fn an_empty_revert_is_unknown_whoever_raised_it() {
    let r = read_simulation(BASE, Address::repeat_byte(1), &blockpi("pancake_min_out"), DurationNanos(1)).expect("an answer");
    assert_eq!(r.revert, Some((RevertClass::Unknown, Vec::new())));
}

/// An error whose data does not decode is a malformed answer, refused whole
/// rather than classified from half of it.
#[test]
fn an_error_whose_data_does_not_decode_is_refused() {
    let mut answer = blockpi("router_min_out");
    answer[0]["calls"][0]["error"]["data"] = json!("0xnot-hex");
    assert!(read_simulation(BASE, Address::repeat_byte(1), &answer, DurationNanos(1)).is_none());
}

/// **The trap.** Before the Phase 5 executor exists at the committed address, a
/// call to it is a call to an account with no code — status 1, nothing returned,
/// recorded. That is not a `startV2` and not a success. Nor is any other return
/// that is not one word — and since it is not a revert either, its bytes are not
/// read as a revert's, however they begin.
#[test]
fn a_success_that_is_not_a_start_v2_is_a_failure() {
    let r = read("no_executor");
    assert!(!r.success);
    assert_eq!(r.revert, Some((RevertClass::Unknown, Vec::new())));
    assert!(!r.loan_repaid);

    let f = fixture();
    let mut answer = f["cases"]["no_executor"]["response"].clone();
    let looks_like = format!("0x{}{}", hex::encode(revert::NOT_EXECUTOR), "00".repeat(60));
    answer[0]["calls"][0]["returnData"] = json!(looks_like);
    let r = read_simulation(BASE, executor(&f), &answer, DurationNanos(1)).unwrap();
    assert_eq!(r.revert.map(|(c, _)| c), Some(RevertClass::Unknown), "a return is not a revert");
}

fn error_string(msg: &str) -> Vec<u8> {
    let mut out = revert::ERROR_STRING.to_vec();
    out.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(msg.len()).to_be_bytes::<32>());
    let mut body = msg.as_bytes().to_vec();
    body.resize(msg.len().div_ceil(32) * 32, 0);
    out.extend(body);
    out
}

/// Every selector is its signature's hash, and every row of the table.
#[test]
fn the_classification_table() {
    for (sel, sig) in [
        (revert::DEBT_NOT_REPAID, "DebtNotRepaid(address,uint256,uint256)"),
        (revert::UNACCOUNTED_RESIDUE, "UnaccountedResidue(address,uint256,uint256)"),
        (revert::PROFIT_TOKEN_NOT_BORROWED, "ProfitTokenNotBorrowed(address)"),
        (revert::ROUTE_EXPIRED, "RouteExpired(uint64,uint256)"),
        (revert::NOT_EXECUTOR, "NotExecutor()"),
        (revert::SELECTOR_NOT_ALLOWED, "SelectorNotAllowed(uint16,bytes4)"),
        (revert::UNKNOWN_ADAPTER, "UnknownAdapter(uint16)"),
        (revert::ERROR_STRING, "Error(string)"),
    ] {
        assert_eq!(sel, keccak256(sig.as_bytes())[..4], "{sig}");
    }
    let with = |sel: [u8; 4]| {
        let mut d = sel.to_vec();
        d.extend([0u8; 96]);
        d
    };
    for (data, class) in [
        (with(revert::DEBT_NOT_REPAID), RevertClass::FlashRepaymentShortfall),
        (with(revert::UNACCOUNTED_RESIDUE), RevertClass::ProfitInvariantViolated),
        (with(revert::PROFIT_TOKEN_NOT_BORROWED), RevertClass::ProfitInvariantViolated),
        (with(revert::ROUTE_EXPIRED), RevertClass::Expired),
        (with(revert::NOT_EXECUTOR), RevertClass::Unauthorized),
        (with(revert::SELECTOR_NOT_ALLOWED), RevertClass::Unauthorized),
        (with(revert::UNKNOWN_ADAPTER), RevertClass::Unauthorized),
        (error_string("Too little received"), RevertClass::MinOutNotMet),
        (error_string("Transaction too old"), RevertClass::Expired),
        (error_string("BAL#528"), RevertClass::InsufficientLiquidity),
        (error_string("STF"), RevertClass::TokenTransferFailed),
        (error_string("TF"), RevertClass::TokenTransferFailed),
        (error_string("something else"), RevertClass::Unknown),
        // Panic(uint256): an arithmetic fault somewhere; nothing more is known.
        (hex::decode("4e487b710000000000000000000000000000000000000000000000000000000000000011").unwrap(), RevertClass::Unknown),
    ] {
        assert_eq!(classify(&data, None), class, "{}", hex::encode(&data[..4]));
    }
    assert_eq!(classify(&[], Some("execution failed: out of gas")), RevertClass::OutOfGas);
    assert_eq!(classify(&[], Some("execution failed")), RevertClass::Unknown, "an empty revert says nothing");
    assert_eq!(classify(&[], None), RevertClass::Unknown);
}

/// The executor's transfers, netted by token: in and out of the executor
/// counted, anyone else's ignored, and a transfer that does not decode fails
/// the reading rather than being left out of it.
#[test]
fn deltas_are_the_executors_net_transfers() {
    let exe = address!("7CDB3F91fA5df7c7580cC9D857DCfAaEB8f7A044");
    let weth = "0x4200000000000000000000000000000000000006";
    let usdc = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    let topic = |a: &str| format!("0x000000000000000000000000{}", a.trim_start_matches("0x"));
    let transfer = |token: &str, from: &str, to: &str, amount: u64| json!({
        "address": token,
        "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", topic(from), topic(to)],
        "data": format!("0x{}", hex::encode(U256::from(amount).to_be_bytes::<32>())),
    });
    let e = "0x7CDB3F91fA5df7c7580cC9D857DCfAaEB8f7A044";
    let (a, b) = ("0x00000000000000000000000000000000000000aa", "0x00000000000000000000000000000000000000bb");
    let answer = |logs: Vec<Value>| json!([{
        "number": "0x10", "hash": format!("0x{}", "11".repeat(32)), "parentHash": format!("0x{}", "22".repeat(32)),
        "calls": [{ "status": "0x1", "returnData": format!("0x{}", "00".repeat(32)), "gasUsed": "0x5208", "logs": logs }],
    }]);
    let r = read_simulation(BASE, exe, &answer(vec![
        transfer(weth, a, e, 100),
        transfer(weth, e, b, 60),
        transfer(usdc, a, e, 7),
        transfer(usdc, a, b, 1_000), // not the executor's
        json!({"address": usdc, "topics": [format!("0x{}", "33".repeat(32))], "data": "0x"}), // not a transfer
        // ERC-721's Transfer shares the topic and indexes the token id: a fourth
        // topic. Not a balance, whatever its data holds.
        json!({"address": weth, "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", topic(a), topic(e), format!("0x{}", "00".repeat(31) + "07")],
               "data": format!("0x{}", hex::encode(U256::from(9_999u64).to_be_bytes::<32>()))}),
        // A Transfer topic with too few parties: skipped, and nothing indexes into it.
        json!({"address": weth, "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", topic(a)], "data": "0x01"}),
    ]), DurationNanos(1)).unwrap();
    let deltas: Vec<(String, i128)> = r.balance_deltas.iter().map(|(t, d)| (format!("{:#x}", t.address), *d)).collect();
    assert_eq!(deltas, vec![(weth.to_string(), 40), (usdc.to_lowercase(), 7)]);
    assert_eq!(r.token_residues.values().copied().collect::<Vec<_>>(), vec![40, 7]);

    let mut bad = transfer(weth, a, e, 1);
    bad["data"] = json!("0xzz");
    assert!(read_simulation(BASE, exe, &answer(vec![bad]), DurationNanos(1)).is_none());

    // A reverted call moved nothing, whatever a node lists beside it.
    let mut reverted = answer(vec![transfer(weth, a, e, 100)]);
    reverted[0]["calls"][0]["status"] = json!("0x0");
    let r = read_simulation(BASE, exe, &reverted, DurationNanos(1)).unwrap();
    assert!(r.balance_deltas.is_empty() && r.token_residues.is_empty());
}

/// Records what it was asked and answers from a script.
struct Recording {
    answer: Result<Value, RpcError>,
    asked: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl RpcTransport for Recording {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.asked.lock().unwrap().push((method.to_string(), params));
        self.answer.clone()
    }
}

fn call_to(to: Address) -> ExecutorCall {
    let plan = PlanV2 {
        loans: vec![],
        cycle_slippage_bps: 0,
        steps: vec![],
        min_profit: U256::from(1u64),
        declared_residue: U256::ZERO,
        chain_id: 8453,
        deadline: 1,
    };
    let declared = plan_commitment(&plan, 8453, to);
    ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, to).unwrap())
}

/// **One block, one call, as the lane that will sign it**, to the committed
/// executor, with the gas limit it will be signed with, transfers traced and
/// validation off — and what comes back is read, not trusted.
#[tokio::test]
async fn the_simulator_asks_one_call_as_the_lane_with_the_gas_limit() {
    let f = fixture();
    let exe = executor(&f);
    let rpc = Arc::new(Recording { answer: Ok(f["cases"]["success"]["response"].clone()), asked: Mutex::new(vec![]) });
    let sim = LiveSimulator::new(rpc.clone(), BASE);
    let lane = address!("70997970C51812dC3A010C7d01b50e0d17dc79C8");
    let c = support::candidate(1, 100, 1);
    let call = call_to(exe);
    // Not the candidate's: the limit the plane chose, which the signer gets.
    let signed = GasLimit(c.total_execution_cost.gas_limit.0 + 123_457);
    let r = sim.simulate(&c, &call, lane, signed).await.expect("simulated");
    assert!(r.success);

    let asked = rpc.asked.lock().unwrap();
    let [(method, params)] = &asked[..] else { panic!("{asked:?}") };
    assert_eq!(method, "eth_simulateV1");
    let one = &params[0]["blockStateCalls"][0]["calls"][0];
    assert_eq!(one["from"].as_str().unwrap().parse::<Address>().unwrap(), lane);
    assert_eq!(one["to"].as_str().unwrap().parse::<Address>().unwrap(), exe);
    assert_eq!(one["data"], json!(format!("0x{}", hex::encode(call.data()))));
    assert_eq!(one["gas"], json!(format!("{:#x}", signed.0)));
    assert_eq!((params[0]["traceTransfers"].clone(), params[0]["validation"].clone(), params[1].clone()), (json!(true), json!(false), json!("latest")));
}

/// A simulation that could not run has no outcome to classify: a failure
/// without a class, never a success and never a guess.
#[tokio::test]
async fn a_simulation_that_cannot_run_fails_without_a_class() {
    let c = support::candidate(1, 100, 1);
    for answer in [
        Err(RpcError::Exhausted { method: "eth_simulateV1".into(), endpoints: 1, last: "timed out".into() }),
        Ok(json!({"unexpected": true})),
        Ok(json!([{ "number": "0x1", "hash": format!("0x{}", "11".repeat(32)), "parentHash": format!("0x{}", "22".repeat(32)), "calls": [] }])),
    ] {
        let sim = LiveSimulator::new(Arc::new(Recording { answer, asked: Mutex::new(vec![]) }), BASE);
        let err = sim
            .simulate(&c, &call_to(Address::repeat_byte(1)), Address::repeat_byte(2), c.total_execution_cost.gas_limit)
            .await
            .unwrap_err();
        assert_eq!(err, Decline::SimulationFailed { class: None });
    }
}
