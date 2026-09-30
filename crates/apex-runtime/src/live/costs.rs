//! What a trade costs on Base, read from the chain and kept current (Task 8.5
//! R9).
//!
//! # Refreshed, not booted with
//!
//! `LiveEconomics` prices a route at a gas price and at the L1 fee oracle's
//! parameters. The base fee moves every block and the oracle with every L1
//! block, so a 14-day run priced at its boot's figures would misprice every
//! trade after the first hour. The head task reads them and replaces the costs
//! the economics and the pricer hold — **one figure for both**, because a
//! search that sized against one cost while the economics charged another would
//! propose routes the economics refuses (`LiveEconomics::route_cost_wei`).
//!
//! The gas price is the head's base fee: the plane bids no priority fee
//! (`plane::fee_caps`), so the base fee is what a transaction pays per gas. The
//! L1 parameters come from `GasPriceOracle`, the predeploy the chain computes
//! the fee with — and only while it says Fjord, the formula `apex-econ` holds.

use crate::econ::{ChainCosts, LiveEconomics};
use crate::live::abi::{self, selector};
use crate::live::pricing::LivePricer;
use crate::live::reads::{ChainReads, ReadError};
use alloy_primitives::{address, Address, U256};
use apex_econ::cost::l1_data::L1FeeParameters;
use ethers_core::types::U256 as EthersU256;
use std::sync::Arc;

/// Base's `GasPriceOracle` predeploy.
pub const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");

/// Every route the live frontier holds has two hops.
const HOPS: usize = 2;

/// The oracle's parameters at `block`: one `aggregate3`, refused whole if the
/// oracle is not on Fjord or any answer does not decode.
pub async fn read_l1(reads: &ChainReads, block: u64) -> Result<L1FeeParameters, ReadError> {
    let calls = [
        selector::L1_BASE_FEE,
        selector::BLOB_BASE_FEE,
        selector::BASE_FEE_SCALAR,
        selector::BLOB_BASE_FEE_SCALAR,
        selector::IS_FJORD,
    ]
    .map(|s| (GAS_PRICE_ORACLE, abi::call0(s)));
    let answers = reads.multicall(&calls, block).await?;
    let word = |i: usize, bits: u32, what: &str| {
        answers
            .get(i)
            .cloned()
            .flatten()
            .and_then(|d| abi::word_uint(&d, 0, bits))
            .ok_or_else(|| ReadError::Malformed(format!("the L1 fee oracle's {what}")))
    };
    if word(4, 1, "isFjord")? != 1 {
        return Err(ReadError::Malformed("the L1 fee oracle is not on Fjord, the formula priced".into()));
    }
    // A `u32` scalar decodes within 32 bits, so the conversion cannot fail.
    let scalar = |i: usize, what: &str| word(i, 32, what).map(|v| u32::try_from(v).unwrap_or(u32::MAX));
    Ok(L1FeeParameters {
        l1_base_fee: EthersU256::from(word(0, 128, "l1BaseFee")?),
        l1_blob_base_fee: EthersU256::from(word(1, 128, "blobBaseFee")?),
        base_fee_scalar: scalar(2, "baseFeeScalar")?,
        blob_base_fee_scalar: scalar(3, "blobBaseFeeScalar")?,
    })
}

/// The economics and the pricer, whose costs move together.
pub struct Costs {
    econ: Arc<LiveEconomics>,
    pricer: Arc<LivePricer>,
}

impl Costs {
    /// Aligns the pricer with the economics at once: whatever it was built
    /// with, it sizes against the economics' cost from here on.
    pub fn new(econ: Arc<LiveEconomics>, pricer: Arc<LivePricer>) -> Self {
        pricer.set_fixed_cost_wei(econ.route_cost_wei(HOPS));
        Self { econ, pricer }
    }

    /// Price every later route at `base_fee_wei` and `l1`. What the chain does
    /// not say — the settlement's gas, its limit, how it fails — is kept.
    pub fn set(&self, base_fee_wei: u128, l1: L1FeeParameters) -> ChainCosts {
        let costs = ChainCosts { gas_price_wei: U256::from(base_fee_wei), l1, ..self.econ.costs() };
        self.econ.set_costs(costs);
        self.pricer.set_fixed_cost_wei(self.econ.route_cost_wei(HOPS));
        costs
    }

    /// What a two-hop route costs now, in wei.
    pub fn route_cost_wei(&self) -> u128 {
        self.econ.route_cost_wei(HOPS)
    }
}
