//! Helpers shared across the per-venue quote modules.

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
