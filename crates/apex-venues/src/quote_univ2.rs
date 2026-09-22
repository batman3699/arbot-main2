use anyhow::Result;
use ethers::{prelude::*, providers::JsonRpcClient};
use std::sync::Arc;

use apex_math::quote_common::{apply_swap_fee, constant_product_out, constant_product_price_impact_bps};

abigen!(
    IUniswapV2Pair,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
        function getReserves() external view returns (uint112,uint112,uint32)
    ]"#,
);

pub struct UniV2Quote {
    pub amount_out: U256,
    pub price_impact_bps: u32,
}

#[derive(Clone, Debug)]
pub struct UniV2PairState {
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
}

impl UniV2PairState {
    pub fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }
}

/// Selectors for the three constant-product pair reads, derived rather than
/// hardcoded so a typo cannot silently produce a batch of reverting sub-calls.
fn pair_state_selectors() -> [[u8; 4]; 3] {
    let sel = |sig: &str| {
        let h = ethers::utils::keccak256(sig.as_bytes());
        [h[0], h[1], h[2], h[3]]
    };
    [sel("getReserves()"), sel("token0()"), sel("token1()")]
}

/// Load constant-product pair state (UniV2 / Solidly / Aerodrome) for MANY
/// pairs in one Multicall3 round-trip.
///
/// The per-pair [`load_pair_state`] issues three SEQUENTIAL `eth_call`s. With 34
/// Aerodrome pools that is 102 calls per scan, and against a rate-limited
/// provider it does not merely cost latency — it costs COVERAGE. A measured run
/// built 14, 15, 18, 20, 23, 26 … edges from the same 34 configured pools on
/// successive scans, because whichever pools happened to be throttled were
/// silently dropped. A graph missing a random 24-59% of one venue's pools cannot
/// be reasoned about: a real two-hop arb frequently has one leg simply absent.
///
/// This also PINS every read to `block`, which the per-pair path does not do.
/// Previously each pool was read at whatever "latest" meant when its call
/// landed, so reserves within a single graph could straddle several blocks —
/// quoting a cycle across inconsistent state.
///
/// Pairs whose sub-calls revert or return malformed data are simply absent from
/// the result; callers fall back to the per-pair path for those.
pub async fn load_pair_states_batched<C>(
    provider: Arc<Provider<C>>,
    pairs: &[Address],
    block: U64,
) -> std::collections::HashMap<Address, UniV2PairState>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    use std::collections::HashMap;
    let mut out: HashMap<Address, UniV2PairState> = HashMap::new();
    if pairs.is_empty() {
        return out;
    }
    let [reserves_sel, token0_sel, token1_sel] = pair_state_selectors();

    // 3 sub-calls per pair; 64 pairs => 192 sub-calls per batch.
    const PAIRS_PER_BATCH: usize = 64;
    for chunk in pairs.chunks(PAIRS_PER_BATCH) {
        let mut calls: Vec<(Address, Vec<u8>)> = Vec::with_capacity(chunk.len() * 3);
        for pair in chunk {
            calls.push((*pair, reserves_sel.to_vec()));
            calls.push((*pair, token0_sel.to_vec()));
            calls.push((*pair, token1_sel.to_vec()));
        }

        let results = match crate::quote_cl::multicall3_aggregate3(&provider, &calls, block).await {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!(
                    target: "venue::univ2",
                    error = %err,
                    pairs = chunk.len(),
                    "batched pair-state read failed; callers fall back per-pair"
                );
                continue;
            }
        };

        for (i, pair) in chunk.iter().enumerate() {
            let base = i * 3;
            // getReserves() -> (uint112 reserve0, uint112 reserve1, uint32 ts)
            let Some(Some(reserves)) = results.get(base) else {
                continue;
            };
            if reserves.len() < 64 {
                continue;
            }
            let reserve0 = U256::from_big_endian(&reserves[..32]);
            let reserve1 = U256::from_big_endian(&reserves[32..64]);
            if reserve0.is_zero() || reserve1.is_zero() {
                continue;
            }

            let Some(token0) = results.get(base + 1).and_then(decode_address) else {
                continue;
            };
            let Some(token1) = results.get(base + 2).and_then(decode_address) else {
                continue;
            };

            out.insert(
                *pair,
                UniV2PairState {
                    token0,
                    token1,
                    reserve0,
                    reserve1,
                },
            );
        }
    }
    out
}

/// Decode a 32-byte word holding a right-aligned `address`.
fn decode_address(ret: &Option<Vec<u8>>) -> Option<Address> {
    let bytes = ret.as_ref()?;
    if bytes.len() < 32 {
        return None;
    }
    let addr = Address::from_slice(&bytes[12..32]);
    if addr.is_zero() {
        return None;
    }
    Some(addr)
}

pub async fn load_pair_state<C>(
    provider: Arc<Provider<C>>,
    pair: Address,
) -> Result<Option<UniV2PairState>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = IUniswapV2Pair::new(pair, provider.clone());
    let (reserve0, reserve1, _) = match contract.get_reserves().call().await {
        Ok(result) => result,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let token0 = match contract.token_0().call().await {
        Ok(token) => token,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let token1 = match contract.token_1().call().await {
        Ok(token) => token,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(Some(UniV2PairState {
        token0,
        token1,
        reserve0: U256::from(reserve0),
        reserve1: U256::from(reserve1),
    }))
}

fn should_skip_pair<M: Middleware>(err: &ContractError<M>) -> bool {
    let msg = err.to_string();
    msg.contains("execution reverted")
        || msg.contains("Invalid name")
        || msg.contains("empty bytes")
}

pub fn quote_exact_input_from_state(
    state: &UniV2PairState,
    token_in: Address,
    amount_in: U256,
    fee_bps: u32,
) -> Result<Option<UniV2Quote>> {
    if amount_in.is_zero() {
        return Ok(None);
    }

    let (reserve_in, reserve_out) = match state.reserves_for(token_in) {
        Some(reserves) => reserves,
        None => return Ok(None),
    };

    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Ok(None);
    }

    let amount_in_with_fee = apply_swap_fee(amount_in, fee_bps)?;
    let amount_out = match constant_product_out(amount_in_with_fee, reserve_in, reserve_out) {
        Some(amount_out) => amount_out,
        None => return Ok(None),
    };
    let price_impact_bps = constant_product_price_impact_bps(amount_in, reserve_in);

    Ok(Some(UniV2Quote {
        amount_out,
        price_impact_bps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_state_selectors_match_signatures() {
        // Derived, not hardcoded: a wrong selector makes every batched sub-call
        // revert and silently degrades the whole venue to the per-pair path.
        let [reserves, token0, token1] = pair_state_selectors();
        assert_eq!(reserves, &ethers::utils::id("getReserves()")[..4]);
        assert_eq!(token0, &ethers::utils::id("token0()")[..4]);
        assert_eq!(token1, &ethers::utils::id("token1()")[..4]);
        // Canonical UniV2 values as an independent second check.
        assert_eq!(reserves, [0x09, 0x02, 0xf1, 0xac]);
        assert_eq!(token0, [0x0d, 0xfe, 0x16, 0x81]);
        assert_eq!(token1, [0xd2, 0x12, 0x20, 0xa7]);
    }

    #[test]
    fn decode_address_reads_right_aligned_word() {
        let mut word = vec![0u8; 32];
        word[12..32].copy_from_slice(&[0x11u8; 20]);
        assert_eq!(decode_address(&Some(word)), Some(Address::from([0x11u8; 20])));
    }

    #[test]
    fn decode_address_rejects_unusable_returndata() {
        // A zero address means the call answered but the pair is not a pair;
        // short/absent data means it did not answer. Neither may become an edge.
        assert_eq!(decode_address(&Some(vec![0u8; 32])), None);
        assert_eq!(decode_address(&Some(vec![0u8; 8])), None);
        assert_eq!(decode_address(&None), None);
    }
}
