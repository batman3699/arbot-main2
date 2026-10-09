//! The handful of contract calls the live pool book makes, as bytes.
//!
//! Hand-written for the same reason `apex-exec::encode` is: the set is small,
//! and a generated binding would hide the one property that matters — that these
//! bytes are what the contract decodes — inside a macro. Every selector is
//! written as the four bytes of its signature's hash, and `tests/live_book.rs`
//! recomputes each from the signature text; the `aggregate3` encoder is held
//! byte-for-byte to `cast calldata`, and the decoder to a real response recorded
//! from Base.

use alloy_primitives::{address, Address, U256};

/// Multicall3, at its canonical address. 3,808 bytes of code on Base, checked
/// 2026-09-30.
pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Four-byte selectors, each `keccak256(signature)[..4]`.
pub mod selector {
    /// `aggregate3((address,bool,bytes)[])`
    pub const AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
    /// `slot0()` — Uniswap v3 and Slipstream return different tuples, and both
    /// begin `(uint160 sqrtPriceX96, int24 tick, …)`, which is all that is read.
    pub const SLOT0: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];
    /// `liquidity()`
    pub const LIQUIDITY: [u8; 4] = [0x1a, 0x68, 0x65, 0x02];
    /// `fee()`
    pub const FEE: [u8; 4] = [0xdd, 0xca, 0x3f, 0x43];
    /// `tickSpacing()`
    pub const TICK_SPACING: [u8; 4] = [0xd0, 0xc9, 0x3a, 0x7c];
    /// `token0()`
    pub const TOKEN0: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];
    /// `token1()`
    pub const TOKEN1: [u8; 4] = [0xd2, 0x12, 0x20, 0xa7];
    /// `factory()`
    pub const FACTORY: [u8; 4] = [0xc4, 0x5a, 0x01, 0x55];
    /// `decimals()`
    pub const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
    /// `balanceOf(address)`
    pub const BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
    /// `tickBitmap(int16)`
    pub const TICK_BITMAP: [u8; 4] = [0x53, 0x39, 0xc2, 0x96];
    /// `ticks(int24)`
    pub const TICKS: [u8; 4] = [0xf3, 0x0d, 0xba, 0x93];
    /// `observe(uint32[])` — a pool's oracle: tick cumulatives at each age.
    pub const OBSERVE: [u8; 4] = [0x88, 0x3b, 0xdb, 0xfd];
    /// `swapFeeModule()` — the Slipstream factory's fee module.
    pub const SWAP_FEE_MODULE: [u8; 4] = [0x23, 0xc4, 0x3a, 0x51];
    /// `tickSpacingToFee(int24)` — the Slipstream factory's default per spacing.
    pub const TICK_SPACING_TO_FEE: [u8; 4] = [0x38, 0x0d, 0xc1, 0xc2];
    /// `dynamicFeeConfig(address)` — the fee module's per-pool configuration.
    pub const DYNAMIC_FEE_CONFIG: [u8; 4] = [0x5b, 0xb5, 0x25, 0xff];
    /// `defaultScalingFactor()`
    pub const DEFAULT_SCALING_FACTOR: [u8; 4] = [0xba, 0x74, 0x3e, 0x38];
    /// `defaultFeeCap()`
    pub const DEFAULT_FEE_CAP: [u8; 4] = [0xdc, 0xf4, 0xeb, 0x27];
    /// `secondsAgo()` — the module's TWAP window.
    pub const SECONDS_AGO: [u8; 4] = [0x63, 0x3d, 0xd1, 0x45];
    /// `l1BaseFee()` — the L1 fee oracle's.
    pub const L1_BASE_FEE: [u8; 4] = [0x51, 0x9b, 0x4b, 0xd3];
    /// `blobBaseFee()`
    pub const BLOB_BASE_FEE: [u8; 4] = [0xf8, 0x20, 0x61, 0x40];
    /// `baseFeeScalar()`
    pub const BASE_FEE_SCALAR: [u8; 4] = [0xc5, 0x98, 0x59, 0x18];
    /// `blobBaseFeeScalar()`
    pub const BLOB_BASE_FEE_SCALAR: [u8; 4] = [0x68, 0xd5, 0xdc, 0xa6];
    /// `isFjord()`
    pub const IS_FJORD: [u8; 4] = [0x96, 0x0e, 0x3a, 0x23];
    /// `adapterOf(uint16)` — the executor's adapter registry.
    pub const ADAPTER_OF: [u8; 4] = [0xb9, 0x69, 0xa5, 0x30];
    /// `isSelectorAllowed(uint16,bytes4)`
    pub const IS_SELECTOR_ALLOWED: [u8; 4] = [0x28, 0x4a, 0xea, 0x3f];
    /// `stable()` — Aerodrome v2's curve flag (R24).
    pub const STABLE: [u8; 4] = [0x22, 0xbe, 0x3d, 0xe1];
    /// `getReserves()` — Aerodrome v2: `(uint256, uint256, uint256)`.
    pub const GET_RESERVES: [u8; 4] = [0x09, 0x02, 0xf1, 0xac];
    /// `getFee(address,bool)` — Aerodrome v2's factory, in basis points.
    pub const GET_FEE: [u8; 4] = [0xcc, 0x56, 0xb2, 0xc5];
    /// `isPool(address)` — Aerodrome v2's factory.
    pub const IS_POOL: [u8; 4] = [0x5b, 0x16, 0xeb, 0xb7];
}

const WORD: usize = 32;

/// A call with no arguments.
pub fn call0(sel: [u8; 4]) -> Vec<u8> {
    sel.to_vec()
}

/// `f(uint16)`.
pub fn call_u16(sel: [u8; 4], v: u16) -> Vec<u8> {
    let mut out = sel.to_vec();
    out.extend_from_slice(&[0u8; 30]);
    out.extend_from_slice(&v.to_be_bytes());
    out
}

/// `f(uint16,bytes4)`: the integer right-aligned in its word, and the bytes
/// **left**-aligned in theirs — a fixed-size `bytesN` is padded on the right.
pub fn call_u16_bytes4(sel: [u8; 4], v: u16, b: [u8; 4]) -> Vec<u8> {
    let mut out = call_u16(sel, v);
    out.extend_from_slice(&b);
    out.extend_from_slice(&[0u8; 28]);
    out
}

/// `f(address)`.
pub fn call_address(sel: [u8; 4], a: Address) -> Vec<u8> {
    let mut out = sel.to_vec();
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(a.as_slice());
    out
}

/// `f(address,bool)`.
pub fn call_address_bool(sel: [u8; 4], a: Address, b: bool) -> Vec<u8> {
    let mut out = call_address(sel, a);
    out.extend_from_slice(&[0u8; 31]);
    out.push(u8::from(b));
    out
}

/// `f(intN)` for a signed argument. ABI-encoded as a sign-extended 256-bit word,
/// which is why a negative `int16` word position is `0xff…ff` in front and not
/// zeros.
pub fn call_signed(sel: [u8; 4], v: i64) -> Vec<u8> {
    let mut out = sel.to_vec();
    let fill = if v < 0 { 0xff } else { 0x00 };
    out.extend_from_slice(&[fill; 24]);
    out.extend_from_slice(&v.to_be_bytes());
    out
}

/// `observe([seconds_ago, 0])`: a dynamic `uint32[]` of two, so an offset, a
/// length and the two ages.
pub fn call_observe(seconds_ago: u32) -> Vec<u8> {
    let mut out = selector::OBSERVE.to_vec();
    push_usize(&mut out, WORD);
    push_usize(&mut out, 2);
    push_usize(&mut out, seconds_ago as usize);
    push_usize(&mut out, 0);
    out
}

/// The time-weighted tick over the last `seconds_ago` from `observe([seconds_ago,
/// 0])`'s answer, computed as Slipstream's fee module computes it:
/// `int24((cumulatives[1] − cumulatives[0]) / secondsAgo)`, the division
/// truncating toward zero as Solidity's signed division does. `None` for an
/// answer that is not two `int56` cumulatives, or a zero window.
pub fn decode_twap(data: &[u8], seconds_ago: u32) -> Option<i32> {
    let off = usize_at(data, 0)?;
    if off % WORD != 0 || seconds_ago == 0 || usize_at(data, off)? != 2 {
        return None;
    }
    let first = off / WORD + 1;
    let (c0, c1) = (word_int(data, first, 56)?, word_int(data, first + 1, 56)?);
    i32::try_from((c1 - c0) / i128::from(seconds_ago)).ok().filter(|t| (-(1 << 23)..(1 << 23)).contains(t))
}

/// `Multicall3.aggregate3(calls)`, every call with `allowFailure = true`.
///
/// Allowing failure is what makes one bad pool cost one answer rather than the
/// whole batch: an `aggregate3` with `allowFailure = false` reverts entirely on
/// the first failing sub-call.
pub fn encode_aggregate3(calls: &[(Address, Vec<u8>)]) -> Vec<u8> {
    let tuple_len = |data: &[u8]| 4 * WORD + padded(data.len());
    let mut out = selector::AGGREGATE3.to_vec();
    push_usize(&mut out, WORD); // the array's offset
    push_usize(&mut out, calls.len());
    let mut next = calls.len() * WORD;
    for (_, data) in calls {
        push_usize(&mut out, next);
        next += tuple_len(data);
    }
    for (target, data) in calls {
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(target.as_slice());
        push_usize(&mut out, 1); // allowFailure
        push_usize(&mut out, 3 * WORD); // `bytes callData` starts after the head
        push_usize(&mut out, data.len());
        out.extend_from_slice(data);
        out.extend(std::iter::repeat_n(0u8, padded(data.len()) - data.len()));
    }
    out
}

/// `aggregate3`'s return: `(bool success, bytes returnData)[]`, one per call.
///
/// `None` for a malformed response — never a partial list, because a list shorter
/// than the request would shift every later answer onto the wrong call.
pub fn decode_aggregate3(data: &[u8]) -> Option<Vec<(bool, Vec<u8>)>> {
    let array = usize_at(data, 0)?;
    let n = usize_at(data, array)?;
    let base = array.checked_add(WORD)?;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let tuple = base.checked_add(usize_at(data, base.checked_add(i * WORD)?)?)?;
        let success = usize_at(data, tuple)? == 1;
        let bytes_at = tuple.checked_add(usize_at(data, tuple.checked_add(WORD)?)?)?;
        let len = usize_at(data, bytes_at)?;
        let start = bytes_at.checked_add(WORD)?;
        out.push((success, data.get(start..start.checked_add(len)?)?.to_vec()));
    }
    Some(out)
}

/// Word `i` of an ABI-encoded return.
pub fn word(data: &[u8], i: usize) -> Option<[u8; 32]> {
    let start = i.checked_mul(WORD)?;
    data.get(start..start.checked_add(WORD)?)?.try_into().ok()
}

pub fn word_u256(data: &[u8], i: usize) -> Option<U256> {
    word(data, i).map(U256::from_be_bytes)
}

/// An `address` word: twelve zero bytes, then twenty. Anything else is not an
/// address, and is refused rather than truncated into one.
pub fn word_address(data: &[u8], i: usize) -> Option<Address> {
    let w = word(data, i)?;
    w[..12].iter().all(|b| *b == 0).then(|| Address::from_slice(&w[12..]))
}

/// An unsigned word that must fit `bits` bits.
pub fn word_uint(data: &[u8], i: usize, bits: u32) -> Option<u128> {
    let v = word_u256(data, i)?;
    (v.bit_len() <= bits as usize).then(|| v.to::<u128>())
}

/// A signed word that must fit `bits` bits: sign-extended, so every byte above
/// the value is `0x00` for a non-negative value and `0xff` for a negative one.
pub fn word_int(data: &[u8], i: usize, bits: u32) -> Option<i128> {
    let w = word(data, i)?;
    let low = i128::from_be_bytes(w[16..].try_into().ok()?);
    let fill = if low < 0 { 0xff } else { 0x00 };
    if w[..16].iter().any(|b| *b != fill) {
        return None;
    }
    // At 128 bits the range is all of `i128`, and computing it as below would
    // shift a 1 into the sign bit and overflow — which is how swap amounts,
    // the first 128-bit signed words decoded, found it.
    if bits >= 128 {
        return Some(low);
    }
    let max = (1i128 << (bits - 1)) - 1;
    let min = -(1i128 << (bits - 1));
    (min..=max).contains(&low).then_some(low)
}

fn padded(len: usize) -> usize {
    len.div_ceil(WORD) * WORD
}

fn push_usize(out: &mut Vec<u8>, v: usize) {
    out.extend_from_slice(&U256::from(v).to_be_bytes::<32>());
}

fn usize_at(data: &[u8], at: usize) -> Option<usize> {
    let w: [u8; 32] = data.get(at..at.checked_add(WORD)?)?.try_into().ok()?;
    let v = U256::from_be_bytes(w);
    (v.bit_len() <= 32).then(|| v.to::<usize>())
}
