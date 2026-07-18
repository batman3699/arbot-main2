//! Helpers shared across the per-venue quote modules.

use crate::math::mul_div;
use ethers::types::U256;

/// Apply a bps swap fee to an input amount: `amount_in * (10_000 - fee_bps) / 10_000`.
///
/// Returns `Err` when `fee_bps >= 10_000` (no output is possible). The returned
/// post-fee amount may be zero (dust input); callers treat that as "no quote".
/// Shared by the constant-product venues (UniV2 and the volatile Solidly curve)
/// so the fee arithmetic cannot drift between them.
pub(crate) fn apply_swap_fee(amount_in: U256, fee_bps: u32) -> anyhow::Result<U256> {
    let fee_den = U256::from(10_000u64);
    let fee_num = U256::from(10_000u64.saturating_sub(fee_bps as u64));
    if fee_num.is_zero() {
        return Err(anyhow::anyhow!("fee basis points must be less than 10_000"));
    }
    Ok(amount_in * fee_num / fee_den)
}

/// Constant-product (`x*y=k`) exact-input output for a post-fee input amount:
/// `(amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee)`.
/// Returns `None` when the result degenerates to zero (dust input, empty
/// denominator, or zero output).
pub(crate) fn constant_product_out(
    amount_in_with_fee: U256,
    reserve_in: U256,
    reserve_out: U256,
) -> Option<U256> {
    if amount_in_with_fee.is_zero() {
        return None;
    }
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in + amount_in_with_fee;
    if denominator.is_zero() {
        return None;
    }
    let amount_out = numerator / denominator;
    if amount_out.is_zero() {
        None
    } else {
        Some(amount_out)
    }
}

/// Constant-product price-impact proxy in bps:
/// `amount_in / (reserve_in + amount_in) * 10_000`, saturating to `u32::MAX`.
/// Uses the pre-fee `amount_in`, matching the on-chain-agnostic slippage proxy.
pub(crate) fn constant_product_price_impact_bps(amount_in: U256, reserve_in: U256) -> u32 {
    let denom = reserve_in.saturating_add(amount_in);
    let bps = if denom.is_zero() {
        U256::zero()
    } else {
        mul_div(amount_in, U256::from(10_000u64), denom)
    };
    u32::try_from(bps.as_u64()).unwrap_or(u32::MAX)
}

/// Classifies RPC errors caused by quoting against a block the endpoint has
/// not indexed yet (or has already pruned). Callers retry these against the
/// latest block instead of treating the venue edge as hard-failed.
///
/// Single source of truth: every quote module must use this classifier so a
/// new node error string only ever needs to be added in one place.
pub fn is_block_out_of_range_error(err: &impl std::fmt::Display) -> bool {
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    lower.contains("blockoutofrangeerror")
        || lower.contains("block out of range")
        || lower.contains("header not found")
        || lower.contains("requested was")
}

#[cfg(test)]
mod tests {
    use super::is_block_out_of_range_error;

    #[test]
    fn detects_all_block_out_of_range_variants() {
        for message in [
            "BlockOutOfRangeError: block height is 123",
            "block out of range",
            "header not found",
            "requested was 123, latest is 120",
        ] {
            assert!(
                is_block_out_of_range_error(&message),
                "must classify as block-lag error: {message}",
            );
        }
    }

    #[test]
    fn ignores_unrelated_errors() {
        for message in ["execution reverted", "insufficient funds", "timeout"] {
            assert!(
                !is_block_out_of_range_error(&message),
                "must not classify as block-lag error: {message}",
            );
        }
    }
}
