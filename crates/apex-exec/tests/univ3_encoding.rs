//! The UniV3 step payload, asserted against the bytes the contract decodes.
//!
//! `crates/apex-exec/tests/fixtures/univ3_steps.json` holds the encodings.
//! **Neither side generates it**: this asserts `univ3_step` produces those bytes,
//! and `test/PlanCommitmentFixture.t.sol::testUniV3StepsDecode` asserts
//! `abi.decode` of the same bytes yields the recorded fields. A generator on
//! either side would make the two agree by construction.
//!
//! Decoding rather than hashing is deliberate here. An ABI offset that is wrong
//! by one word does not revert — it decodes as garbage — so a hash comparison
//! would tell you the bytes changed and a decode tells you what they now mean.

use alloy_primitives::{address, Address, U256};
use apex_exec::encode::{apply_slippage, univ3_path, univ3_step, EncodeError, PathHop};
use std::collections::BTreeMap;

const TOKEN_A: Address = address!("1111111111111111111111111111111111111111");
const TOKEN_B: Address = address!("2222222222222222222222222222222222222222");

struct Case {
    path: Vec<u8>,
    amount_in: U256,
    min_out: U256,
    encoded: Vec<u8>,
}

fn fixture() -> BTreeMap<String, Case> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/univ3_steps.json");
    let text = std::fs::read_to_string(path).expect("the tracked fixture");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    doc.get("cases")
        .and_then(serde_json::Value::as_object)
        .expect("cases")
        .iter()
        .map(|(k, v)| {
            let hex = |field: &str| {
                let s = v.get(field).and_then(serde_json::Value::as_str).expect(field);
                hex_bytes(s)
            };
            let dec = |field: &str| -> U256 {
                v.get(field)
                    .and_then(serde_json::Value::as_str)
                    .expect(field)
                    .parse()
                    .expect("a decimal U256")
            };
            (
                k.clone(),
                Case {
                    path: hex("path"),
                    amount_in: dec("amount_in"),
                    min_out: dec("min_out"),
                    encoded: hex("encoded"),
                },
            )
        })
        .collect()
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// Every fixture case, re-encoded.
#[test]
fn univ3_step_matches_the_recorded_encoding() {
    for (name, case) in fixture() {
        let encoded =
            univ3_step(&case.path, case.amount_in, case.min_out).expect("encodable");
        assert_eq!(
            encoded, case.encoded,
            "{name}: the encoding drifted. The contract would decode this into a \
             different trade, and an ABI offset that is wrong by one word decodes \
             as garbage rather than reverting."
        );
    }
}

/// The packed path: `token || fee(3) || token || … || token`. Length is
/// `20 + 23n`, which is what distinguishes a 2-hop path from a 1-hop one to the
/// router's own decoder.
#[test]
fn the_packed_path_is_token_fee_token() {
    let one = univ3_path(&[PathHop { token_in: TOKEN_A, fee: 500 }], TOKEN_B).expect("path");
    assert_eq!(one.len(), 43, "20 + 3 + 20");
    assert_eq!(&one[..20], TOKEN_A.as_slice());
    assert_eq!(&one[20..23], &[0x00, 0x01, 0xf4], "500, big-endian, three bytes");
    assert_eq!(&one[23..], TOKEN_B.as_slice());

    let two = univ3_path(
        &[PathHop { token_in: TOKEN_A, fee: 500 }, PathHop { token_in: TOKEN_B, fee: 3_000 }],
        TOKEN_A,
    )
    .expect("path");
    assert_eq!(two.len(), 66, "20 + 23 * 2");
}

/// A fee above three bytes is refused, not truncated. A truncated fee is a path
/// that points at a different pool — which prices, executes, and is not the trade
/// that was simulated.
#[test]
fn a_fee_that_does_not_fit_is_refused() {
    let err = univ3_path(&[PathHop { token_in: TOKEN_A, fee: 0x0100_0000 }], TOKEN_B)
        .expect_err("must refuse");
    assert_eq!(err, EncodeError::FeeTooLarge { fee: 0x0100_0000 });

    // The largest fee that does fit is accepted, so the bound is exact.
    assert!(univ3_path(&[PathHop { token_in: TOKEN_A, fee: 0x00ff_ffff }], TOKEN_B).is_ok());
}

#[test]
fn an_empty_path_is_refused() {
    assert_eq!(univ3_path(&[], TOKEN_B).expect_err("must refuse"), EncodeError::EmptyPath);
}

/// **`minOut == 0` is what `_execUniswap` reverts on**, and it is right to: a
/// zero minimum accepts any output, which on a public mempool is a donation.
#[test]
fn a_zero_min_out_is_refused_at_both_ends() {
    let path = univ3_path(&[PathHop { token_in: TOKEN_A, fee: 500 }], TOKEN_B).expect("path");
    assert_eq!(
        univ3_step(&path, U256::from(1u64), U256::ZERO).expect_err("must refuse"),
        EncodeError::ZeroMinOut
    );

    // ...and the slippage arithmetic saturates to 1 rather than producing one.
    assert_eq!(apply_slippage(U256::from(1u64), 9_999).expect("slippage"), U256::from(1u64));
    assert_eq!(
        apply_slippage(U256::ZERO, 30).expect_err("no expected output"),
        EncodeError::ZeroMinOut
    );
}

/// Slippage is basis points, and above 10,000 it is not a bound.
#[test]
fn slippage_is_bounded_and_exact() {
    let out = U256::from(1_000_000u64);
    assert_eq!(apply_slippage(out, 0).expect("none"), out, "zero slippage keeps everything");
    assert_eq!(apply_slippage(out, 30).expect("30 bps"), U256::from(997_000u64));
    assert_eq!(apply_slippage(out, 10_000).expect("all of it"), U256::from(1u64), "saturates");
    assert_eq!(
        apply_slippage(out, 10_001).expect_err("out of range"),
        EncodeError::SlippageOutOfRange { bps: 10_001 }
    );
}

/// The ABI tail is padded to a word boundary. A `bytes` whose tail is not padded
/// is not a valid encoding, and a decoder reading past it gets the next field.
#[test]
fn the_path_is_right_padded_to_a_word() {
    let path = univ3_path(&[PathHop { token_in: TOKEN_A, fee: 500 }], TOKEN_B).expect("path");
    assert_eq!(path.len(), 43, "not a multiple of 32");

    let encoded = univ3_step(&path, U256::from(1u64), U256::from(1u64)).expect("encodable");
    assert_eq!(encoded.len() % 32, 0, "the whole encoding is whole words");
    assert_eq!(encoded.len(), 96 + 32 + 64, "head + length + two padded words");
    assert!(
        encoded[96 + 32 + 43..].iter().all(|b| *b == 0),
        "the pad must be zero, not whatever was in the buffer"
    );
}

/// The offset is `0x60` — past all three head words — not `0x20`. This is the
/// classic ABI mistake and it decodes as garbage rather than reverting.
#[test]
fn the_dynamic_offset_points_past_the_whole_head() {
    let path = univ3_path(&[PathHop { token_in: TOKEN_A, fee: 500 }], TOKEN_B).expect("path");
    let encoded = univ3_step(&path, U256::from(7u64), U256::from(9u64)).expect("encodable");

    assert_eq!(
        U256::from_be_slice(&encoded[..32]),
        U256::from(0x60u64),
        "three head words: offset, amountIn, minOut"
    );
    assert_eq!(U256::from_be_slice(&encoded[32..64]), U256::from(7u64));
    assert_eq!(U256::from_be_slice(&encoded[64..96]), U256::from(9u64));
    assert_eq!(U256::from_be_slice(&encoded[96..128]), U256::from(43u64), "the path length");
}
