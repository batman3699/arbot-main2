//! QuoterV2 path encoding for the Uniswap V3-family venues.
//!
//! `token(20) [fee(3) token(20)]*`, packed, exactly as `quoteExactInput`
//! expects it. Moved out of `arb-exec`'s `util` in Phase 2: it is a venue
//! wire format, not a general utility, and it was the last thing the
//! UniV3-family quoters still reached back into the legacy crate for.

use anyhow::{anyhow, Result};
use ethers::types::Address;
use tracing::warn;

pub fn encode_univ3_path(path: &[(Address, Option<u32>)]) -> Result<Vec<u8>> {
    let hops = path.len();
    if hops == 0 {
        return Ok(Vec::new());
    }

    let mut capacity = hops.saturating_mul(20);
    if hops > 1 {
        capacity += (hops - 1) * 3;
    }

    let mut bytes = Vec::with_capacity(capacity);
    for (i, (token, fee)) in path.iter().enumerate() {
        if i > 0 {
            let Some(fee) = fee else {
                warn!("Skipping UniV3 path encoding due to missing fee");
                return Err(anyhow!("missing fee for hop {i}"));
            };
            bytes.extend_from_slice(&fee.to_be_bytes()[1..4]);
        }
        bytes.extend_from_slice(token.as_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn encodes_univ3_path_with_expected_layout() {
        let path = vec![(addr(1), None), (addr(2), Some(500))];
        let encoded = encode_univ3_path(&path).expect("path should encode");

        assert_eq!(encoded.len(), 43);
        assert_eq!(&encoded[..20], addr(1).as_bytes());
        assert_eq!(&encoded[20..23], &500u32.to_be_bytes()[1..4]);
        assert_eq!(&encoded[23..], addr(2).as_bytes());
    }

    #[test]
    fn rejects_univ3_path_without_fee_on_second_hop() {
        let path = vec![(addr(1), Some(500)), (addr(2), None)];
        let err = encode_univ3_path(&path).expect_err("path should be rejected");
        assert!(err.to_string().contains("missing fee for hop 1"));
    }
}
