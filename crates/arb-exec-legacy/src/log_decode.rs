//! Pure decoding of pool events. No I/O, no async, no state.
//!
//! Topic constants are DERIVED from their event signatures rather than
//! hardcoded. The previous hardcoded values (`0x1c4168cd…`, `0xd78ad95f…5c01`)
//! each had a correct prefix and a fabricated tail, matched no log on any
//! chain, and left the pool monitor subscribed but deaf for the life of the
//! process. Derivation removes that failure mode entirely.

use ethers::types::{I256, Log, H256, U256};
use std::sync::LazyLock;

/// keccak256 of an event signature — the value that appears as `topics[0]`.
pub fn topic_of(signature: &str) -> H256 {
    H256(ethers::utils::keccak256(signature.as_bytes()))
}

/// `Sync(uint112 reserve0, uint112 reserve1)` — UniV2 / Solidly / Aerodrome.
/// Emitted on every reserve change, so it carries complete state.
pub static TOPIC_V2_SYNC: LazyLock<H256> = LazyLock::new(|| topic_of("Sync(uint112,uint112)"));

/// `Swap(address indexed sender, uint amount0In, uint amount1In,
///       uint amount0Out, uint amount1Out, address indexed to)` — UniV2.
pub static TOPIC_V2_SWAP: LazyLock<H256> =
    LazyLock::new(|| topic_of("Swap(address,uint256,uint256,uint256,uint256,address)"));

/// `Sync(uint256 reserve0, uint256 reserve1)` — Solidly forks (Aerodrome,
/// Velodrome). The reserves are uint256, not uint112, so this is a DIFFERENT
/// topic from [`TOPIC_V2_SYNC`] and a UniV2-only filter matches these pools
/// never.
///
/// This is not hypothetical: the Base pool monitor watches 23 Solidly pools and
/// zero UniV2 pools, so subscribing to the UniV2 pair alone delivered nothing
/// at all. Verified on Base at block 0x3045206 from pool
/// `0xcdac0d6c6c59727a65f871236188350531885c43`.
pub static TOPIC_SOLIDLY_SYNC: LazyLock<H256> =
    LazyLock::new(|| topic_of("Sync(uint256,uint256)"));

/// `Swap(address indexed sender, address indexed to, uint amount0In,
///       uint amount1In, uint amount0Out, uint amount1Out)` — Solidly forks.
///
/// `to` is the SECOND indexed argument here, where UniV2 puts it last, so the
/// signature and therefore the topic differ.
pub static TOPIC_SOLIDLY_SWAP: LazyLock<H256> =
    LazyLock::new(|| topic_of("Swap(address,address,uint256,uint256,uint256,uint256)"));

/// `Swap(address indexed sender, address indexed recipient, int256 amount0,
///       int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)`
///
/// Uniswap V3 AND Aerodrome Slipstream. Confirmed on Base at block 0x3049142:
/// six Slipstream pools and one UniV3 pool emitted this same topic in one
/// block, with an identical five-word payload — so ONE decoder covers both.
///
/// PancakeSwap V3 uses a DIFFERENT topic (`0x19b47279…`, two extra trailing
/// fields) which is not yet verified against a real log, so it is not decoded.
pub static TOPIC_CL_SWAP: LazyLock<H256> =
    LazyLock::new(|| topic_of("Swap(address,address,int256,int256,uint160,uint128,int24)"));

/// Every topic the pool monitor subscribes to, across both pool families.
///
/// Kept as one list so a venue cannot be silently omitted from the filter: the
/// monitored set mixes UniV2 and Solidly pools, and covering only one family
/// looks identical to a quiet market.
pub fn monitored_topics() -> Vec<H256> {
    vec![
        *TOPIC_V2_SYNC,
        *TOPIC_V2_SWAP,
        *TOPIC_SOLIDLY_SYNC,
        *TOPIC_SOLIDLY_SWAP,
        *TOPIC_CL_SWAP,
        *TOPIC_CL_MINT,
        *TOPIC_CL_BURN,
    ]
}

/// Topics we subscribe to but deliberately do NOT decode a payload from.
///
/// A V2/Solidly `Swap` is redundant for state purposes: the pool emits a
/// `Sync` in the same transaction carrying COMPLETE reserves, so the Swap adds
/// nothing a decoder could use. We still subscribe, because the log is what
/// marks the pool dirty.
///
/// This is distinct from a topic we do not recognise at all. Conflating them
/// makes `live_state_undecodable_total` useless for its actual job — warning
/// that some venue is emitting something we do not understand.
pub fn is_known_non_state_topic(topic: &H256) -> bool {
    topic == &*TOPIC_V2_SWAP || topic == &*TOPIC_SOLIDLY_SWAP
}


/// `Mint(address sender, address indexed owner, int24 indexed tickLower,
///       int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)`
///
/// UniV3 and Slipstream. Adds liquidity to a range.
pub static TOPIC_CL_MINT: LazyLock<H256> = LazyLock::new(|| {
    topic_of("Mint(address,address,int24,int24,uint128,uint256,uint256)")
});

/// `Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper,
///       uint128 amount, uint256 amount0, uint256 amount1)`
///
/// Removes liquidity from a range.
///
/// Measured need: with `Swap` alone, local `liquidity` went stale whenever a
/// position changed. Two of 48 CL observations diverged by +8656 and +1314 bps
/// on liquidity while price and tick stayed exact — traced to `Burn`s landing
/// later in the same block as the observed `Swap`.
pub static TOPIC_CL_BURN: LazyLock<H256> =
    LazyLock::new(|| topic_of("Burn(address,int24,int24,uint128,uint256,uint256)"));

/// A position change: how much liquidity moved, and over which tick range.
///
/// This is PRICING state. It deliberately carries no `amount0`/`amount1`:
/// balances are RPC-anchored, never log-derived (spec §3.2.1), because
/// `Collect`, `CollectProtocol`, `Flash` and plain ERC-20 transfers also move
/// them — and a direct transfer emits no pool event at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClLiquidityDelta {
    pub tick_lower: i32,
    pub tick_upper: i32,
    /// Positive for `Mint`, negative for `Burn`.
    pub liquidity_delta: i128,
}

/// Sign-extend an indexed `int24` topic word to `i32`.
fn topic_to_i32(t: &H256) -> i32 {
    word_to_i32(t.0)
}

/// Decode a `Mint` or `Burn` into a signed liquidity delta over a tick range.
///
/// `tickLower`/`tickUpper` are INDEXED, so they arrive in `topics[2]`/
/// `topics[3]`, not the payload. Reading them from data would yield garbage
/// ranges that still parse.
///
/// The `amount` offset differs between the two: `Burn`'s payload starts with
/// `amount`, while `Mint`'s starts with the non-indexed `sender`.
pub fn decode_cl_liquidity(log: &Log) -> Option<ClLiquidityDelta> {
    let topic = log.topics.first()?;
    let is_mint = topic == &*TOPIC_CL_MINT;
    if !is_mint && topic != &*TOPIC_CL_BURN {
        return None;
    }
    let tick_lower = topic_to_i32(log.topics.get(2)?);
    let tick_upper = topic_to_i32(log.topics.get(3)?);
    // Mint: [sender, amount, amount0, amount1]; Burn: [amount, amount0, amount1].
    let amount_word = if is_mint { 1 } else { 0 };
    let amount = U256::from_big_endian(&word(&log.data, amount_word)?);
    let amount = i128::try_from(amount.as_u128()).ok()?;
    Some(ClLiquidityDelta {
        tick_lower,
        tick_upper,
        liquidity_delta: if is_mint { amount } else { -amount },
    })
}

/// The `index`-th 32-byte ABI word of `data`, or `None` if it is not there.
pub fn word(data: &[u8], index: usize) -> Option<[u8; 32]> {
    let start = index.checked_mul(32)?;
    let end = start.checked_add(32)?;
    let slice = data.get(start..end)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(slice);
    Some(out)
}

/// Reserves after a `Sync`, for either pool family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2SyncDelta {
    pub reserve0: U256,
    pub reserve1: U256,
}

/// Decode a `Sync` from either UniV2 (`uint112`) or Solidly (`uint256`).
///
/// The payload layout is identical — ABI pads both widths to 32 bytes — so the
/// topic is the only thing that differs, and this accepts both. `Sync` carries
/// COMPLETE state rather than a delta, which is why V2 needs no drift budget.
pub fn decode_v2_sync(log: &Log) -> Option<V2SyncDelta> {
    let topic = log.topics.first()?;
    if topic != &*TOPIC_V2_SYNC && topic != &*TOPIC_SOLIDLY_SYNC {
        return None;
    }
    Some(V2SyncDelta {
        reserve0: U256::from_big_endian(&word(&log.data, 0)?),
        reserve1: U256::from_big_endian(&word(&log.data, 1)?),
    })
}

/// Post-swap CL pool state, straight out of the log.
///
/// `sqrt_price_x96`, `liquidity` and `tick` are the pool's new `slot0`/
/// `liquidity()` — no RPC needed. `amount0`/`amount1` are signed pool-balance
/// deltas, retained for the balance tracking Phase 2 adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClSwapDelta {
    pub amount0: I256,
    pub amount1: I256,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
}

/// Sign-extend a two's-complement ABI word to `i32`.
///
/// `int24` arrives sign-extended across all 32 bytes, so reading the low bytes
/// unsigned turns tick -198238 into a huge positive number and puts the pool on
/// the wrong side of the curve.
fn word_to_i32(w: [u8; 32]) -> i32 {
    let negative = w[0] & 0x80 != 0;
    let mut v: i64 = 0;
    for b in &w[28..32] {
        v = (v << 8) | i64::from(*b);
    }
    if negative {
        v -= 1i64 << 32;
    }
    v as i32
}

pub fn decode_cl_swap(log: &Log) -> Option<ClSwapDelta> {
    if log.topics.first()? != &*TOPIC_CL_SWAP {
        return None;
    }
    let amount0 = I256::from_raw(U256::from_big_endian(&word(&log.data, 0)?));
    let amount1 = I256::from_raw(U256::from_big_endian(&word(&log.data, 1)?));
    let sqrt_price_x96 = U256::from_big_endian(&word(&log.data, 2)?);
    let liquidity = U256::from_big_endian(&word(&log.data, 3)?).as_u128();
    let tick = word_to_i32(word(&log.data, 4)?);
    Some(ClSwapDelta {
        amount0,
        amount1,
        sqrt_price_x96,
        liquidity,
        tick,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Address, Bytes};

    fn log_with(topic: H256, data: Vec<u8>) -> Log {
        Log {
            address: Address::from_low_u64_be(1),
            topics: vec![topic],
            data: Bytes::from(data),
            ..Default::default()
        }
    }

    /// N 32-byte words, big-endian.
    fn words(vals: &[u128]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in vals {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            out.extend_from_slice(&w);
        }
        out
    }

    #[test]
    fn swap_topics_are_known_but_not_state_bearing() {
        assert!(is_known_non_state_topic(&TOPIC_V2_SWAP));
        assert!(is_known_non_state_topic(&TOPIC_SOLIDLY_SWAP));
        assert!(
            !is_known_non_state_topic(&TOPIC_V2_SYNC),
            "Sync carries state and must be decoded"
        );
        assert!(!is_known_non_state_topic(&TOPIC_CL_SWAP));
        assert!(!is_known_non_state_topic(&H256::repeat_byte(0xAB)));
    }

    #[test]
    fn decodes_univ2_sync_reserves() {
        let log = log_with(*TOPIC_V2_SYNC, words(&[111, 222]));
        let d = decode_v2_sync(&log).expect("should decode");
        assert_eq!(d.reserve0, U256::from(111u64));
        assert_eq!(d.reserve1, U256::from(222u64));
    }

    /// Same payload shape, different topic — one decoder covers both families.
    #[test]
    fn decodes_solidly_sync_with_the_same_layout() {
        let log = log_with(*TOPIC_SOLIDLY_SYNC, words(&[333, 444]));
        let d = decode_v2_sync(&log).expect("should decode");
        assert_eq!(d.reserve0, U256::from(333u64));
        assert_eq!(d.reserve1, U256::from(444u64));
    }

    #[test]
    fn refuses_a_foreign_topic() {
        let log = log_with(*TOPIC_V2_SWAP, words(&[1, 2]));
        assert!(decode_v2_sync(&log).is_none(), "topic0 decides, nothing else");
    }

    /// A truncated payload must decline rather than read garbage or panic.
    #[test]
    fn refuses_a_short_payload() {
        let log = log_with(*TOPIC_V2_SYNC, vec![0u8; 63]);
        assert!(decode_v2_sync(&log).is_none());
    }

    #[test]
    fn refuses_a_log_with_no_topics() {
        let mut log = log_with(*TOPIC_V2_SYNC, words(&[1, 2]));
        log.topics.clear();
        assert!(decode_v2_sync(&log).is_none());
    }

    #[test]
    fn liquidity_event_topics_match_their_signatures() {
        assert_eq!(
            *TOPIC_CL_MINT,
            topic_of("Mint(address,address,int24,int24,uint128,uint256,uint256)")
        );
        assert_eq!(
            *TOPIC_CL_BURN,
            topic_of("Burn(address,int24,int24,uint128,uint256,uint256)")
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_CL_MINT),
            "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_CL_BURN),
            "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
        );
    }

    /// tickLower and tickUpper are INDEXED, so they live in topics[2] and
    /// topics[3], not in the data. Reading them from the payload would silently
    /// produce garbage ranges.
    fn liquidity_log(topic: H256, lower: i32, upper: i32, amount: u128, data_words: usize) -> Log {
        let enc = |v: i32| {
            let mut w = [0u8; 32];
            let bytes = (v as i64).to_be_bytes();
            let fill = if v < 0 { 0xffu8 } else { 0x00u8 };
            for b in w.iter_mut().take(24) {
                *b = fill;
            }
            w[24..].copy_from_slice(&bytes);
            H256(w)
        };
        // Mint's payload leads with `amount` for Burn but `sender`+`amount` for
        // Mint; `data_words` selects which layout to build.
        let mut data = Vec::new();
        if data_words == 3 {
            // Burn: amount, amount0, amount1
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&amount.to_be_bytes());
            data.extend_from_slice(&w);
            data.extend_from_slice(&[0u8; 64]);
        } else {
            // Mint: sender, amount, amount0, amount1
            data.extend_from_slice(&[0u8; 32]);
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&amount.to_be_bytes());
            data.extend_from_slice(&w);
            data.extend_from_slice(&[0u8; 64]);
        }
        Log {
            address: Address::from_low_u64_be(1),
            topics: vec![topic, H256::zero(), enc(lower), enc(upper)],
            data: Bytes::from(data),
            ..Default::default()
        }
    }

    #[test]
    fn decodes_a_mint_as_a_positive_liquidity_delta() {
        let log = liquidity_log(*TOPIC_CL_MINT, -100, 100, 5_000, 4);
        let d = decode_cl_liquidity(&log).expect("should decode");
        assert_eq!(d.tick_lower, -100);
        assert_eq!(d.tick_upper, 100);
        assert_eq!(d.liquidity_delta, 5_000i128);
    }

    #[test]
    fn decodes_a_burn_as_a_negative_liquidity_delta() {
        let log = liquidity_log(*TOPIC_CL_BURN, -100, 100, 5_000, 3);
        let d = decode_cl_liquidity(&log).expect("should decode");
        assert_eq!(d.liquidity_delta, -5_000i128, "a burn removes liquidity");
    }

    /// Real Base values: block 0x304cfed carried Burns at ticks
    /// 0xfffd1520 / 0xfffd15e8, both negative.
    #[test]
    fn decodes_negative_tick_bounds_from_topics() {
        let log = liquidity_log(*TOPIC_CL_BURN, -191_200, -191_000, 1, 3);
        let d = decode_cl_liquidity(&log).expect("should decode");
        assert_eq!(d.tick_lower, -191_200);
        assert_eq!(d.tick_upper, -191_000);
    }

    #[test]
    fn liquidity_decoder_refuses_a_foreign_topic() {
        let log = liquidity_log(*TOPIC_CL_SWAP, -100, 100, 5_000, 3);
        assert!(decode_cl_liquidity(&log).is_none());
    }

    /// Indexed ticks live in topics; too few topics means we cannot know the
    /// range and must decline rather than guess.
    #[test]
    fn liquidity_decoder_refuses_missing_topics() {
        let mut log = liquidity_log(*TOPIC_CL_MINT, -100, 100, 5_000, 4);
        log.topics.truncate(2);
        assert!(decode_cl_liquidity(&log).is_none());
    }

    #[test]
    fn cl_swap_topic_matches_the_value_observed_on_chain() {
        assert_eq!(
            *TOPIC_CL_SWAP,
            topic_of("Swap(address,address,int256,int256,uint160,uint128,int24)")
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_CL_SWAP),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
    }

    /// Real Base log, block 0x3049142, pool 0x4e392fbfe4d0557c82d2f97f02ec39daa31516dd.
    /// amount1 and tick are negative, which is the case a naive unsigned read
    /// gets catastrophically wrong rather than slightly wrong.
    #[test]
    fn decodes_a_real_cl_swap_including_negative_values() {
        let data = hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000283a6dc44aa9e00",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffe56d1ffc",
            "0000000000000000000000000000000000000000000340475901e2898ee7248d",
            "00000000000000000000000000000000000000000000000001e42289497d0ed7",
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffcf9a2",
        ))
        .expect("fixture hex");
        let log = log_with(*TOPIC_CL_SWAP, data);
        let d = decode_cl_swap(&log).expect("should decode");

        // Values computed from the fixture words, not eyeballed.
        assert_eq!(d.amount0, I256::from(181_171_875_000_000_000i64));
        assert!(d.amount1.is_negative(), "amount1 must decode as negative");
        assert_eq!(d.amount1, I256::from(-445_833_220i64));
        assert_eq!(d.liquidity, 136_271_861_766_754_007u128);
        assert_eq!(d.tick, -198_238, "int24 must sign-extend");
        assert_eq!(
            d.sqrt_price_x96,
            U256::from_dec_str("3930325046233202984166541").expect("sqrt price")
        );
    }

    #[test]
    fn cl_swap_refuses_a_foreign_topic() {
        let log = log_with(*TOPIC_V2_SYNC, vec![0u8; 160]);
        assert!(decode_cl_swap(&log).is_none());
    }

    #[test]
    fn cl_swap_refuses_a_short_payload() {
        let log = log_with(*TOPIC_CL_SWAP, vec![0u8; 159]);
        assert!(
            decode_cl_swap(&log).is_none(),
            "five words are required; a partial read is worse than no read"
        );
    }

    #[test]
    fn topics_match_the_keccak_of_their_signatures() {
        assert_eq!(*TOPIC_V2_SYNC, topic_of("Sync(uint112,uint112)"));
        assert_eq!(
            *TOPIC_V2_SWAP,
            topic_of("Swap(address,uint256,uint256,uint256,uint256,address)")
        );
        assert_eq!(*TOPIC_SOLIDLY_SYNC, topic_of("Sync(uint256,uint256)"));
        assert_eq!(
            *TOPIC_SOLIDLY_SWAP,
            topic_of("Swap(address,address,uint256,uint256,uint256,uint256)")
        );
    }

    /// Solidly forks widened the reserves to uint256 and reordered Swap's
    /// indexed args, so BOTH differ from UniV2. Subscribing to the UniV2 pair
    /// alone matches an Aerodrome pool never — which is exactly what shipped.
    #[test]
    fn solidly_topics_are_distinct_from_univ2() {
        assert_ne!(*TOPIC_SOLIDLY_SYNC, *TOPIC_V2_SYNC);
        assert_ne!(*TOPIC_SOLIDLY_SWAP, *TOPIC_V2_SWAP);
    }

    /// Observed on Base at block 0x3045206 from pool
    /// 0xcdac0d6c6c59727a65f871236188350531885c43, one of the addresses the
    /// pool monitor actually watches.
    #[test]
    fn solidly_topics_match_the_values_observed_on_chain() {
        assert_eq!(
            format!("{:#x}", *TOPIC_SOLIDLY_SYNC),
            "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_SOLIDLY_SWAP),
            "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b"
        );
    }

    /// Pins the on-chain values. Derivation protects against a typo'd hash;
    /// this protects against an edited signature string, which derivation
    /// would happily and silently follow.
    ///
    /// Both values verified against live Base via `eth_getLogs` at block
    /// 0x303afcb (Sync, 6 hits) and 0x303afd2 (Swap, 2 hits).
    #[test]
    fn topics_match_the_values_observed_on_chain() {
        assert_eq!(
            format!("{:#x}", *TOPIC_V2_SYNC),
            "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_V2_SWAP),
            "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
        );
    }

    /// The exact values that shipped. Neither matches any log on Base. If
    /// either ever reappears, this test fails loudly rather than the system
    /// going quietly deaf.
    #[test]
    fn the_shipped_constants_are_rejected() {
        let sync_bug = "0x1c4168cdb0bea3c47cead55631e2d4f769596b056cc50faaa83d728afabaf805";
        let swap_bug = "0xd78ad95fa46c994b6551d0da85fc275fe613d2f6ad697fc0971df54087195c01";
        assert_ne!(format!("{:#x}", *TOPIC_V2_SYNC), sync_bug);
        assert_ne!(format!("{:#x}", *TOPIC_V2_SWAP), swap_bug);
    }
}
