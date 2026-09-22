//! Helpers shared across the per-venue quote modules.

use crate::math::mul_div;
use ethers_core::types::U256;

/// Apply a bps swap fee to an input amount: `amount_in * (10_000 - fee_bps) / 10_000`.
///
/// Returns `Err` when `fee_bps >= 10_000` (no output is possible). The returned
/// post-fee amount may be zero (dust input); callers treat that as "no quote".
/// Shared by the constant-product venues (UniV2 and the volatile Solidly curve)
/// so the fee arithmetic cannot drift between them.
pub fn apply_swap_fee(amount_in: U256, fee_bps: u32) -> anyhow::Result<U256> {
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
pub fn constant_product_out(
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
pub fn constant_product_price_impact_bps(amount_in: U256, reserve_in: U256) -> u32 {
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

/// Distinguishes a definitive on-chain verdict (the call reached the EVM and
/// reverted) from a transport failure (rate limit, timeout, dead endpoint).
///
/// Callers use this to decide whether a FAILED quote may be cached as a fact
/// about the chain. A revert means "no pool / no liquidity on this path" and
/// stays true until the pool set changes. A transport failure means "we do not
/// know" — and caching that as a verdict is what turned a provider rate-limit
/// into a multi-month zero-fill outage: every cycle whose start token got
/// poisoned was rejected pre-simulation for the full cache TTL, silently.
///
/// Fail-safe direction is deliberate: anything unrecognised returns `false`
/// (not a verdict), so an unknown error string causes a retry next scan rather
/// than a cached lie. Over-retrying costs RPC budget; over-caching costs fills.
pub fn is_execution_revert(err: &impl std::fmt::Display) -> bool {
    let lower = err.to_string().to_ascii_lowercase();
    // "execution reverted" covers the plain revert and the `: SPL` /
    // `: STF` Uniswap variants. Invalid-opcode is how a QuoterV2 surfaces a
    // missing pool on some nodes.
    lower.contains("execution reverted")
        || lower.contains("invalidfeopcode")
        || lower.contains("invalid opcode")
}

#[cfg(test)]
mod tests {
    use super::{is_block_out_of_range_error, is_execution_revert};

    #[test]
    fn classifies_reverts_as_definitive_verdicts() {
        // Observed against Base QuoterV2 for tokens with no route.
        for message in [
            "execution reverted",
            "execution reverted: SPL",
            "execution reverted: STF",
            "JSON-RPC error: EVM error: InvalidFEOpcode (code -32003)",
        ] {
            assert!(
                is_execution_revert(&message),
                "must classify as definitive revert: {message}",
            );
        }
    }

    #[test]
    fn refuses_to_treat_transport_failures_as_verdicts() {
        // These MUST NOT be cached as "unpriceable" — they say nothing about
        // the chain, only about our connection to it.
        for message in [
            "429 Too Many Requests",
            "Monthly capacity limit exceeded",
            "request timed out",
            "connection reset by peer",
            "all endpoints failed",
            "error sending request for url",
            "some new error string nobody has seen before",
        ] {
            assert!(
                !is_execution_revert(&message),
                "must NOT classify as revert (fail-safe to retry): {message}",
            );
        }
    }

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
