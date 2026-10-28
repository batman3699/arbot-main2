//! Task 8.5 R37: a v4 pool priced by the book's ladder is the quoter's, to the
//! unit. `fixtures/v4_axis_quotes.json` is AXIS's hooked v4 pool at Base block
//! 53,155,787, recorded through StateView and V4Quoter
//! (`scripts/data/record_v4_fixture.py`); the snapshot is built by the same
//! `v4_snapshot` a live read uses.

use alloy_primitives::{Address, B256, U256};
use apex_math::cl_swap::quote_exact_input_multi_tick;
use apex_runtime::live::book::{v4_snapshot, PoolSnapshot, V4Read};
use apex_runtime::live::gas::LEAN;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::MAX_TICKS;
use apex_runtime::live::v4::{key_address, pool_id_of, HookAllowlist, V4Key};
use serde_json::Value;
use std::collections::BTreeMap;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/v4_axis_quotes.json")).expect("the fixture parses")
}

fn big(v: &Value) -> U256 {
    v.as_str().expect("a decimal string").parse().expect("a number")
}

/// The fixture's bitmap words and ticks, as a tick source. A word or tick it
/// did not record is unreadable, never zero.
struct Recorded {
    words: BTreeMap<i16, U256>,
    ticks: BTreeMap<i32, (u128, i128)>,
}

#[async_trait::async_trait]
impl apex_venues::cl_ticks::TickDataSource for Recorded {
    async fn tick_words(
        &self,
        _pool: ethers_core::types::Address,
        word_positions: &[i16],
        _block: ethers_core::types::U64,
    ) -> anyhow::Result<Vec<Option<ethers_core::types::U256>>> {
        Ok(word_positions.iter().map(|w| self.words.get(w).map(|v| apex_types::compat::u256_to_ethers(*v))).collect())
    }

    async fn liquidity_net(
        &self,
        _pool: ethers_core::types::Address,
        ticks: &[i32],
        _block: ethers_core::types::U64,
    ) -> anyhow::Result<Vec<Option<i128>>> {
        Ok(ticks.iter().map(|t| Some(self.ticks.get(t).map_or(0, |(_, n)| *n))).collect())
    }

    async fn tick_liquidity(
        &self,
        _pool: ethers_core::types::Address,
        ticks: &[i32],
        _block: ethers_core::types::U64,
    ) -> anyhow::Result<Option<Vec<Option<(u128, i128)>>>> {
        Ok(Some(ticks.iter().map(|t| Some(self.ticks.get(t).copied().unwrap_or((0, 0)))).collect()))
    }
}

async fn snapshot(f: &Value) -> PoolSnapshot {
    let k = &f["key"];
    let addr = |v: &Value| v.as_str().unwrap().parse::<Address>().unwrap();
    let (c0, c1, hooks) = (addr(&k["currency0"]), addr(&k["currency1"]), addr(&k["hooks"]));
    let key = V4Key {
        pool_id: f["pool_id"].as_str().unwrap().parse::<B256>().unwrap(),
        fee: k["fee"].as_u64().unwrap() as u32,
        tick_spacing: k["tick_spacing"].as_i64().unwrap() as i32,
        hooks,
    };
    assert_eq!(pool_id_of(c0, c1, &key), key.pool_id, "the recorded key is the pool's");
    let s0 = &f["slot0"];
    let tick = s0["tick"].as_i64().unwrap();
    let slot0 = big(&s0["sqrt_price_x96"])
        | (U256::from(tick.rem_euclid(1 << 24) as u64) << 160)
        | (U256::from(s0["protocol_fee"].as_u64().unwrap()) << 184)
        | (U256::from(s0["lp_fee"].as_u64().unwrap()) << 208);
    let spec = PoolSpec { pool: key_address(key.pool_id), venue: Venue::UniswapV4, token0: c0, token1: c1, fee_ppm: key.fee, depth_usd: 1e6, v4: Some(key) };
    let read = V4Read {
        slot0,
        liquidity: big(&f["liquidity"]),
        balance0: Some(ethers_core::types::U256::MAX),
        balance1: Some(ethers_core::types::U256::MAX),
        decimals: (18, 6),
        hook_code_hash: B256::repeat_byte(1),
    };
    let source = Recorded {
        words: f["bitmap"].as_object().unwrap().iter().map(|(w, v)| (w.parse().unwrap(), big(v))).collect(),
        ticks: f["ticks"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(t, v)| (t.parse().unwrap(), (big(&v[0]).to::<u128>(), v[1].as_str().unwrap().parse::<i128>().unwrap())))
            .collect(),
    };
    let allow = HookAllowlist { hooks: [(hooks, B256::repeat_byte(1))].into_iter().collect() };
    v4_snapshot(&spec, &read, &source, f["block"].as_u64().unwrap(), &allow).await.expect("the recorded pool loads")
}

#[tokio::test]
async fn every_recorded_quote_is_the_ladder_s_to_the_unit() {
    let f = fixture();
    let p = snapshot(&f).await;
    let mut compared = [0usize; 2];
    for q in f["quotes"].as_array().unwrap() {
        let zero_for_one = q["zero_for_one"].as_bool().unwrap();
        let amount = apex_types::compat::u256_to_ethers(big(&q["amount_in"]));
        let Some(ours) = quote_exact_input_multi_tick(&p.state, &p.ladder, amount, zero_for_one, MAX_TICKS) else { continue };
        if ours.exhausted || ours.amount_in_consumed < amount {
            continue; // past the ladder's reach: refused, as a live quote would be
        }
        assert_eq!(
            apex_types::compat::u256_to_alloy(ours.amount_out),
            big(&q["amount_out"]),
            "zero_for_one {zero_for_one}, {} in",
            q["amount_in"]
        );
        compared[usize::from(zero_for_one)] += 1;
    }
    assert!(compared[0] >= 6 && compared[1] >= 6, "most sizes inside the ladder: {compared:?}");
}

/// LEAN's v4 hop: the fork's v4 hop inside the AXIS trade (swap 141,429,
/// settle 11,012, take 39,014: 191,455) plus half its fixed cost (76,119),
/// less the ticks this swap crossed at v4's figure (the recorded pool, 2,000
/// USDC one-for-zero), rounded up to the next 100.
#[tokio::test]
async fn the_lean_gas_model_knows_v4() {
    let f = fixture();
    let p = snapshot(&f).await;
    let q = quote_exact_input_multi_tick(&p.state, &p.ladder, ethers_core::types::U256::from(2_000_000_000u64), false, MAX_TICKS).unwrap();
    let crossed = u64::from(q.ticks_crossed);
    let measured = 191_455 + 76_119 / 2 - crossed * LEAN.crossing_up.of(Venue::UniswapV4);
    assert_eq!(LEAN.hop.of(Venue::UniswapV4), measured.div_ceil(100) * 100, "crossed {crossed}");
}
