use anyhow::{anyhow, Context, Result};
use ethers::abi::{decode, ParamType, Token};
use ethers::providers::{JsonRpcClient, Middleware};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Address, Bytes, TransactionRequest, U256};
use ethers::utils::keccak256;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::warn;

use crate::ops_inputs::GasModel;

#[derive(Clone, Debug, Default)]
pub struct ArbitrumFeeConfig {
    pub per_byte_wei: Option<U256>,
    pub max_deviation_bps: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct FeeEstimate {
    pub gas_limit: U256,
    pub gas_price: U256,
    pub base_fee_per_gas: Option<U256>,
    pub priority_fee_per_gas: Option<U256>,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee_per_gas: Option<U256>,
    pub l1_data_fee: U256,
    pub total_fee_native: U256,
}

#[derive(Clone)]
pub struct FeeEstimator<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub chain_name: String,
    pub model: GasModel,
    pub provider: ethers::providers::Provider<C>,
    pub arb_config: ArbitrumFeeConfig,
    pub custom_rpc_method: Option<String>,
}

impl<C> FeeEstimator<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(
        chain_name: String,
        model: GasModel,
        provider: ethers::providers::Provider<C>,
        arb_config: ArbitrumFeeConfig,
        custom_rpc_method: Option<String>,
    ) -> Self {
        Self {
            chain_name,
            model,
            provider,
            arb_config,
            custom_rpc_method,
        }
    }

    pub async fn estimate_for_tx(
        &self,
        tx: &TypedTransaction,
        gas_limit_hint: Option<U256>,
        priority_fee: Option<U256>,
    ) -> Result<FeeEstimate> {
        match self.model {
            GasModel::LineaEstimateGas => self.estimate_linea_fee(tx, priority_fee).await,
            GasModel::CustomRpcMethod => {
                self.estimate_custom_fee(tx, gas_limit_hint, priority_fee)
                    .await
            }
            _ => {
                let gas_limit = gas_limit_hint.unwrap_or_else(|| U256::from(210_000u64));
                let base_fee = self
                    .provider
                    .get_block(ethers::types::BlockNumber::Latest)
                    .await
                    .ok()
                    .and_then(|block| block.and_then(|b| b.base_fee_per_gas));
                let mut gas_price = self.provider.get_gas_price().await.unwrap_or_default();
                if let Some(priority_fee) = priority_fee {
                    let forced = base_fee
                        .map(|base| base.saturating_add(priority_fee))
                        .unwrap_or(priority_fee);
                    if forced > gas_price {
                        gas_price = forced;
                    }
                }

                let l1_data_fee = match self.model {
                    GasModel::OpStack => self.estimate_op_stack_l1_fee(tx).await?,
                    GasModel::Arbitrum => self.estimate_arbitrum_l1_fee(tx).await?,
                    _ => U256::zero(),
                };

                let max_fee_per_gas =
                    base_fee.and_then(|base| priority_fee.map(|priority| base + priority));
                let total_fee_native = gas_price
                    .saturating_mul(gas_limit)
                    .saturating_add(l1_data_fee);

                Ok(FeeEstimate {
                    gas_limit,
                    gas_price,
                    base_fee_per_gas: base_fee,
                    priority_fee_per_gas: priority_fee,
                    max_fee_per_gas,
                    max_priority_fee_per_gas: priority_fee,
                    l1_data_fee,
                    total_fee_native,
                })
            }
        }
    }

    async fn estimate_op_stack_l1_fee(&self, tx: &TypedTransaction) -> Result<U256> {
        const GAS_PRICE_ORACLE: &str = "0x420000000000000000000000000000000000000F";
        let oracle = Address::from_str(GAS_PRICE_ORACLE)?;
        let selector = &keccak256("getL1Fee(bytes)")[..4];
        let calldata = tx.data().cloned().unwrap_or_default();
        let encoded_args = ethers::abi::encode(&[Token::Bytes(calldata.to_vec())]);
        let mut data = Vec::with_capacity(selector.len() + encoded_args.len());
        data.extend_from_slice(selector);
        data.extend_from_slice(&encoded_args);
        let req = TransactionRequest::new().to(oracle).data(Bytes::from(data));
        let raw_fee = self.provider.call(&req.into(), None).await?;
        let decoded = decode(&[ParamType::Uint(256)], &raw_fee)?;
        Ok(decoded
            .first()
            .and_then(|token| token.clone().into_uint())
            .unwrap_or_default())
    }

    async fn estimate_arbitrum_l1_fee(&self, tx: &TypedTransaction) -> Result<U256> {
        const ARB_GAS_INFO: &str = "0x000000000000000000000000000000000000006C";
        let gas_info = Address::from_str(ARB_GAS_INFO)?;
        let selector = &keccak256("getPricesInWei()")[..4];
        let req = TransactionRequest::new()
            .to(gas_info)
            .data(Bytes::from(selector.to_vec()));
        let raw = self.provider.call(&req.into(), None).await?;
        let decoded = decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            &raw,
        )?;
        let onchain_per_byte = decoded
            .get(5)
            .and_then(|token| token.clone().into_uint())
            .unwrap_or_default();

        let per_byte_price = if let Some(configured) = self.arb_config.per_byte_wei {
            if let Some(max_dev) = self.arb_config.max_deviation_bps {
                let diff = if onchain_per_byte > configured {
                    onchain_per_byte - configured
                } else {
                    configured - onchain_per_byte
                };
                let dev_bps = if configured.is_zero() {
                    0u32
                } else {
                    diff.saturating_mul(U256::from(10_000u64))
                        .checked_div(configured)
                        .unwrap_or_default()
                        .as_u32()
                };
                if dev_bps > max_dev {
                    warn!(
                        chain = %self.chain_name,
                        configured = %configured,
                        onchain = %onchain_per_byte,
                        deviation_bps = dev_bps,
                        "Arbitrum per-byte fee deviates from configured value"
                    );
                }
            }
            configured
        } else {
            onchain_per_byte
        };

        let calldata = tx.data().cloned().unwrap_or_default();
        Ok(per_byte_price.saturating_mul(U256::from(calldata.len() as u64)))
    }

    async fn estimate_linea_fee(
        &self,
        tx: &TypedTransaction,
        priority_fee: Option<U256>,
    ) -> Result<FeeEstimate> {
        let params = LineaEstimateGasParams::from_tx(tx);
        let resp: LineaEstimateGasResponse = self
            .provider
            .request("linea_estimateGas", vec![params])
            .await
            .context("linea_estimateGas request failed")?;
        let gas_limit = resp.gas_limit;
        let base_fee = resp.base_fee_per_gas;
        let priority_fee = priority_fee.unwrap_or(resp.priority_fee_per_gas);
        let gas_price = base_fee.saturating_add(priority_fee);
        let total_fee_native = gas_price
            .saturating_mul(gas_limit)
            .saturating_add(U256::zero());

        Ok(FeeEstimate {
            gas_limit,
            gas_price,
            base_fee_per_gas: Some(base_fee),
            priority_fee_per_gas: Some(priority_fee),
            max_fee_per_gas: Some(gas_price),
            max_priority_fee_per_gas: Some(priority_fee),
            l1_data_fee: U256::zero(),
            total_fee_native,
        })
    }

    async fn estimate_custom_fee(
        &self,
        tx: &TypedTransaction,
        gas_limit_hint: Option<U256>,
        priority_fee: Option<U256>,
    ) -> Result<FeeEstimate> {
        let method = self
            .custom_rpc_method
            .clone()
            .ok_or_else(|| anyhow!("custom rpc method not configured"))?;
        let params = CustomEstimateParams::from_tx(tx);
        let resp: CustomFeeResponse = self
            .provider
            .request(method.as_str(), vec![params])
            .await
            .context("custom fee rpc request failed")?;
        let gas_limit = resp
            .gas_limit
            .or(gas_limit_hint)
            .ok_or_else(|| anyhow!("custom fee response missing gasLimit"))?;
        let base_fee = resp.base_fee_per_gas;
        let priority_fee = priority_fee
            .or(resp.priority_fee_per_gas)
            .or(resp.gas_price);
        let gas_price = resp
            .gas_price
            .or_else(|| base_fee.and_then(|base| priority_fee.map(|priority| base + priority)))
            .unwrap_or_default();
        let total_fee_native = gas_price
            .saturating_mul(gas_limit)
            .saturating_add(resp.l1_data_fee.unwrap_or_default());

        Ok(FeeEstimate {
            gas_limit,
            gas_price,
            base_fee_per_gas: base_fee,
            priority_fee_per_gas: priority_fee,
            max_fee_per_gas: base_fee.and_then(|base| priority_fee.map(|priority| base + priority)),
            max_priority_fee_per_gas: priority_fee,
            l1_data_fee: resp.l1_data_fee.unwrap_or_default(),
            total_fee_native,
        })
    }
}

fn max_fee_per_gas_from_tx(tx: &TypedTransaction) -> Option<U256> {
    match tx {
        TypedTransaction::Eip1559(inner) => inner.max_fee_per_gas,
        _ => None,
    }
}

fn max_priority_fee_per_gas_from_tx(tx: &TypedTransaction) -> Option<U256> {
    match tx {
        TypedTransaction::Eip1559(inner) => inner.max_priority_fee_per_gas,
        _ => None,
    }
}

#[derive(Debug, Serialize)]
struct LineaEstimateGasParams {
    from: Option<Address>,
    to: Option<Address>,
    gas: Option<U256>,
    gas_price: Option<U256>,
    max_fee_per_gas: Option<U256>,
    max_priority_fee_per_gas: Option<U256>,
    value: Option<U256>,
    data: Option<Bytes>,
}

impl LineaEstimateGasParams {
    fn from_tx(tx: &TypedTransaction) -> Self {
        Self {
            from: tx.from().copied(),
            to: tx.to().and_then(|to| to.as_address().copied()),
            gas: tx.gas().copied(),
            gas_price: tx.gas_price(),
            max_fee_per_gas: max_fee_per_gas_from_tx(tx),
            max_priority_fee_per_gas: max_priority_fee_per_gas_from_tx(tx),
            value: tx.value().copied(),
            data: tx.data().cloned(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LineaEstimateGasResponse {
    gas_limit: U256,
    base_fee_per_gas: U256,
    priority_fee_per_gas: U256,
}

#[derive(Debug, Serialize)]
struct CustomEstimateParams {
    from: Option<Address>,
    to: Option<Address>,
    gas: Option<U256>,
    gas_price: Option<U256>,
    max_fee_per_gas: Option<U256>,
    max_priority_fee_per_gas: Option<U256>,
    value: Option<U256>,
    data: Option<Bytes>,
}

impl CustomEstimateParams {
    fn from_tx(tx: &TypedTransaction) -> Self {
        Self {
            from: tx.from().copied(),
            to: tx.to().and_then(|to| to.as_address().copied()),
            gas: tx.gas().copied(),
            gas_price: tx.gas_price(),
            max_fee_per_gas: max_fee_per_gas_from_tx(tx),
            max_priority_fee_per_gas: max_priority_fee_per_gas_from_tx(tx),
            value: tx.value().copied(),
            data: tx.data().cloned(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CustomFeeResponse {
    gas_limit: Option<U256>,
    base_fee_per_gas: Option<U256>,
    priority_fee_per_gas: Option<U256>,
    gas_price: Option<U256>,
    l1_data_fee: Option<U256>,
}

#[cfg(test)]
mod tests {
    use super::CustomFeeResponse;
    use ethers::types::U256;

    #[test]
    fn parses_custom_fee_response() {
        let raw = r#"{
            "gasLimit": "0x5208",
            "baseFeePerGas": "0x64",
            "priorityFeePerGas": "0x2",
            "gasPrice": "0x69",
            "l1DataFee": "0x384"
        }"#;
        let parsed: CustomFeeResponse = serde_json::from_str(raw).expect("parse response");
        assert_eq!(parsed.gas_limit, Some(U256::from(21000u64)));
        assert_eq!(parsed.base_fee_per_gas, Some(U256::from(100u64)));
        assert_eq!(parsed.priority_fee_per_gas, Some(U256::from(2u64)));
        assert_eq!(parsed.gas_price, Some(U256::from(105u64)));
        assert_eq!(parsed.l1_data_fee, Some(U256::from(900u64)));
    }
}
