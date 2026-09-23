//! The OP Stack L1 data fee, modelled locally (§23.2, Task 3.2).
//!
//! # Why model it at all
//!
//! The legacy path calls `GasPriceOracle.getL1Fee(bytes)` on chain, one
//! `eth_call` per estimate. That is accurate and it is also a round trip on the
//! hot path — and, more importantly, it makes Task 3.3's calldata optimizer
//! impossible: choosing between encodings means pricing several of them, and
//! several round trips per candidate is not a thing the fast path can do.
//!
//! # The Fjord cost function
//!
//! Base has been on Fjord since July 2024. The fee is a linear function of the
//! FastLZ-compressed size of the **signed, RLP-encoded transaction** — not of
//! the calldata alone:
//!
//! ```text
//! estimated_size_scaled = max(MIN_TX_SIZE * 1e6,
//!                             INTERCEPT + FASTLZ_COEF * fastlz_size)
//! l1_fee_scaled         = base_fee_scalar * l1_base_fee * 16
//!                       + blob_base_fee_scalar * l1_blob_base_fee
//! l1_data_fee           = estimated_size_scaled * l1_fee_scaled / 1e12
//! ```
//!
//! `INTERCEPT` is **negative**, which is the part that surprises: a transaction
//! whose compressed size is under ~51 bytes produces a negative linear term and
//! the `MIN_TX_SIZE` clamp is what it actually pays. The clamp is not a safety
//! rail, it is the operative branch for small transactions.
//!
//! # This model is NOT validated against a real receipt
//!
//! Task 3.2's acceptance criterion is reproduction of recorded Base receipts
//! within 1% across at least 50 of them. **That has not been done**: there are
//! no Base receipts in this repository — `broadcast/` holds Ethereum deploy
//! artifacts, which carry no `l1Fee` — and the development environment has no
//! egress to Base. The arithmetic below is implemented from the published
//! constants and is exercised by its own algebra; whether those constants match
//! what Base's oracle currently holds is unverified.
//!
//! That is carried in the types rather than in this comment. [`L1FeeModel`]
//! reports its [`Validation`], and a fee computed from an unvalidated model
//! says so to whoever consumes it.

use ethers_core::types::U256;

/// `intercept` from the Fjord cost function. Negative: see the module docs.
pub const INTERCEPT: i64 = -42_585_600;
/// `fastlzCoef`.
pub const FASTLZ_COEF: i64 = 836_500;
/// `minTransactionSize`, in bytes, before the 1e6 scaling.
pub const MIN_TX_SIZE: i64 = 100;
/// The divisor that takes `estimated_size_scaled * l1_fee_scaled` to wei.
pub const FJORD_DIVISOR: u64 = 1_000_000_000_000;
/// Bytes the oracle adds for the signature when the transaction handed to it is
/// unsigned.
pub const SIGNATURE_OVERHEAD_BYTES: u32 = 68;

/// Whether this model has been checked against the chain it models.
///
/// Same shape, and the same purpose, as `apex_venues::adapter::GasProvenance`:
/// a number that has never been compared with reality is an assumption, and an
/// assumption that does not say so is the kind §8.3 forbids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validation {
    /// Reproduced recorded receipts to within the stated tolerance.
    AgainstReceipts { count: u32, max_error_bps: u32 },
    /// Implemented from the published constants and never compared with a
    /// receipt. **May not price a live dispatch.**
    FromPublishedConstantsOnly,
}

impl Validation {
    /// §14 / INV-17's shape: an unvalidated cost model may rank and propose.
    pub const fn may_price_a_live_dispatch(self) -> bool {
        matches!(self, Self::AgainstReceipts { .. })
    }
}

/// The oracle state the fee depends on. Read once per L1 block, not per
/// estimate — that is the whole latency argument for modelling locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1FeeParameters {
    pub l1_base_fee: U256,
    pub l1_blob_base_fee: U256,
    pub base_fee_scalar: u32,
    pub blob_base_fee_scalar: u32,
}

/// The FastLZ-compressed size of a serialized transaction, and where the
/// number came from.
///
/// The distinction matters for a reason beyond bookkeeping: the calldata
/// optimizer (§23.3) compares two encodings, and a *monotone but biased*
/// estimator still ranks them correctly. The same estimator is not good enough
/// to state an absolute fee. One number, two jobs, different accuracy
/// requirements — so the provenance travels with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressedSize {
    /// From a receipt, or from the oracle itself.
    Measured(u32),
    /// From the local estimator.
    Estimated(u32),
}

impl CompressedSize {
    pub const fn bytes(self) -> u32 {
        match self {
            Self::Measured(n) | Self::Estimated(n) => n,
        }
    }

    pub const fn is_measured(self) -> bool {
        matches!(self, Self::Measured(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1DataFee {
    pub wei: U256,
    pub validation: Validation,
    pub size: CompressedSize,
}

impl L1DataFee {
    /// Whether this fee may be used to authorize a live dispatch.
    ///
    /// Both halves have to hold: a validated model fed an estimated size is
    /// still an estimate.
    pub const fn is_authoritative(&self) -> bool {
        self.validation.may_price_a_live_dispatch() && self.size.is_measured()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct L1FeeModel {
    validation: Validation,
}

impl Default for L1FeeModel {
    fn default() -> Self {
        Self::unvalidated()
    }
}

impl L1FeeModel {
    /// The only constructor available today. Named for what it is.
    pub const fn unvalidated() -> Self {
        Self {
            validation: Validation::FromPublishedConstantsOnly,
        }
    }

    /// Construct a model that has been checked against receipts.
    ///
    /// Takes the evidence as arguments so the claim cannot be made without
    /// stating what backs it.
    pub const fn validated(count: u32, max_error_bps: u32) -> Self {
        Self {
            validation: Validation::AgainstReceipts {
                count,
                max_error_bps,
            },
        }
    }

    pub const fn validation(&self) -> Validation {
        self.validation
    }

    /// The Fjord L1 data fee for a transaction of this compressed size.
    pub fn fee(&self, size: CompressedSize, params: L1FeeParameters) -> L1DataFee {
        L1DataFee {
            wei: fjord_fee(size.bytes(), params),
            validation: self.validation,
            size,
        }
    }
}

/// `max(MIN_TX_SIZE * 1e6, INTERCEPT + FASTLZ_COEF * fastlz_size)`.
///
/// Signed arithmetic, deliberately: the intercept is negative and the whole
/// point of the clamp is that the linear term goes below the floor for small
/// transactions. Computing this in unsigned arithmetic would wrap, and wrap to
/// an enormous number, which is a fee nobody would fail to notice — but the
/// near-miss version, saturating at zero, would silently charge every small
/// transaction the floor for the wrong reason.
pub fn estimated_size_scaled(fastlz_size: u32) -> u64 {
    let linear = INTERCEPT.saturating_add(FASTLZ_COEF.saturating_mul(i64::from(fastlz_size)));
    let floor = MIN_TX_SIZE.saturating_mul(1_000_000);
    linear.max(floor).max(0) as u64
}

/// `base_fee_scalar * l1_base_fee * 16 + blob_base_fee_scalar * l1_blob_base_fee`.
pub fn l1_fee_scaled(params: L1FeeParameters) -> U256 {
    params
        .l1_base_fee
        .saturating_mul(U256::from(params.base_fee_scalar))
        .saturating_mul(U256::from(16u64))
        .saturating_add(
            params
                .l1_blob_base_fee
                .saturating_mul(U256::from(params.blob_base_fee_scalar)),
        )
}

fn fjord_fee(fastlz_size: u32, params: L1FeeParameters) -> U256 {
    U256::from(estimated_size_scaled(fastlz_size))
        .saturating_mul(l1_fee_scaled(params))
        / U256::from(FJORD_DIVISOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> L1FeeParameters {
        L1FeeParameters {
            l1_base_fee: U256::from(10_000_000_000u64), // 10 gwei
            l1_blob_base_fee: U256::from(1_000_000_000u64), // 1 gwei
            base_fee_scalar: 1_368,
            blob_base_fee_scalar: 810_949,
        }
    }

    /// The clamp is the operative branch for small transactions, not a safety
    /// rail. `INTERCEPT + FASTLZ_COEF * size` is negative below ~51 bytes.
    #[test]
    fn the_minimum_size_clamp_is_where_small_transactions_actually_land() {
        let breakeven = (-INTERCEPT + MIN_TX_SIZE * 1_000_000) / FASTLZ_COEF;
        assert!(
            (150..=180).contains(&breakeven),
            "breakeven moved to {breakeven}; the constants changed"
        );
        for size in [0u32, 1, 10, 50, breakeven as u32 - 1] {
            assert_eq!(
                estimated_size_scaled(size),
                (MIN_TX_SIZE * 1_000_000) as u64,
                "size {size} must be priced at the floor"
            );
        }
        assert!(estimated_size_scaled(breakeven as u32 + 1) > (MIN_TX_SIZE * 1_000_000) as u64);
    }

    /// Unsigned arithmetic here would wrap on the negative intercept. This is
    /// the test that would have caught it.
    #[test]
    fn a_zero_size_does_not_wrap_to_an_enormous_fee() {
        let fee = L1FeeModel::unvalidated().fee(CompressedSize::Estimated(0), params());
        let floor = L1FeeModel::unvalidated().fee(CompressedSize::Estimated(1), params());
        assert_eq!(fee.wei, floor.wei);
        assert!(fee.wei < U256::from(1_000_000_000_000_000_000u128), "{}", fee.wei);
    }

    /// Above the clamp the fee is strictly increasing in size. The calldata
    /// optimizer depends on exactly this and on nothing more.
    #[test]
    fn the_fee_is_monotone_in_compressed_size() {
        let m = L1FeeModel::unvalidated();
        let mut previous = U256::zero();
        for size in [200u32, 300, 500, 1_000, 2_000, 8_000] {
            let fee = m.fee(CompressedSize::Estimated(size), params()).wei;
            assert!(fee > previous, "fee did not increase at {size} bytes");
            previous = fee;
        }
    }

    /// Doubling either L1 price component doubles its contribution — to
    /// within one wei of truncation, and in a known direction.
    ///
    /// `floor(2a) >= 2*floor(a)`, so the doubled fee is at least twice the
    /// single fee and at most one wei more. Measured here as exactly one wei
    /// on this fixture. The direction is the useful part: truncation charges
    /// the trader slightly MORE than exact linearity would, so a cost model
    /// built on it cannot understate.
    #[test]
    fn the_fee_is_linear_in_the_l1_prices() {
        let m = L1FeeModel::unvalidated();
        let size = CompressedSize::Estimated(1_000);
        let base = m.fee(size, params()).wei;

        let doubled = m
            .fee(
                size,
                L1FeeParameters {
                    l1_base_fee: params().l1_base_fee * 2,
                    l1_blob_base_fee: params().l1_blob_base_fee * 2,
                    ..params()
                },
            )
            .wei;
        assert!(
            doubled >= base * 2 && doubled <= base * 2 + U256::one(),
            "doubling the L1 price gave {doubled}, not within a wei of {}",
            base * 2
        );
    }

    /// The model says what it is, and an unvalidated one cannot price a live
    /// dispatch.
    #[test]
    fn an_unvalidated_model_is_not_authoritative() {
        let m = L1FeeModel::unvalidated();
        assert_eq!(m.validation(), Validation::FromPublishedConstantsOnly);
        assert!(!m.validation().may_price_a_live_dispatch());
        assert!(!m.fee(CompressedSize::Measured(500), params()).is_authoritative());

        // And a validated model fed an ESTIMATED size is still not authoritative:
        // both halves have to hold.
        let v = L1FeeModel::validated(50, 100);
        assert!(v.validation().may_price_a_live_dispatch());
        assert!(!v.fee(CompressedSize::Estimated(500), params()).is_authoritative());
        assert!(v.fee(CompressedSize::Measured(500), params()).is_authoritative());
    }

    /// Task 3.2's acceptance criterion, pinned as the state of the world.
    ///
    /// This test exists to FAIL when someone validates the model, forcing the
    /// claim and its evidence into the type rather than into a commit message.
    /// The same device as `no_venue_gas_figure_has_been_measured_yet`.
    #[test]
    fn the_l1_model_has_not_been_checked_against_a_receipt() {
        assert_eq!(
            L1FeeModel::default().validation(),
            Validation::FromPublishedConstantsOnly,
            "the default model claims validation -- update this test with the \
             receipt count and the measured error"
        );
    }
}
