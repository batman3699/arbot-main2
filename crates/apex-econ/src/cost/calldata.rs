//! Choosing the cheapest encoding of a route (§23.3).
//!
//! # Byte count is not cost
//!
//! The intuition that a smaller encoding is a cheaper one is wrong on an OP
//! Stack chain, and wrong in a way that matters. Fjord prices the
//! **FastLZ-compressed** size of the serialized transaction, and ABI encoding
//! pads everything to 32-byte words — so a "wasteful" padded encoding is mostly
//! zero bytes in a long repeating pattern, which is exactly what a compressor
//! removes. A hand-packed encoding can be smaller on the wire and cost more to
//! post.
//!
//! So the optimizer ranks by **modelled fee**, never by length, and
//! [`cheapest`] is deliberately unable to see a byte count at all.
//!
//! # The estimator is a proxy, and says so
//!
//! Fjord's input is `flzCompressLen(tx_bytes)`. This crate does **not**
//! implement FastLZ. A port written from memory would produce a number that
//! looks authoritative and cannot be checked against Base from here — the same
//! shape of mistake as a fabricated venue address, in a different costume.
//!
//! [`byte_class_size`] is instead the pre-Ecotone Bedrock measure —
//! `4·zeros + 16·nonzeros`, scaled back to bytes — used as a **compressibility
//! proxy**. It is right about the thing that dominates here (zero bytes are
//! cheap) and wrong about everything a real compressor does with repetition.
//!
//! That is enough for the optimizer and not enough for an absolute fee, and
//! the difference is the reason [`CompressedSize`] carries its provenance:
//! ranking needs the estimator to be *monotone* in the truth, while quoting a
//! fee needs it to be *equal* to the truth. One number, two jobs, two accuracy
//! requirements.

use super::l1_data::{CompressedSize, L1DataFee, L1FeeModel, L1FeeParameters};

/// Bedrock's calldata measure, expressed in bytes rather than gas.
///
/// `(4·zeros + 16·nonzeros) / 16`, which is the Ecotone `tx_compressed_size`.
/// A zero byte counts as a quarter of a non-zero one.
pub fn byte_class_size(data: &[u8]) -> u32 {
    let zeros = data.iter().filter(|b| **b == 0).count() as u64;
    let nonzeros = data.len() as u64 - zeros;
    ((zeros * 4 + nonzeros * 16) / 16) as u32
}

/// One way of encoding a route, and what it costs to post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoding {
    /// A name for the choice, so a decision can be attributed.
    pub label: &'static str,
    pub bytes: Vec<u8>,
}

impl Encoding {
    pub fn new(label: &'static str, bytes: Vec<u8>) -> Self {
        Self { label, bytes }
    }

    /// The estimated compressed size. Always `Estimated`: see the module docs.
    pub fn size(&self) -> CompressedSize {
        CompressedSize::Estimated(byte_class_size(&self.bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen<'a> {
    pub encoding: &'a Encoding,
    pub fee: L1DataFee,
    /// What the runner-up would have cost, when there was one. The saving is
    /// reported rather than assumed, because on many routes it is zero and a
    /// caller should be able to see that the choice did not matter.
    pub next_best_fee_wei: Option<ethers_core::types::U256>,
}

/// Pick the encoding with the lowest modelled L1 data fee.
///
/// Ties go to the **first** candidate. Callers are expected to list encodings
/// in order of preference on every other axis — readability, executor support,
/// how well understood they are — so that a tie is broken by something other
/// than iteration order.
///
/// `None` for an empty candidate list: no encoding is not an encoding.
pub fn cheapest<'a>(
    encodings: &'a [Encoding],
    model: &L1FeeModel,
    params: L1FeeParameters,
) -> Option<Chosen<'a>> {
    let mut priced: Vec<(&Encoding, L1DataFee)> = encodings
        .iter()
        .map(|e| (e, model.fee(e.size(), params)))
        .collect();
    if priced.is_empty() {
        return None;
    }
    // Stable sort: equal fees keep the caller's order.
    priced.sort_by_key(|(_, fee)| fee.wei);
    let (encoding, fee) = priced[0];
    Some(Chosen {
        encoding,
        fee,
        next_best_fee_wei: priced.get(1).map(|(_, f)| f.wei),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers_core::types::U256;

    fn params() -> L1FeeParameters {
        L1FeeParameters {
            l1_base_fee: U256::from(10_000_000_000u64),
            l1_blob_base_fee: U256::from(1_000_000_000u64),
            base_fee_scalar: 1_368,
            blob_base_fee_scalar: 810_949,
        }
    }

    /// A zero byte is a quarter of a non-zero one.
    #[test]
    fn zero_bytes_are_cheaper_than_non_zero_ones() {
        assert_eq!(byte_class_size(&[0u8; 16]), 4);
        assert_eq!(byte_class_size(&[1u8; 16]), 16);
        assert_eq!(byte_class_size(&[]), 0);
    }

    /// The §23.3 claim, as a measurement: a LONGER encoding can be CHEAPER.
    ///
    /// 400 bytes of ABI padding — one non-zero word's worth of payload in
    /// every 32 — against 200 bytes of densely packed data. The padded form is
    /// twice the length and costs less to post, which is why the optimizer
    /// ranks by fee and cannot see a byte count.
    #[test]
    fn a_longer_encoding_can_cost_less_than_a_shorter_one() {
        let mut padded = Vec::new();
        for _ in 0..12 {
            padded.extend(std::iter::repeat_n(0u8, 28));
            padded.extend([0xde, 0xad, 0xbe, 0xef]);
        }
        let packed: Vec<u8> = (0..200u32).map(|i| (i % 251 + 1) as u8).collect();
        assert!(padded.len() > packed.len(), "fixture must have the padded form longer");

        let model = L1FeeModel::unvalidated();
        let candidates = [
            Encoding::new("padded", padded),
            Encoding::new("packed", packed),
        ];
        let chosen = cheapest(&candidates, &model, params()).expect("a choice");
        assert_eq!(
            chosen.encoding.label, "padded",
            "the longer, more compressible encoding must win: {} bytes beat {}",
            candidates[0].bytes.len(),
            candidates[1].bytes.len()
        );
        assert!(chosen.next_best_fee_wei.expect("runner-up") > chosen.fee.wei);
    }

    /// Two encodings that differ in size produce different fees. The other
    /// half of Task 3.3's test.
    #[test]
    fn encodings_of_different_size_price_differently() {
        let model = L1FeeModel::unvalidated();
        let small = Encoding::new("small", vec![0x11; 400]);
        let large = Encoding::new("large", vec![0x11; 1_600]);
        let fs = model.fee(small.size(), params()).wei;
        let fl = model.fee(large.size(), params()).wei;
        assert!(fl > fs, "a four-times-larger encoding must cost more");

        let candidates = [large, small];
        let chosen = cheapest(&candidates, &model, params()).expect("choice");
        assert_eq!(chosen.encoding.label, "small");
        assert_eq!(chosen.next_best_fee_wei, Some(fl));
    }

    /// Ties keep the caller's order, so the tie-break is the caller's
    /// preference rather than whichever happened to be enumerated first.
    #[test]
    fn a_tie_goes_to_the_caller_s_preferred_encoding() {
        let model = L1FeeModel::unvalidated();
        let a = Encoding::new("preferred", vec![0x22; 500]);
        let b = Encoding::new("equivalent", vec![0x33; 500]);
        assert_eq!(a.size(), b.size());
        let forwards = [a.clone(), b.clone()];
        assert_eq!(
            cheapest(&forwards, &model, params()).expect("choice").encoding.label,
            "preferred"
        );
        let backwards = [b, a];
        assert_eq!(
            cheapest(&backwards, &model, params()).expect("choice").encoding.label,
            "equivalent"
        );
    }

    /// The chosen fee is never authoritative, because the size is an estimate.
    /// The optimizer is allowed to rank on it; nothing is allowed to quote it.
    #[test]
    fn an_optimized_fee_is_still_not_authoritative() {
        let model = L1FeeModel::validated(50, 100);
        let only = [Encoding::new("only", vec![0u8; 300])];
        let chosen = cheapest(&only, &model, params()).expect("choice");
        assert!(
            !chosen.fee.is_authoritative(),
            "a fee built on an estimated size must not read as authoritative, \
             even from a validated model"
        );
    }

    #[test]
    fn no_encodings_is_not_a_choice() {
        assert!(cheapest(&[], &L1FeeModel::unvalidated(), params()).is_none());
    }

    /// The property the optimizer actually depends on: the fee is monotone in
    /// the estimated size. Ranking survives a biased estimator; it does not
    /// survive a non-monotone one.
    #[test]
    fn ranking_depends_only_on_monotonicity_not_on_accuracy() {
        let model = L1FeeModel::unvalidated();
        let mut previous = U256::zero();
        for size in [200u32, 250, 400, 800, 1_600, 3_200] {
            let fee = model.fee(CompressedSize::Estimated(size), params()).wei;
            assert!(fee > previous, "fee is not monotone at {size}");
            previous = fee;
        }
    }
}
