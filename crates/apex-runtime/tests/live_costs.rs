//! Task 8.5 R9 — costs read from the chain and kept current
//! (`apex_runtime::live::costs`).
//!
//! The oracle is answered by a scripted node with the values Base's
//! `GasPriceOracle` returned on 2026-10-01; the economics and the pricer are
//! the real ones, so "one figure for both" is checked on the types that hold it.

use alloy_primitives::{hex, Address, U256};
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_econ::cost::failure::FailureProfile;
use apex_econ::cost::l1_data::{L1FeeModel, L1FeeParameters};
use apex_runtime::econ::{ChainCosts, LiveEconomics, ScenarioPriors};
use apex_runtime::live::abi::{selector, MULTICALL3};
use apex_runtime::live::book::PoolBook;
use apex_runtime::live::costs::{self, Costs, GAS_PRICE_ORACLE};
use apex_runtime::live::pricing::LivePricer;
use apex_runtime::live::reads::{ChainReads, ReadError};
use apex_types::cost::{GasLimit, GasUsed};
use apex_types::ids::StrategyId;
use apex_types::state::ReconstructionStatus;
use ethers_core::types::U256 as EthersU256;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Base's oracle, read 2026-10-01: `l1BaseFee`, `blobBaseFee`, `baseFeeScalar`,
/// `blobBaseFeeScalar`, `isFjord`.
const BASE_ANSWERS: [u128; 5] = [82_228_355, 4_300_268, 2_269, 1_055_762, 1];

fn word(v: u128) -> Vec<u8> {
    U256::from(v).to_be_bytes::<32>().to_vec()
}

/// `aggregate3`'s `(bool success, bytes returnData)[]`, one word of data each.
fn encode_results(results: &[Option<Vec<u8>>]) -> Vec<u8> {
    let w = |v: usize| word(v as u128);
    let mut out = [w(32), w(results.len())].concat();
    for i in 0..results.len() {
        out.extend(w(results.len() * 32 + i * 128));
    }
    for r in results {
        out.extend(w(usize::from(r.is_some())));
        out.extend(w(64));
        let d = r.clone().unwrap_or_default();
        out.extend(w(d.len()));
        out.extend(d);
    }
    out
}

/// Answers one `aggregate3` of the oracle's five reads, and checks it was asked
/// for exactly those, of the oracle, in order.
struct Oracle(Mutex<Vec<Option<Vec<u8>>>>);

#[async_trait::async_trait]
impl RpcTransport for Oracle {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        assert_eq!(method, "eth_call");
        assert_eq!(params[0]["to"].as_str().unwrap().parse::<Address>().unwrap(), MULTICALL3);
        let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
        let hex_data = hex::encode(&data);
        let oracle = hex::encode(GAS_PRICE_ORACLE.as_slice());
        for s in [
            selector::L1_BASE_FEE,
            selector::BLOB_BASE_FEE,
            selector::BASE_FEE_SCALAR,
            selector::BLOB_BASE_FEE_SCALAR,
            selector::IS_FJORD,
        ] {
            assert!(hex_data.contains(&hex::encode(s)), "{s:?} not asked");
        }
        assert_eq!(hex_data.matches(&oracle).count(), 5, "five calls, all to the oracle");
        Ok(json!(format!("0x{}", hex::encode(encode_results(&self.0.lock().unwrap())))))
    }
}

fn reads(answers: [Option<u128>; 5]) -> ChainReads {
    ChainReads::new(Arc::new(Oracle(Mutex::new(answers.iter().map(|a| a.map(word)).collect()))))
}

fn base() -> [Option<u128>; 5] {
    BASE_ANSWERS.map(Some)
}

/// **Read as Base answered it.**
#[tokio::test]
async fn the_oracle_is_read_as_base_answered_it() {
    let got = costs::read_l1(&reads(base()), 100).await.unwrap();
    assert_eq!(
        got,
        L1FeeParameters {
            l1_base_fee: EthersU256::from(82_228_355u64),
            l1_blob_base_fee: EthersU256::from(4_300_268u64),
            base_fee_scalar: 2_269,
            blob_base_fee_scalar: 1_055_762,
        }
    );
}

/// **Only Fjord's formula is priced**, and an oracle that says otherwise is not
/// read around.
#[tokio::test]
async fn an_oracle_not_on_fjord_is_refused() {
    let mut a = base();
    a[4] = Some(0);
    assert!(matches!(costs::read_l1(&reads(a), 100).await, Err(ReadError::Malformed(m)) if m.contains("Fjord")));
}

/// A failed call, or a scalar wider than its `uint32`, is refused whole —
/// never priced at a guess.
#[tokio::test]
async fn a_failed_or_oversized_answer_is_refused() {
    for (i, bad) in [(0, None), (1, None), (2, Some(1u128 << 32)), (3, None), (4, None)] {
        let mut a = base();
        a[i] = bad;
        assert!(costs::read_l1(&reads(a), 100).await.is_err(), "answer {i} = {bad:?}");
    }
}

fn chain_costs() -> ChainCosts {
    ChainCosts {
        gas_price_wei: U256::from(6_000_000u64),
        l1: L1FeeParameters {
            l1_base_fee: EthersU256::from(82_228_355u64),
            l1_blob_base_fee: EthersU256::from(4_300_268u64),
            base_fee_scalar: 2_269,
            blob_base_fee_scalar: 1_055_762,
        },
        l1_model: L1FeeModel::unvalidated(),
        failure: FailureProfile { gas_on_failure: GasUsed(411_945), failure_ppm: 50_000 },
        success_gas: GasUsed(534_100),
        gas_limit: GasLimit(800_000),
    }
}

/// **One figure for both.** The pricer is aligned with the economics on
/// construction, and a refresh moves both; what the chain does not say is kept.
#[test]
fn a_refresh_moves_the_economics_and_the_pricer_together() {
    let book = Arc::new(PoolBook::from_snapshots([], ReconstructionStatus::Verified));
    let pricer = Arc::new(LivePricer::new(book, BTreeMap::new(), 7));
    let econ = Arc::new(LiveEconomics::new(pricer.clone(), ScenarioPriors::default(), chain_costs(), StrategyId(1)));
    let c = Costs::new(Arc::clone(&econ), Arc::clone(&pricer));
    let booted = c.route_cost_wei();
    assert_eq!(pricer.fixed_cost_wei(), booted);
    assert_eq!(booted, econ.route_cost_wei(2));

    let mut l1 = chain_costs().l1;
    l1.l1_base_fee *= 3;
    let set = c.set(60_000_000, l1);
    assert_eq!(set.gas_price_wei, U256::from(60_000_000u64));
    assert_eq!(set.l1, l1);
    assert_eq!((set.success_gas, set.gas_limit, set.failure), (GasUsed(534_100), GasLimit(800_000), chain_costs().failure));
    assert_eq!(econ.costs().gas_price_wei, set.gas_price_wei);
    assert!(c.route_cost_wei() > booted);
    assert_eq!(pricer.fixed_cost_wei(), c.route_cost_wei());
}
