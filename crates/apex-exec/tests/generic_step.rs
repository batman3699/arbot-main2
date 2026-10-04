//! Task 8.5 R6 — the generic adapter step and Slipstream's `exactInputSingle`;
//! and PancakeSwap's, through an `IV3SwapRouter` (adapter 2).
//!
//! Held byte-for-byte to `cast` — the independent encoder — from the signature
//! text, the way `aggregate3` is in `apex-runtime`: `cast calldata` for the
//! router call, `cast abi-encode 'f(uint16,address,uint256,bytes)'` for the
//! payload `_execAdapter` decodes. Payloads of 0 and 33 bytes cover the padding
//! either side of a word boundary.

use alloy_primitives::{address, hex, keccak256, Address, U256};
use apex_exec::encode::{
    generic_step, slipstream_exact_input_single, v3_router_exact_input_single, EncodeError, SlipstreamSwap,
    V3RouterSwap, SLIPSTREAM_EXACT_INPUT_SINGLE, V3_ROUTER_EXACT_INPUT_SINGLE,
};

const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const EXECUTOR: Address = address!("1c3d856D29eA2118c8d955070a6AD83C984586f3");

/// `cast calldata "exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))"
///  "(WETH,USDC,100,EXECUTOR,1790000000,1500000000000000000,4000000000,0)"`
const SWAP: &str = "a026383e0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000640000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f3000000000000000000000000000000000000000000000000000000006ab13b8000000000000000000000000000000000000000000000000014d1120d7b16000000000000000000000000000000000000000000000000000000000000ee6b28000000000000000000000000000000000000000000000000000000000000000000";

/// The same, `(USDC,WETH,-1,EXECUTOR,1,2,3,0)`: a negative `int24`.
const NEGATIVE: &str = "a026383e000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda029130000000000000000000000004200000000000000000000000000000000000006ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f30000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000000";

/// `cast abi-encode 'f(uint16,address,uint256,bytes)' 1 WETH 1500000000000000000 <SWAP>`
const GENERIC: &str = "0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000420000000000000000000000000000000000000600000000000000000000000000000000000000000000000014d1120d7b16000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000104a026383e0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000640000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f3000000000000000000000000000000000000000000000000000000006ab13b8000000000000000000000000000000000000000000000000014d1120d7b16000000000000000000000000000000000000000000000000000000000000ee6b2800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

/// `… 65535 USDC 0 0x`: an empty payload is its length word and nothing.
const GENERIC_EMPTY: &str = "000000000000000000000000000000000000000000000000000000000000ffff000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000000";

/// `… 7 USDC 5 0xab×33`: one byte past a word, padded to two.
const GENERIC_33: &str = "0000000000000000000000000000000000000000000000000000000000000007000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913000000000000000000000000000000000000000000000000000000000000000500000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000021ababababababababababababababababababababababababababababababababab00000000000000000000000000000000000000000000000000000000000000";

fn swap() -> SlipstreamSwap {
    SlipstreamSwap {
        token_in: WETH,
        token_out: USDC,
        tick_spacing: 100,
        recipient: EXECUTOR,
        deadline: 1_790_000_000,
        amount_in: U256::from(1_500_000_000_000_000_000u128),
        min_out: U256::from(4_000_000_000u64),
    }
}

#[test]
fn the_selector_is_its_signatures_hash() {
    let sig = "exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))";
    assert_eq!(SLIPSTREAM_EXACT_INPUT_SINGLE, keccak256(sig.as_bytes())[..4]);
}

#[test]
fn a_slipstream_swap_encodes_as_cast_does() {
    assert_eq!(hex::encode(slipstream_exact_input_single(&swap()).unwrap()), SWAP);
    let negative = SlipstreamSwap {
        token_in: USDC,
        token_out: WETH,
        tick_spacing: -1,
        deadline: 1,
        amount_in: U256::from(2u64),
        min_out: U256::from(3u64),
        ..swap()
    };
    assert_eq!(hex::encode(slipstream_exact_input_single(&negative).unwrap()), NEGATIVE);
}

#[test]
fn the_generic_step_encodes_as_cast_does() {
    let call = slipstream_exact_input_single(&swap()).unwrap();
    assert_eq!(
        hex::encode(generic_step(1, WETH, U256::from(1_500_000_000_000_000_000u128), &call)),
        GENERIC
    );
    assert_eq!(hex::encode(generic_step(u16::MAX, USDC, U256::ZERO, &[])), GENERIC_EMPTY);
    assert_eq!(hex::encode(generic_step(7, USDC, U256::from(5u64), &[0xab; 33])), GENERIC_33);
}

/// A zero minimum accepts any output, and a spacing outside `int24` names a
/// different pool: both refused, never encoded.
#[test]
fn a_swap_that_cannot_be_bounded_or_placed_is_refused() {
    let unbounded = SlipstreamSwap { min_out: U256::ZERO, ..swap() };
    assert_eq!(slipstream_exact_input_single(&unbounded), Err(EncodeError::ZeroMinOut));
    for spacing in [1 << 23, -(1 << 23) - 1] {
        let wide = SlipstreamSwap { tick_spacing: spacing, ..swap() };
        assert_eq!(
            slipstream_exact_input_single(&wide),
            Err(EncodeError::TickSpacingOutOfRange { spacing })
        );
    }
    // The int24 extremes themselves fit.
    for spacing in [(1 << 23) - 1, -(1 << 23)] {
        assert!(slipstream_exact_input_single(&SlipstreamSwap { tick_spacing: spacing, ..swap() }).is_ok());
    }
}

// ------------------------------------------------------------------ IV3SwapRouter

/// `cast calldata "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))"
///  "(WETH,USDC,100,EXECUTOR,1500000000000000000,4000000000,0)"`
const V3_SWAP: &str = "04e45aaf0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000640000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f300000000000000000000000000000000000000000000000014d1120d7b16000000000000000000000000000000000000000000000000000000000000ee6b28000000000000000000000000000000000000000000000000000000000000000000";

/// The same, `(USDC,WETH,16777215,EXECUTOR,1,2,0)`: the widest `uint24`.
const V3_WIDEST: &str = "04e45aaf000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000042000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000ffffff0000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f3000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000";

fn v3_swap() -> V3RouterSwap {
    V3RouterSwap {
        token_in: WETH,
        token_out: USDC,
        fee: 100,
        recipient: EXECUTOR,
        amount_in: U256::from(1_500_000_000_000_000_000u128),
        min_out: U256::from(4_000_000_000u64),
    }
}

#[test]
fn the_v3_router_selector_is_its_signatures_hash() {
    let sig = "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))";
    assert_eq!(V3_ROUTER_EXACT_INPUT_SINGLE, keccak256(sig.as_bytes())[..4]);
}

#[test]
fn a_v3_router_swap_encodes_as_cast_does() {
    assert_eq!(hex::encode(v3_router_exact_input_single(&v3_swap()).unwrap()), V3_SWAP);
    let widest = V3RouterSwap {
        token_in: USDC,
        token_out: WETH,
        fee: 0x00ff_ffff,
        amount_in: U256::from(1u64),
        min_out: U256::from(2u64),
        ..v3_swap()
    };
    assert_eq!(hex::encode(v3_router_exact_input_single(&widest).unwrap()), V3_WIDEST);
}

/// A zero minimum, and a fee a `uint24` cannot hold — which would name another
/// pool once truncated — are refused, never encoded.
#[test]
fn a_v3_router_swap_that_cannot_be_bounded_or_placed_is_refused() {
    let unbounded = V3RouterSwap { min_out: U256::ZERO, ..v3_swap() };
    assert_eq!(v3_router_exact_input_single(&unbounded), Err(EncodeError::ZeroMinOut));
    let wide = V3RouterSwap { fee: 1 << 24, ..v3_swap() };
    assert_eq!(v3_router_exact_input_single(&wide), Err(EncodeError::FeeTooLarge { fee: 1 << 24 }));
}
