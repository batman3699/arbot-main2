//! The one conversion boundary between `alloy-primitives` and `ethers-core`.
//!
//! `apex-types` speaks alloy (§2.2 C-10) and the pricing layer still speaks
//! ethers. `lib.rs` reserved this module for *"Phase 1, when the first crate
//! actually has to cross it"*. Phase 1 never did. **`apex-econ` is the first,**
//! because `sizing::discrete::refine` reads a size from `apex_math`'s route
//! evaluation — ethers — and mints an `apex_types::DiscreteSize` — alloy.
//!
//! # Why this is safe rather than merely convenient
//!
//! Both types are unsigned 256-bit integers with a defined big-endian byte
//! representation, so the conversion is **total and exact in both directions**:
//! every value of one is a value of the other, and the round trip is the
//! identity. There is no truncation to reason about and no failure case to
//! handle, which is why these return values rather than `Result`.
//!
//! The functions live here, in the crate that owns neither representation's
//! hot path, so the boundary is one greppable place rather than a `from_be`
//! call scattered through the economics.

use alloy_primitives::U256 as AlloyU256;
use ethers_core::types::U256 as EthersU256;

/// ethers → alloy.
pub fn u256_to_alloy(value: EthersU256) -> AlloyU256 {
    let mut bytes = [0u8; 32];
    value.to_big_endian(&mut bytes);
    AlloyU256::from_be_bytes(bytes)
}

/// alloy → ethers.
pub fn u256_to_ethers(value: AlloyU256) -> EthersU256 {
    EthersU256::from_big_endian(&value.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions, at the values where a 256-bit conversion goes wrong if
    /// anyone reached for a `u128` or got the endianness backwards.
    #[test]
    fn the_round_trip_is_the_identity_at_every_boundary() {
        let cases = [
            EthersU256::zero(),
            EthersU256::one(),
            EthersU256::from(u64::MAX),
            EthersU256::from(u128::MAX),
            EthersU256::from(1u64) << 128,
            EthersU256::from(1u64) << 255,
            EthersU256::MAX,
            // A wei amount with no symmetry, so a byte-order slip cannot
            // accidentally round-trip.
            EthersU256::from_dec_str("123456789012345678901234567890").expect("literal"),
        ];
        for value in cases {
            let there = u256_to_alloy(value);
            let back = u256_to_ethers(there);
            assert_eq!(back, value, "round trip changed {value}");
        }
    }

    /// The two representations agree on ORDER, not just on bits. A size
    /// comparison that flipped across the boundary would pick the wrong trade.
    #[test]
    fn the_conversion_preserves_ordering() {
        let a = EthersU256::from_dec_str("1000000000000000000").expect("literal");
        let b = EthersU256::from_dec_str("2000000000000000000").expect("literal");
        assert!(a < b);
        assert!(u256_to_alloy(a) < u256_to_alloy(b));
        assert!(u256_to_ethers(u256_to_alloy(a)) < u256_to_ethers(u256_to_alloy(b)));
    }

    /// Endianness, stated as a value rather than trusted.
    #[test]
    fn a_known_value_has_the_bytes_it_should() {
        let one = u256_to_alloy(EthersU256::one());
        let bytes = one.to_be_bytes::<32>();
        assert_eq!(bytes[31], 1, "one must be in the LAST byte, big-endian");
        assert!(bytes[..31].iter().all(|b| *b == 0));
    }
}
