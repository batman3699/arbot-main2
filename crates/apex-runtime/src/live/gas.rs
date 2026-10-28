//! What settling a route uses in gas, and the most it can (Task 8.5 R12).
//!
//! # Gas follows the ticks a trade crosses
//!
//! Until R12 every route was priced at one figure, 534,100 gas — the R6 fork
//! cycle — and every ticket's limit was built from it. A simulation of the
//! deployed executor on Base used **1,246,209** gas for one cycle that crossed
//! 23 initialized ticks: v3-core writes each tick it crosses, and the trades
//! large enough to be worth making, after the swaps that open a gap, are the
//! ones that cross many. At the old limit every one of them runs out of gas.
//!
//! So a settlement's gas is built from its hops: each hop's own work, which
//! depends on its venue (Uniswap's pool directly, Slipstream and PancakeSwap
//! through their routers), plus each initialized tick it crosses and each
//! bitmap word it steps into ([`MultiTickQuote`]). Two figures come out:
//!
//! - **expected** — the measured mean, which the EV prices (§23.1's p50);
//! - **ceiling** — the most the settlement can use, which its gas limit is
//!   built from (p99, then §21.3's headroom).
//!
//! # The ceiling comes from the EVM's rules, not from the sample
//!
//! What one crossing costs depends on the tick's history. Crossing writes the
//! tick's "outside" accumulators, and a slot written from zero costs 22,100 gas
//! where an already non-zero one costs 5,000. A tick above the price that no
//! swap has crossed since it was initialized has every such slot at zero — a
//! trade climbing through a pool nothing has touched in the block meets exactly
//! those, and measured up to ~55k a crossing on PancakeSwap against a mean of
//! ~35k. A sample can miss the worst case; the rules cannot. So a crossing's
//! ceiling is a seasoned crossing plus 17,100 for every slot it can write from
//! zero: Uniswap's two fee accumulators; PancakeSwap's two and its farm pool's
//! reward accumulator; Slipstream's two and its gauge's reward accumulator.
//!
//! [`MultiTickQuote`]: apex_math::cl_swap::MultiTickQuote

use crate::econ::GasEstimate;
use crate::live::inventory::Venue;
use apex_types::cost::GasUsed;

/// One figure for each venue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerVenue {
    pub uniswap_v3: u64,
    pub slipstream: u64,
    /// The second Slipstream factory: its pools cross ticks as the first's do,
    /// but read their fee module more, which the lean executor's hop shows
    /// (R29).
    pub slipstream_v3: u64,
    pub pancake_v3: u64,
    pub aerodrome_v2: u64,
    /// Uniswap v4 (R37), inside PoolManager's unlock.
    pub uniswap_v4: u64,
}

impl PerVenue {
    pub const fn of(&self, venue: Venue) -> u64 {
        match venue {
            Venue::UniswapV3 => self.uniswap_v3,
            Venue::Slipstream => self.slipstream,
            Venue::SlipstreamV3 => self.slipstream_v3,
            Venue::PancakeV3 => self.pancake_v3,
            Venue::AerodromeV2 => self.aerodrome_v2,
            Venue::UniswapV4 => self.uniswap_v4,
        }
    }
}

/// What one hop of a settlement does that costs gas: from its quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HopSteps {
    pub venue: Venue,
    /// The direction the hop moves its pool's price: down when selling token0.
    pub zero_for_one: bool,
    /// Initialized ticks crossed, a zero net included.
    pub crossed: u32,
    /// Bitmap words stepped into past the first.
    pub word_steps: u32,
}

/// The gas model: what each part of a settlement costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettlementGas {
    /// A hop that crosses nothing, by venue — with half the settlement's own
    /// work (the flash loan, the executor's checks) each, because every route
    /// the live frontier holds has two hops.
    pub hop: PerVenue,
    /// Each initialized tick a hop crosses while the price falls: the mean.
    pub crossing_down: PerVenue,
    /// The same while the price rises, which meets more ticks crossed for the
    /// first time, so costs more on average.
    pub crossing_up: PerVenue,
    /// Each bitmap word a hop steps into: the mean. Zero as measured — the fit
    /// could not tell a word step from noise at the one or two a hop takes.
    pub word: u64,
    /// Added to each hop for the ceiling: what the mean leaves out — the first
    /// swap on a pool in its block writes the oracle and accrues its farm, and
    /// hops differ by token.
    pub hop_margin: u64,
    /// The most one crossing can cost, either direction: a seasoned crossing
    /// plus every slot it can write from zero.
    pub crossing_ceiling: PerVenue,
    /// The most one word step can cost: a cold read of the word and a loop of
    /// the swap.
    pub word_ceiling: u64,
}

impl SettlementGas {
    /// What settling hops like these uses: expected, and at most.
    pub fn estimate(&self, hops: &[HopSteps]) -> GasEstimate {
        let (mut expected, mut ceiling) = (0u64, 0u64);
        for h in hops {
            let per_crossing =
                if h.zero_for_one { self.crossing_down.of(h.venue) } else { self.crossing_up.of(h.venue) };
            let (crossed, words) = (u64::from(h.crossed), u64::from(h.word_steps));
            expected = expected
                .saturating_add(self.hop.of(h.venue))
                .saturating_add(per_crossing.saturating_mul(crossed))
                .saturating_add(self.word.saturating_mul(words));
            ceiling = ceiling
                .saturating_add(self.hop.of(h.venue))
                .saturating_add(self.hop_margin)
                .saturating_add(self.crossing_ceiling.of(h.venue).saturating_mul(crossed))
                .saturating_add(self.word_ceiling.saturating_mul(words));
        }
        GasEstimate { expected: GasUsed(expected), ceiling: GasUsed(ceiling.max(expected)) }
    }
}

/// A slot written from zero, 22,100 cold, over one already non-zero, 5,000.
pub const ZERO_SLOT_PREMIUM: u64 = 17_100;

/// **Measured on Base, 2026-10-04** (blocks 52,155,662 and 52,155,833): the
/// deployed executor's `startV2`, simulated by `eth_simulateV1` over every
/// cycle of the live universe after a whale moved one of its pools — all three
/// venues, both directions, 0 to 36 crossings a hop — 1,560 trades, every one
/// returning the book's predicted gross to the wei
/// (`tests/fixtures/settlement_gas.json`; PLAN.md R12). The means are a least
/// squares fit: actual over expected ran 0.86 to 1.32, median 1.00. No trade
/// used more than 94% of its ceiling; the fixed limit before R12, 777,650,
/// would have run 443 of them out of gas.
///
/// A seasoned crossing is the falling-price mean: the ticks below a pool's
/// price have all been crossed or written before, and the rising-price mean is
/// higher by the share it meets that have not.
pub const MEASURED: SettlementGas = SettlementGas {
    hop: PerVenue {
        uniswap_v3: 225_700,
        slipstream: 303_000,
        // The second factory's pools and router are the first's code, so they
        // cost the same through this executor (R22's router simulation).
        slipstream_v3: 303_000,
        pancake_v3: 236_000,
        // R24: PancakeSwap's measured hop (both through GENERIC adapters) plus
        // what Aerodrome's router swap used over PancakeSwap's SmartRouter's,
        // 205,780 − 175,239 gas: `eth_simulateV1` of 0.01 WETH → USDC from an
        // override-funded account, 2026-10-09. Verification check 4 re-measures
        // it through the executor.
        aerodrome_v2: 266_500,
        // R37: v4 runs only through LeanExecutor2, so it has no Phase 5
        // figure; LEAN's stands in.
        uniswap_v4: 287_500,
    },
    // Aerodrome v2 crosses no ticks.
    crossing_down: PerVenue {
        uniswap_v3: 21_300,
        slipstream: 43_400,
        slipstream_v3: 43_400,
        pancake_v3: 32_100,
        aerodrome_v2: 0,
        // R37: v4's swap loop is v3's; its crossings are taken as v3's.
        uniswap_v4: 21_300,
    },
    crossing_up: PerVenue {
        uniswap_v3: 27_200,
        slipstream: 48_300,
        slipstream_v3: 48_300,
        pancake_v3: 36_300,
        aerodrome_v2: 0,
        uniswap_v4: 27_200,
    },
    word: 0,
    // Twice the most any recorded trade needed (11,800).
    hop_margin: 30_000,
    crossing_ceiling: PerVenue {
        uniswap_v3: 21_300 + 2 * ZERO_SLOT_PREMIUM,
        slipstream: 43_400 + 3 * ZERO_SLOT_PREMIUM,
        slipstream_v3: 43_400 + 3 * ZERO_SLOT_PREMIUM,
        pancake_v3: 32_100 + 3 * ZERO_SLOT_PREMIUM,
        // It crosses no ticks.
        aerodrome_v2: 0,
        uniswap_v4: 21_300 + 2 * ZERO_SLOT_PREMIUM,
    },
    // A cold read of the next word (2,100) and a loop of the swap.
    word_ceiling: 10_000,
};

/// **The lean executor's model (R29)**, which the runtime prices with.
///
/// Each hop measured on a fork of Base at block 52,671,989
/// (`test/fork/LeanExecutorFork.t.sol`, `testHopGasByVenue`): 0.1 WETH through
/// each venue's pool inside one call, with the middle token's balance already
/// non-zero, plus half the lean executor's fixed cost — the transaction, Morpho
/// Blue's loan, the settlement: 56,663 gas, which is ticket 664's dusted trade
/// (348,462) less its two hops measured the same way. Rounded up to the next
/// 100. A tick crossed is the pool's own work, so every crossing figure is
/// [`MEASURED`]'s.
///
/// Its check against real trades is still to come: [`MEASURED`] is held to
/// 1,560 recorded settlements of the Phase 5 executor, and nothing has settled
/// through this one yet.
pub const LEAN: SettlementGas = SettlementGas {
    hop: PerVenue {
        uniswap_v3: 116_000,
        slipstream: 232_500,
        slipstream_v3: 256_000,
        pancake_v3: 126_300,
        aerodrome_v2: 168_400,
        // R37, `test/fork/LeanExecutor2Fork.t.sol` at block 53,155,787: AXIS's
        // hooked pool then v3 under PoolManager's loan, 403,479 gas, less v3's
        // 116,000. Provisional: Task 8 of the R37 plan settles it.
        uniswap_v4: 287_500,
    },
    ..MEASURED
};
