//! Pure decoding of pool events. No I/O, no async, no state.
//!
//! Topic constants are DERIVED from their event signatures rather than
//! hardcoded. The previous hardcoded values (`0x1c4168cd…`, `0xd78ad95f…5c01`)
//! each had a correct prefix and a fabricated tail, matched no log on any
//! chain, and left the pool monitor subscribed but deaf for the life of the
//! process. Derivation removes that failure mode entirely.

use ethers::types::{Log, H256, U256};
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
    ]
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
