use anyhow::{anyhow, Context, Result};
use ethers::abi::{decode, ParamType};
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
        let estimate = match self.model {
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
                // This used to be `unwrap_or_default()`, so a failed
                // eth_gasPrice produced a gas price of ZERO and the candidate
                // was then priced as if execution were free. Fail closed: the
                // caller can skip the block, it cannot un-know a fake number.
                //
                // The message says "rpc" deliberately -- `is_rpc_error` in
                // main.rs matches on that word to trip the circuit breaker and
                // mark the endpoint unhealthy, which is exactly what a gas-price
                // failure is evidence of.
                let mut gas_price = self
                    .provider
                    .get_gas_price()
                    .await
                    .context("eth_gasPrice rpc call failed")?;

                // An endpoint that answers zero has not given us a price
                // either. Substitute base+priority only when BOTH are known:
                // a priority fee alone is the tip on top of a gas price, not a
                // gas price, and using it as the whole cost understates gas by
                // the entire base fee. `base_fee` being absent is tolerated
                // above only because pre-EIP-1559 chains legitimately have
                // none -- there, eth_gasPrice is the answer, so it must work.
                if gas_price.is_zero() {
                    gas_price = match (base_fee, priority_fee) {
                        (Some(base), Some(priority)) => base.saturating_add(priority),
                        _ => U256::zero(),
                    };
                }

                // The tip only ever RAISES a price that already exists. With
                // no base fee the floor is the tip alone, which is a legitimate
                // floor over a real gas price (legacy chains) but is not itself
                // a gas price -- so it must not be allowed to lift a ZERO into
                // something that merely looks priced. That is why the repair
                // above runs first and insists on a base fee, and why this is
                // skipped entirely while gas_price is zero.
                if let Some(priority_fee) = priority_fee.filter(|_| !gas_price.is_zero()) {
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
        }?;

        // One invariant, enforced once for every gas model: a fee estimate
        // never carries a zero gas price.
        //
        // Zero is not a cheap estimate, it is an absent one, and it is the most
        // dangerous value on the profitability path -- `total_fee_native`
        // becomes zero, `dynamic_min_profit` charges nothing for gas, and every
        // candidate clears the profit gate as if it executed for free. The
        // ceiling checks downstream only catch gas that is too HIGH.
        //
        // `estimate_custom_fee` reached zero the same way (`.unwrap_or_default()`
        // when the response carried neither a gas price nor base+priority), and
        // `estimate_linea_fee` can be handed a zero base fee, so the check lives
        // here rather than in one branch.
        if estimate.gas_price.is_zero() {
            return Err(anyhow!(
                "{}: no usable gas price (the rpc endpoint returned zero and \
                 base_fee/priority_fee cannot supply one); refusing to price a \
                 candidate as if gas were free",
                self.chain_name
            ));
        }

        Ok(estimate)
    }

    async fn estimate_op_stack_l1_fee(&self, tx: &TypedTransaction) -> Result<U256> {
        let oracle = Address::from_str(crate::sim_revm::GAS_PRICE_ORACLE)?;
        let calldata = tx.data().cloned().unwrap_or_default();
        let data = crate::sim_revm::encode_get_l1_fee_calldata(calldata.as_ref());
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
    use super::{ArbitrumFeeConfig, CustomFeeResponse, FeeEstimator};
    use crate::ops_inputs::GasModel;
    use ethers::providers::{MockProvider, Provider};
    use ethers::types::transaction::eip2718::TypedTransaction;
    use ethers::types::{TransactionRequest, U256};

    /// `MockProvider` answers requests LIFO, so responses are pushed in the
    /// reverse of the order the code under test asks for them. The Eip1559 arm
    /// of `estimate_for_tx` asks for the latest block first, then eth_gasPrice.
    fn estimator_answering(
        gas_price: Option<&str>,
        base_fee: Option<&str>,
    ) -> FeeEstimator<MockProvider> {
        let (provider, mock) = Provider::mocked();
        if let Some(price) = gas_price {
            mock.push::<U256, _>(U256::from_str_radix(price, 16).expect("gas price"))
                .expect("push gas price");
        }
        let block = match base_fee {
            Some(fee) => serde_json::json!({
                "number": "0x1",
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "baseFeePerGas": format!("0x{fee}"),
                "transactions": [],
                "uncles": [],
            }),
            None => serde_json::Value::Null,
        };
        mock.push::<serde_json::Value, _>(block).expect("push block");
        FeeEstimator::new(
            "testchain".to_string(),
            GasModel::Eip1559,
            provider,
            ArbitrumFeeConfig::default(),
            None,
        )
    }

    fn bare_tx() -> TypedTransaction {
        TransactionRequest::new().into()
    }

    /// Zero is not a cheap gas price, it is an absent one. Pricing a candidate
    /// at zero gas makes every route look profitable, and the ceiling checks
    /// downstream only catch gas that is too high.
    #[tokio::test]
    async fn a_zero_gas_price_is_refused_rather_than_priced_as_free() {
        // eth_gasPrice answers 0, and no priority fee is offered as a floor.
        let estimator = estimator_answering(Some("0"), Some("64"));
        let err = estimator
            .estimate_for_tx(&bare_tx(), Some(U256::from(210_000u64)), None)
            .await
            .expect_err("a zero gas price must not produce a fee estimate");
        let message = format!("{err:#}");
        assert!(
            message.contains("no usable gas price"),
            "unexpected error: {message}"
        );
    }

    /// The failure has to reach `is_rpc_error` in main.rs, which matches on the
    /// word "rpc" to trip the circuit breaker and mark the endpoint unhealthy.
    /// A gas-price call that fails is exactly that evidence.
    #[tokio::test]
    async fn a_failed_gas_price_call_errors_and_reads_as_an_rpc_fault() {
        // Nothing pushed for eth_gasPrice, so the mock errors on that request.
        let estimator = estimator_answering(None, Some("64"));
        let err = estimator
            .estimate_for_tx(&bare_tx(), Some(U256::from(210_000u64)), None)
            .await
            .expect_err("a failed eth_gasPrice must not silently become zero");
        let message = format!("{err:#}").to_ascii_lowercase();
        assert!(
            message.contains("rpc"),
            "error must classify as an RPC fault, got: {message}"
        );
    }

    /// A priority fee alone is the tip on top of a gas price, not a gas price.
    /// Accepting it as the whole cost understates gas by the entire base fee,
    /// which is the larger term on every chain worth trading.
    #[tokio::test]
    async fn a_priority_fee_alone_cannot_stand_in_for_a_missing_gas_price() {
        // eth_gasPrice answers 0 and the block carries no base fee, so the only
        // number available is the caller's 0.01 gwei tip.
        let estimator = estimator_answering(Some("0"), None);
        let err = estimator
            .estimate_for_tx(
                &bare_tx(),
                Some(U256::from(210_000u64)),
                Some(U256::from(10_000_000u64)),
            )
            .await
            .expect_err("priority fee alone is not a gas price");
        assert!(
            format!("{err:#}").contains("no usable gas price"),
            "unexpected error: {err:#}"
        );
    }

    /// The ordinary path still works: a real gas price is returned untouched
    /// and the priority fee only ever raises it.
    #[tokio::test]
    async fn a_real_gas_price_is_returned_and_the_tip_only_raises_it() {
        let estimator = estimator_answering(Some("64"), Some("64"));
        let estimate = estimator
            .estimate_for_tx(&bare_tx(), Some(U256::from(210_000u64)), None)
            .await
            .expect("a valid gas price must produce an estimate");
        assert_eq!(estimate.gas_price, U256::from(100u64));
        assert_eq!(estimate.base_fee_per_gas, Some(U256::from(100u64)));
        assert_eq!(
            estimate.total_fee_native,
            U256::from(100u64) * U256::from(210_000u64)
        );
    }

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
