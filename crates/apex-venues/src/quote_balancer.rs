use anyhow::{anyhow, Result};
use ethers::{prelude::*, providers::JsonRpcClient};
use std::{collections::HashMap, convert::TryInto, sync::Arc};
use tokio::sync::Mutex;

abigen!(
    IBalancerVault,
    r#"[
  {
    "type": "function",
    "stateMutability": "view",
    "inputs": [ { "internalType": "bytes32", "name": "poolId", "type": "bytes32" } ],
    "name": "getPoolTokens",
    "outputs": [
      { "internalType": "address[]", "name": "tokens", "type": "address[]" },
      { "internalType": "uint256[]", "name": "balances", "type": "uint256[]" },
      { "internalType": "uint256", "name": "lastChangeBlock", "type": "uint256" }
    ]
  },
  {
    "type": "function",
    "stateMutability": "view",
    "inputs": [
      { "internalType": "uint8", "name": "kind", "type": "uint8" },
      {
        "components": [
          { "internalType": "bytes32", "name": "poolId", "type": "bytes32" },
          { "internalType": "uint256", "name": "assetInIndex", "type": "uint256" },
          { "internalType": "uint256", "name": "assetOutIndex", "type": "uint256" },
          { "internalType": "uint256", "name": "amount", "type": "uint256" },
          { "internalType": "bytes", "name": "userData", "type": "bytes" }
        ],
        "internalType": "struct IVault.BatchSwapStep[]",
        "name": "swaps",
        "type": "tuple[]"
      },
      { "internalType": "address[]", "name": "assets", "type": "address[]" },
      {
        "components": [
          { "internalType": "address", "name": "sender", "type": "address" },
          { "internalType": "bool", "name": "fromInternalBalance", "type": "bool" },
          { "internalType": "address", "name": "recipient", "type": "address" },
          { "internalType": "bool", "name": "toInternalBalance", "type": "bool" }
        ],
        "internalType": "struct IVault.FundManagement",
        "name": "funds",
        "type": "tuple"
      }
    ],
    "name": "queryBatchSwap",
    "outputs": [ { "internalType": "int256[]", "name": "assetDeltas", "type": "int256[]" } ]
  }
]"#,
);

pub use i_balancer_vault::{BatchSwapStep, FundManagement};

use apex_math::quote_common::is_block_out_of_range_error;

pub struct BalQuote<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub vault: IBalancerVault<Provider<C>>,
    cache: Mutex<HashMap<BalQuoteKey, U256>>,
    pool_tokens: Mutex<HashMap<H256, Vec<Address>>>,
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct BalQuoteKey {
    pool: H256,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block: U64,
}

impl<C> BalQuote<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(pv: Arc<Provider<C>>, addr: Address) -> Self {
        Self {
            vault: IBalancerVault::new(addr, pv),
            cache: Mutex::new(HashMap::new()),
            pool_tokens: Mutex::new(HashMap::new()),
        }
    }

    pub async fn quote_single_given_in(
        &self,
        pool: H256,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        block: U64,
    ) -> Result<U256> {
        let cache_key = BalQuoteKey {
            pool,
            token_in,
            token_out,
            amount_in,
            block,
        };

        {
            let cache = self.cache.lock().await;
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(*cached);
            }
        }

        let (assets, asset_in_index, asset_out_index) = self
            .pool_assets_and_indices(pool, token_in, token_out)
            .await?;
        let steps = vec![BatchSwapStep {
            pool_id: pool.into(),
            asset_in_index,
            asset_out_index,
            amount: amount_in,
            user_data: Bytes::default(),
        }];
        let funds = FundManagement {
            sender: Address::zero(),
            from_internal_balance: false,
            recipient: Address::zero(),
            to_internal_balance: false,
        };

        let block_id = if block.is_zero() {
            BlockId::Number(BlockNumber::Latest)
        } else {
            BlockId::Number(BlockNumber::Number(block))
        };
        let deltas = match self
            .vault
            .query_batch_swap(0u8, steps.clone(), assets.clone(), funds.clone())
            .block(block_id)
            .call()
            .await
        {
            Ok(value) => value,
            Err(err) if !block.is_zero() && is_block_out_of_range_error(&err) => {
                self.vault
                    .query_batch_swap(0u8, steps, assets, funds)
                    .call()
                    .await?
            }
            Err(err) => return Err(err.into()),
        };

        let out_index: usize = asset_out_index
            .try_into()
            .map_err(|_| anyhow!("asset_out_index does not fit into usize"))?;

        let received = deltas
            .get(out_index)
            .ok_or_else(|| anyhow!("missing delta for asset_out_index"))?
            .abs();
        let received: U256 = received
            .try_into()
            .map_err(|_| anyhow!("delta does not fit into U256"))?;

        let mut cache = self.cache.lock().await;
        cache.insert(cache_key, received);

        Ok(received)
    }
}

impl<C> BalQuote<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    async fn pool_assets_and_indices(
        &self,
        pool: H256,
        token_in: Address,
        token_out: Address,
    ) -> Result<(Vec<Address>, U256, U256)> {
        let assets = self.pool_assets(pool).await?;
        let find_index = |token: Address| {
            assets
                .iter()
                .position(|t| *t == token)
                .map(U256::from)
                .ok_or_else(|| anyhow!("token not found in pool asset list"))
        };

        let asset_in_index = find_index(token_in)?;
        let asset_out_index = find_index(token_out)?;

        Ok((assets, asset_in_index, asset_out_index))
    }

    async fn pool_assets(&self, pool: H256) -> Result<Vec<Address>> {
        {
            let cache = self.pool_tokens.lock().await;
            if let Some(tokens) = cache.get(&pool) {
                return Ok(tokens.clone());
            }
        }

        let (tokens, _, _) = self.vault.get_pool_tokens(pool.into()).call().await?;
        if tokens.is_empty() {
            return Err(anyhow!("balancer pool returned no tokens"));
        }

        let mut cache = self.pool_tokens.lock().await;
        cache.insert(pool, tokens.clone());

        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn computes_indices_with_existing_tokens() {
        let addr_a = Address::from_str("0x00000000000000000000000000000000000000aa").unwrap();
        let addr_b = Address::from_str("0x000000000000000000000000000000000000000b").unwrap();
        let assets = [addr_b, addr_a];

        let find_index = |token: Address| {
            assets
                .iter()
                .position(|t| *t == token)
                .map(U256::from)
                .ok_or_else(|| anyhow!("token not found in pool asset list"))
        };

        assert_eq!(find_index(addr_b).unwrap(), U256::zero());
        assert_eq!(find_index(addr_a).unwrap(), U256::from(1u64));
    }

    #[test]
    fn detects_block_out_of_range_messages() {
        let err = anyhow::anyhow!("(code: -32602, message: BlockOutOfRangeError: block height is 19665991 but requested was 19665990)");
        assert!(is_block_out_of_range_error(&err));
    }

    #[test]
    fn ignores_non_block_range_messages() {
        let err = anyhow::anyhow!("BAL#507");
        assert!(!is_block_out_of_range_error(&err));
    }
}
