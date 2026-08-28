//! Pure decoding of pool events. No I/O, no async, no state.
//!
//! Topic constants are DERIVED from their event signatures rather than
//! hardcoded. The previous hardcoded values (`0x1c4168cd…`, `0xd78ad95f…5c01`)
//! each had a correct prefix and a fabricated tail, matched no log on any
//! chain, and left the pool monitor subscribed but deaf for the life of the
//! process. Derivation removes that failure mode entirely.

use ethers::types::H256;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_the_keccak_of_their_signatures() {
        assert_eq!(*TOPIC_V2_SYNC, topic_of("Sync(uint112,uint112)"));
        assert_eq!(
            *TOPIC_V2_SWAP,
            topic_of("Swap(address,uint256,uint256,uint256,uint256,address)")
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
